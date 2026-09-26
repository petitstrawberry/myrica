//! Blitz implementation of Myrica's browser-engine boundary.

use std::collections::HashMap;
#[cfg(feature = "javascript")]
use std::collections::HashSet;
use std::sync::Arc;
#[cfg(feature = "javascript")]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::task::{Context, Wake, Waker};
use std::time::Instant;

use blitz_dom::{Document, DocumentConfig, FontContext};
#[cfg(not(feature = "javascript"))]
use blitz_html::HtmlDocument;
use blitz_html::HtmlProvider;
use blitz_paint::paint_scene;
use blitz_traits::events::{
    BlitzImeEvent, BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta,
    BlitzWheelEvent, KeyState, MouseEventButton, MouseEventButtons, Point, PointerCoords,
    PointerDetails, UiEvent,
};
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::net::{AbortController, AbortSignal, Request, Url};
use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};
#[cfg(feature = "javascript")]
use blitz_vibey_script::{
    DefaultScriptFetcher, FetchError, ScriptDocument, ScriptFetcher, module_specifiers,
};
use keyboard_types::{Code, Key, Location, Modifiers};

use super::{
    BackendError, BrowserBackend, BrowserFrame, BrowserInput, BrowserKey, BrowserModifiers,
    BrowserMouseButton, BrowserSnapshot, BrowserTextInput, LoadState, WakeCallback,
};
use crate::network::{FetchResponse, NetworkService};
use crate::sgfx_scene::SgfxSceneRenderer;

mod fonts;

const BACKEND_NAME: &str = "Blitz";
const DEFAULT_WIDTH: u32 = 960;
const DEFAULT_HEIGHT: u32 = 640;
const WELCOME_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <title>Myrica</title>
    <style>
      html, body { height: 100%; margin: 0; }
      body {
        display: grid;
        place-items: center;
        background: #f7f5f2;
        color: #282522;
        font-family: sans-serif;
      }
      main { width: min(38rem, 82vw); }
      h1 { margin: 0 0 0.4rem; color: #a41e35; font-size: 3rem; }
      p { line-height: 1.6; }
      code { color: #7c1930; }
    </style>
  </head>
  <body>
    <main>
      <h1>Myrica</h1>
      <p>A native browser shell for Scarlet.</p>
      <p>The current engine is <code>Blitz</code>. Servo is next.</p>
    </main>
  </body>
</html>"#;

#[derive(Clone, Copy)]
enum HistoryAction {
    Push,
    Traverse(usize),
    Reload,
}

enum BackendMessage {
    Navigate(NavigationOptions),
    RootLoaded {
        generation: u64,
        requested_url: String,
        history_action: HistoryAction,
        result: Result<FetchResponse, String>,
    },
    #[cfg(feature = "javascript")]
    ScriptsLoaded {
        generation: u64,
        requested_url: String,
        history_action: HistoryAction,
        response: FetchResponse,
        sources: HashMap<String, String>,
    },
}

#[cfg(feature = "javascript")]
struct PrefetchBatch {
    generation: u64,
    requested_url: String,
    history_action: HistoryAction,
    response: Option<FetchResponse>,
    sources: HashMap<String, String>,
    seen: HashSet<String>,
    in_flight: usize,
}

#[cfg(feature = "javascript")]
struct PrefetchedScriptFetcher {
    sources: HashMap<String, String>,
}

#[cfg(feature = "javascript")]
impl ScriptFetcher for PrefetchedScriptFetcher {
    fn fetch(&self, url: &Url) -> Result<String, FetchError> {
        self.sources
            .get(url.as_str())
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| DefaultScriptFetcher.fetch(url))
    }
}

/// Resolve a module specifier against its importing module's URL and keep only
/// network schemes, which the prefetcher can fetch.
#[cfg(feature = "javascript")]
fn resolve_module_url(base: &Url, specifier: &str) -> Option<String> {
    let url = if let Ok(url) = Url::parse(specifier) {
        url
    } else {
        base.join(specifier).ok()?
    };
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// Fetch one wave of script URLs and recurse into the module specifiers found
/// in each response, so the synchronous script fetcher can serve the entire
/// transitive module graph from memory.
#[cfg(feature = "javascript")]
fn submit_prefetch_wave(
    network: NetworkService,
    sender: Sender<BackendMessage>,
    wake: WakeCallback,
    abort: Option<AbortSignal>,
    batch: Arc<Mutex<PrefetchBatch>>,
    urls: Vec<String>,
) {
    for url in urls {
        let requested_script = url.clone();
        let mut request = Request::get(Url::parse(&url).expect("prefetched script URL is valid"));
        request.signal = abort.clone();
        let batch = Arc::clone(&batch);
        let sender = sender.clone();
        let wake = Arc::clone(&wake);
        let next_network = network.clone();
        let next_abort = abort.clone();
        network.submit(
            request,
            Box::new(move |result| {
                let mut next = Vec::new();
                let mut message = None;
                {
                    let mut pending = batch.lock().expect("prefetch batch lock poisoned");
                    match result {
                        Ok(response) if (200..400).contains(&response.status) => {
                            let source = response.text().into_owned();
                            pending
                                .sources
                                .insert(requested_script.clone(), source.clone());
                            let script_url = Url::parse(&requested_script)
                                .expect("prefetched script URL is valid");
                            for specifier in module_specifiers(&source) {
                                if let Some(dependency) =
                                    resolve_module_url(&script_url, &specifier)
                                {
                                    if pending.seen.insert(dependency.clone()) {
                                        pending.in_flight += 1;
                                        next.push(dependency);
                                    }
                                }
                            }
                        }
                        Ok(response) => eprintln!(
                            "[myrica:javascript] script {} returned HTTP {}",
                            requested_script, response.status
                        ),
                        Err(error) => eprintln!(
                            "[myrica:javascript] failed to load script {}: {error}",
                            requested_script
                        ),
                    }
                    pending.in_flight -= 1;
                    if pending.in_flight == 0 {
                        message = Some(BackendMessage::ScriptsLoaded {
                            generation: pending.generation,
                            requested_url: std::mem::take(&mut pending.requested_url),
                            history_action: pending.history_action,
                            response: pending.response.take().expect("batch response exists"),
                            sources: std::mem::take(&mut pending.sources),
                        });
                    }
                }
                if let Some(message) = message {
                    if sender.send(message).is_ok() {
                        wake();
                    }
                } else if !next.is_empty() {
                    submit_prefetch_wave(next_network, sender, wake, next_abort, batch, next);
                }
            }),
        );
    }
}

struct BackendWaker(WakeCallback);

impl Wake for BackendWaker {
    fn wake(self: Arc<Self>) {
        (self.0)();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        (self.0)();
    }
}

struct NavigationSink {
    sender: Sender<BackendMessage>,
    wake: WakeCallback,
}

impl NavigationProvider for NavigationSink {
    fn navigate_to(&self, options: NavigationOptions) {
        if self.sender.send(BackendMessage::Navigate(options)).is_ok() {
            (self.wake)();
        }
    }
}

struct RedrawSink {
    wake: WakeCallback,
    pending: Arc<AtomicBool>,
}

impl ShellProvider for RedrawSink {
    fn request_redraw(&self) {
        self.pending.store(true, Ordering::Release);
        (self.wake)();
    }
}

/// Blitz backend rendered as retained SGFX geometry.
pub struct BlitzBackend {
    wake: WakeCallback,
    network: NetworkService,
    message_sender: Sender<BackendMessage>,
    message_receiver: Receiver<BackendMessage>,
    navigation_provider: Arc<dyn NavigationProvider>,
    shell_provider: Arc<dyn ShellProvider>,
    redraw_pending: Arc<AtomicBool>,
    font_context: FontContext,
    document: Option<Box<dyn Document>>,
    waker: Waker,
    renderer: SgfxSceneRenderer,
    snapshot: BrowserSnapshot,
    history: Vec<String>,
    history_index: Option<usize>,
    load_generation: u64,
    current_abort: Option<AbortController>,
    started_at: Instant,
    mouse_buttons: MouseEventButtons,
    key_modifiers: BrowserModifiers,
}

impl BlitzBackend {
    /// Create a Blitz backend and its shared asynchronous network worker.
    pub fn new(wake: WakeCallback) -> Result<Self, BackendError> {
        let network = NetworkService::new(Arc::clone(&wake))?;
        let (message_sender, message_receiver) = channel();
        let navigation_provider: Arc<dyn NavigationProvider> = Arc::new(NavigationSink {
            sender: message_sender.clone(),
            wake: Arc::clone(&wake),
        });
        let redraw_pending = Arc::new(AtomicBool::new(false));
        let shell_provider: Arc<dyn ShellProvider> = Arc::new(RedrawSink {
            wake: Arc::clone(&wake),
            pending: Arc::clone(&redraw_pending),
        });

        let mut backend = Self {
            waker: Waker::from(Arc::new(BackendWaker(Arc::clone(&wake)))),
            wake,
            network,
            message_sender,
            message_receiver,
            navigation_provider,
            shell_provider,
            redraw_pending,
            font_context: fonts::load_font_context(),
            document: None,
            renderer: SgfxSceneRenderer::new(),
            snapshot: BrowserSnapshot {
                backend_name: BACKEND_NAME,
                url: String::from("about:myrica"),
                title: String::from("Myrica"),
                load_state: LoadState::Idle,
                can_go_back: false,
                can_go_forward: false,
                text_input: None,
            },
            history: Vec::new(),
            history_index: None,
            load_generation: 0,
            current_abort: None,
            started_at: Instant::now(),
            mouse_buttons: MouseEventButtons::None,
            key_modifiers: BrowserModifiers::default(),
        };
        backend.document = Some(backend.build_document(WELCOME_HTML, None, None, HashMap::new()));
        Ok(backend)
    }

    fn build_document(
        &self,
        html: &str,
        base_url: Option<String>,
        abort_signal: Option<AbortSignal>,
        sources: HashMap<String, String>,
    ) -> Box<dyn Document> {
        let config = DocumentConfig {
            viewport: Some(Viewport::new(
                DEFAULT_WIDTH,
                DEFAULT_HEIGHT,
                1.0,
                ColorScheme::Light,
            )),
            base_url,
            net_provider: Some(Arc::new(self.network.clone())),
            navigation_provider: Some(Arc::clone(&self.navigation_provider)),
            shell_provider: Some(Arc::clone(&self.shell_provider)),
            html_parser_provider: Some(Arc::new(HtmlProvider)),
            font_ctx: Some(self.font_context.clone()),
            abort_signal,
            ..Default::default()
        };
        #[cfg(feature = "javascript")]
        {
            let mut document = ScriptDocument::from_html(html, config)
                .with_fetcher(PrefetchedScriptFetcher { sources });
            document.execute_scripts();
            for error in document.take_js_errors() {
                eprintln!("[myrica:javascript] {error}");
            }
            Box::new(document)
        }
        #[cfg(not(feature = "javascript"))]
        {
            let _ = sources;
            Box::new(HtmlDocument::from_html(html, config))
        }
    }

    fn start_request(&mut self, mut request: Request, history_action: HistoryAction) {
        if let Some(controller) = self.current_abort.take() {
            controller.abort();
        }

        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let requested_url = request.url.to_string();
        let controller = AbortController::default();
        request.signal = Some(controller.signal.clone());
        self.current_abort = Some(controller);

        self.snapshot.url = requested_url.clone();
        self.snapshot.load_state = LoadState::Loading;
        self.update_history_flags();
        (self.wake)();

        let sender = self.message_sender.clone();
        let wake = Arc::clone(&self.wake);
        self.network.submit(
            request,
            Box::new(move |result| {
                if sender
                    .send(BackendMessage::RootLoaded {
                        generation,
                        requested_url,
                        history_action,
                        result,
                    })
                    .is_ok()
                {
                    wake();
                }
            }),
        );
    }

    #[cfg(feature = "javascript")]
    fn prefetch_scripts(
        &mut self,
        generation: u64,
        requested_url: String,
        history_action: HistoryAction,
        response: FetchResponse,
    ) -> bool {
        if generation != self.load_generation {
            return false;
        }

        let html = response.text();
        let probe = ScriptDocument::from_html(
            &html,
            DocumentConfig {
                base_url: Some(response.final_url.clone()),
                ..Default::default()
            },
        );
        let mut seen = HashSet::new();
        let base_url = Url::parse(&response.final_url)
            .unwrap_or_else(|_| Url::parse("about:blank").expect("about:blank is a valid URL"));
        let mut initial = Vec::new();
        for url in probe.external_script_urls() {
            if matches!(url.scheme(), "http" | "https") && seen.insert(url.to_string()) {
                initial.push(url.to_string());
            }
        }
        for specifier in probe.inline_module_specifiers() {
            if let Some(url) = resolve_module_url(&base_url, &specifier) {
                if seen.insert(url.clone()) {
                    initial.push(url);
                }
            }
        }
        if initial.is_empty() {
            return self.apply_root_response(
                generation,
                requested_url,
                history_action,
                Ok(response),
                HashMap::new(),
            );
        }

        let batch = Arc::new(Mutex::new(PrefetchBatch {
            generation,
            requested_url,
            history_action,
            response: Some(response),
            sources: HashMap::new(),
            seen,
            in_flight: initial.len(),
        }));
        submit_prefetch_wave(
            self.network.clone(),
            self.message_sender.clone(),
            Arc::clone(&self.wake),
            self.current_abort.as_ref().map(|c| c.signal.clone()),
            batch,
            initial,
        );
        false
    }

    fn apply_root_response(
        &mut self,
        generation: u64,
        requested_url: String,
        history_action: HistoryAction,
        result: Result<FetchResponse, String>,
        sources: HashMap<String, String>,
    ) -> bool {
        if generation != self.load_generation {
            return false;
        }

        match result {
            Ok(response) => {
                let html = response.text();
                let resolved_url = response.final_url.clone();
                let abort_signal = self
                    .current_abort
                    .as_ref()
                    .map(|controller| controller.signal.clone());
                let mut document =
                    self.build_document(&html, Some(resolved_url.clone()), abort_signal, sources);
                document.inner_mut().resolve(self.animation_time());
                let title = document
                    .inner()
                    .find_title_node()
                    .map(|node| node.text_content())
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| resolved_url.clone());

                self.document = Some(document);
                self.commit_history(history_action, resolved_url.clone());
                self.snapshot.url = resolved_url;
                self.snapshot.title = title;
                self.snapshot.load_state = if (200..400).contains(&response.status) {
                    LoadState::Ready
                } else {
                    LoadState::Failed(format!("HTTP {}", response.status))
                };
            }
            Err(error) => {
                self.commit_history(history_action, requested_url.clone());
                self.show_error_page(&requested_url, &error);
            }
        }

        self.update_history_flags();
        true
    }

    fn show_error_page(&mut self, url: &str, error: &str) {
        let html = format!(
            r#"<!doctype html>
<html><head><title>Could not load page</title><style>
html,body{{height:100%;margin:0}}body{{display:grid;place-items:center;background:#f7f5f2;color:#282522;font-family:sans-serif}}
main{{width:min(42rem,82vw)}}h1{{color:#a41e35}}code{{word-break:break-all}}
</style></head><body><main><h1>Could not load page</h1><p><code>{}</code></p><p>{}</p></main></body></html>"#,
            escape_html(url),
            escape_html(error),
        );
        self.document = Some(self.build_document(&html, None, None, HashMap::new()));
        self.snapshot.url = url.to_owned();
        self.snapshot.title = String::from("Could not load page");
        self.snapshot.load_state = LoadState::Failed(error.to_owned());
    }

    fn commit_history(&mut self, action: HistoryAction, resolved_url: String) {
        match action {
            HistoryAction::Push => {
                if let Some(index) = self.history_index {
                    self.history.truncate(index + 1);
                } else {
                    self.history.clear();
                }
                self.history.push(resolved_url);
                self.history_index = Some(self.history.len() - 1);
            }
            HistoryAction::Traverse(index) => {
                if let Some(entry) = self.history.get_mut(index) {
                    *entry = resolved_url;
                    self.history_index = Some(index);
                }
            }
            HistoryAction::Reload => {
                if let Some(index) = self.history_index {
                    if let Some(entry) = self.history.get_mut(index) {
                        *entry = resolved_url;
                    }
                } else {
                    self.history.push(resolved_url);
                    self.history_index = Some(0);
                }
            }
        }
    }

    fn update_history_flags(&mut self) {
        self.snapshot.can_go_back = self.history_index.is_some_and(|index| index > 0);
        self.snapshot.can_go_forward = self
            .history_index
            .is_some_and(|index| index + 1 < self.history.len());
    }

    fn animation_time(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    fn render_document(&mut self, width: u32, height: u32, scale: f32) -> BrowserFrame {
        let animation_time = self.animation_time();
        let Some(document) = self.document.as_mut() else {
            return self.renderer.empty_frame(width, height);
        };

        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let mut inner = document.inner_mut();
        if inner.viewport().window_size != (width, height)
            || (inner.viewport().hidpi_scale - scale).abs() > f32::EPSILON
        {
            inner.set_viewport(Viewport::new(width, height, scale, ColorScheme::Light));
        }
        inner.resolve(animation_time);

        self.renderer.begin_frame(width, height);
        paint_scene(
            &mut self.renderer,
            &mut inner,
            f64::from(scale),
            width,
            height,
            0,
            0,
        );
        let frame = self.renderer.finish_frame();

        if inner.is_animating() {
            self.redraw_pending.store(true, Ordering::Release);
            (self.wake)();
        }
        frame
    }

    fn text_input_state(&self) -> Option<BrowserTextInput> {
        let document = self.document.as_ref()?.inner();
        let node = document.get_node(document.get_focussed_node_id()?)?;
        let element = node.element_data()?;
        let input = element.text_input_data()?;
        let layout = input.editor.try_layout()?;
        let caret = input.editor.cursor_geometry(1.0)?;
        let scale = layout.scale();
        let scroll = document.viewport_scroll();
        let pos = node.absolute_position(-scroll.x as f32, -scroll.y as f32);
        let box_layout = node.final_layout();
        let (scroll_x, scroll_y) = if input.is_multiline {
            (0.0, input.scroll_offset)
        } else {
            (input.scroll_offset, 0.0)
        };
        let selection = input.editor.raw_selection();
        let (surrounding_text, cursor_byte, anchor_byte) = if element
            .attr(blitz_dom::local_name!("type"))
            .is_some_and(|kind| kind.eq_ignore_ascii_case("password"))
        {
            (String::new(), 0, 0)
        } else {
            ime_surrounding_text(
                input.editor.raw_text(),
                selection.focus().index(),
                selection.anchor().index(),
            )
        };
        Some(BrowserTextInput {
            cursor_rect: [
                pos.x + box_layout.content_box_x() + caret.x0 as f32 / scale - scroll_x,
                pos.y + box_layout.content_box_y() + caret.y0 as f32 / scale - scroll_y,
                (caret.width() as f32 / scale).max(1.0),
                (caret.height() as f32 / scale).max(1.0),
            ],
            surrounding_text,
            cursor_byte,
            anchor_byte,
        })
    }

    fn pointer_event(&self, x: f32, y: f32, button: MouseEventButton) -> BlitzPointerEvent {
        let (scroll_x, scroll_y) = self
            .document
            .as_ref()
            .map(|document| {
                let scroll = document.inner().viewport_scroll();
                (scroll.x, scroll.y)
            })
            .unwrap_or((0.0, 0.0));
        BlitzPointerEvent {
            id: BlitzPointerId::Mouse,
            is_primary: true,
            coords: PointerCoords {
                page_x: x + scroll_x as f32,
                page_y: y + scroll_y as f32,
                screen_x: x,
                screen_y: y,
                client_x: x,
                client_y: y,
            },
            button,
            buttons: self.mouse_buttons,
            mods: Modifiers::empty(),
            details: PointerDetails::default(),
            element: Point::default(),
            active_pointers: Default::default(),
        }
    }

    fn dispatch_key(
        document: &mut dyn Document,
        key: BrowserKey,
        pressed: bool,
        modifiers: BrowserModifiers,
        text: Option<String>,
    ) {
        let (key, code) = map_key(key);
        let event = BlitzKeyEvent {
            key,
            code,
            modifiers: map_modifiers(modifiers),
            location: Location::Standard,
            is_auto_repeating: false,
            is_composing: false,
            state: if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            text: text.map(Into::into),
        };
        document.handle_ui_event(if pressed {
            UiEvent::KeyDown(event)
        } else {
            UiEvent::KeyUp(event)
        });
    }
}

impl BrowserBackend for BlitzBackend {
    fn navigate(&mut self, location: &str) -> Result<(), BackendError> {
        if location.trim() == "about:myrica" {
            if let Some(controller) = self.current_abort.take() {
                controller.abort();
            }
            self.load_generation = self.load_generation.wrapping_add(1);
            self.document = Some(self.build_document(WELCOME_HTML, None, None, HashMap::new()));
            self.snapshot.url = String::from("about:myrica");
            self.snapshot.title = String::from("Myrica");
            self.snapshot.load_state = LoadState::Ready;
            (self.wake)();
            return Ok(());
        }

        let url = match normalize_location(location) {
            Ok(url) => url,
            Err(error) => {
                self.show_error_page(location, &error.to_string());
                (self.wake)();
                return Err(error);
            }
        };
        self.start_request(Request::get(url), HistoryAction::Push);
        Ok(())
    }

    fn reload(&mut self) {
        let Ok(url) = Url::parse(&self.snapshot.url) else {
            return;
        };
        if matches!(url.scheme(), "http" | "https") {
            self.start_request(Request::get(url), HistoryAction::Reload);
        }
    }

    fn go_back(&mut self) {
        let Some(index) = self.history_index.and_then(|index| index.checked_sub(1)) else {
            return;
        };
        let Some(url) = self.history.get(index).and_then(|url| Url::parse(url).ok()) else {
            return;
        };
        self.start_request(Request::get(url), HistoryAction::Traverse(index));
    }

    fn go_forward(&mut self) {
        let Some(index) = self.history_index.map(|index| index + 1) else {
            return;
        };
        let Some(url) = self.history.get(index).and_then(|url| Url::parse(url).ok()) else {
            return;
        };
        self.start_request(Request::get(url), HistoryAction::Traverse(index));
    }

    fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.message_receiver.try_recv() {
            match message {
                BackendMessage::Navigate(options) => {
                    if self
                        .document
                        .as_ref()
                        .is_some_and(|document| document.inner().id() != options.source_document)
                    {
                        continue;
                    }
                    let action = if options.replace {
                        HistoryAction::Reload
                    } else {
                        HistoryAction::Push
                    };
                    self.start_request(options.into_request(), action);
                    changed = true;
                }
                BackendMessage::RootLoaded {
                    generation,
                    requested_url,
                    history_action,
                    result,
                } => {
                    #[cfg(feature = "javascript")]
                    {
                        changed |= match result {
                            Ok(response) => self.prefetch_scripts(
                                generation,
                                requested_url,
                                history_action,
                                response,
                            ),
                            Err(error) => self.apply_root_response(
                                generation,
                                requested_url,
                                history_action,
                                Err(error),
                                HashMap::new(),
                            ),
                        };
                        continue;
                    }
                    #[cfg(not(feature = "javascript"))]
                    {
                        changed |= self.apply_root_response(
                            generation,
                            requested_url,
                            history_action,
                            result,
                            HashMap::new(),
                        );
                    }
                }
                #[cfg(feature = "javascript")]
                BackendMessage::ScriptsLoaded {
                    generation,
                    requested_url,
                    history_action,
                    response,
                    sources,
                } => {
                    changed |= self.apply_root_response(
                        generation,
                        requested_url,
                        history_action,
                        Ok(response),
                        sources,
                    );
                }
            }
        }
        if let Some(document) = self.document.as_mut() {
            // ScriptDocument reports whether JS ran, including timers that
            // never touched the page. DOM/resource changes request a redraw
            // through the shell provider instead.
            document.poll(Some(Context::from_waker(&self.waker)));
            let title = document
                .inner()
                .find_title_node()
                .map(|node| node.text_content())
                .filter(|title| !title.trim().is_empty());
            if let Some(title) = title {
                if self.snapshot.title != title {
                    self.snapshot.title = title;
                    changed = true;
                }
            }
            #[cfg(feature = "javascript")]
            if let Some(script) =
                (document.as_mut() as &mut dyn std::any::Any).downcast_mut::<ScriptDocument>()
            {
                for error in script.take_js_errors() {
                    eprintln!("[myrica:javascript] {error}");
                }
            }
        }
        changed | self.redraw_pending.swap(false, Ordering::AcqRel)
    }

    fn render(&mut self, width: u32, height: u32, scale: f32) -> BrowserFrame {
        self.tick();
        self.render_document(width, height, scale)
    }

    fn handle_input(&mut self, input: BrowserInput) -> bool {
        self.tick();
        if self.document.is_none() {
            return false;
        }

        match input {
            BrowserInput::PointerMoved { x, y } => {
                let event = self.pointer_event(x, y, MouseEventButton::Main);
                self.document
                    .as_mut()
                    .expect("document existence was checked")
                    .handle_ui_event(UiEvent::PointerMove(event));
            }
            BrowserInput::PointerExited => {
                self.document
                    .as_mut()
                    .expect("document existence was checked")
                    .inner_mut()
                    .clear_hover();
            }
            BrowserInput::PointerButton {
                button,
                pressed,
                x,
                y,
            } => {
                let button = map_mouse_button(button);
                if pressed {
                    self.mouse_buttons.insert(button.into());
                } else {
                    self.mouse_buttons.remove(button.into());
                }
                let event = self.pointer_event(x, y, button);
                self.document
                    .as_mut()
                    .expect("document existence was checked")
                    .handle_ui_event(if pressed {
                        UiEvent::PointerDown(event)
                    } else {
                        UiEvent::PointerUp(event)
                    });
            }
            BrowserInput::Wheel {
                delta_x,
                delta_y,
                x,
                y,
            } => {
                let pointer = self.pointer_event(x, y, MouseEventButton::Main);
                self.document
                    .as_mut()
                    .expect("document existence was checked")
                    .handle_ui_event(UiEvent::Wheel(BlitzWheelEvent {
                        delta: BlitzWheelDelta::Pixels(delta_x, delta_y),
                        coords: pointer.coords,
                        buttons: self.mouse_buttons,
                        mods: Modifiers::empty(),
                        element: Point::default(),
                    }));
            }
            BrowserInput::Key {
                key,
                pressed,
                modifiers,
            } => {
                self.key_modifiers = modifiers;
                // ScarletUI sends printable text separately from physical keys.
                // Inserting on both Key and Text would duplicate every character.
                if matches!(key, BrowserKey::Character(_) | BrowserKey::Space)
                    && !modifiers.control
                    && !modifiers.super_key
                {
                    return true;
                }
                let document = self
                    .document
                    .as_mut()
                    .expect("document existence was checked");
                // Blitz's macOS editor delegates backspace to Cocoa key bindings.
                #[cfg(target_os = "macos")]
                if key == BrowserKey::Backspace && pressed {
                    let command = if modifiers.super_key {
                        "deleteToBeginningOfLine:"
                    } else if modifiers.alt {
                        "deleteWordBackward:"
                    } else {
                        "deleteBackward:"
                    };
                    document.handle_ui_event(UiEvent::AppleStandardKeybinding(command.into()));
                    return true;
                }
                Self::dispatch_key(document.as_mut(), key, pressed, modifiers, None);
            }
            BrowserInput::Text(character) => {
                if self.key_modifiers.control
                    || self.key_modifiers.super_key
                    || character.is_control()
                {
                    return true;
                }
                let text = character.to_string();
                Self::dispatch_key(
                    self.document
                        .as_mut()
                        .expect("document existence was checked")
                        .as_mut(),
                    BrowserKey::Character(character),
                    true,
                    self.key_modifiers,
                    Some(text),
                );
                Self::dispatch_key(
                    self.document
                        .as_mut()
                        .expect("document existence was checked")
                        .as_mut(),
                    BrowserKey::Character(character),
                    false,
                    self.key_modifiers,
                    None,
                );
            }
            BrowserInput::ImePreedit {
                text,
                cursor,
                anchor,
            } => {
                self.document
                    .as_mut()
                    .unwrap()
                    .handle_ui_event(UiEvent::Ime(BlitzImeEvent::Preedit(
                        text,
                        Some((cursor, anchor)),
                    )));
            }
            BrowserInput::ImeCommit(text) => {
                let document = self.document.as_mut().unwrap();
                // ScarletUI folds the empty preedit preceding a commit into the
                // commit itself. Blitz expects the composition to be cleared first.
                document.handle_ui_event(UiEvent::Ime(BlitzImeEvent::Preedit(String::new(), None)));
                document.handle_ui_event(UiEvent::Ime(BlitzImeEvent::Commit(text)));
            }
            BrowserInput::FocusLost => {
                self.key_modifiers = BrowserModifiers::default();
                self.document
                    .as_mut()
                    .unwrap()
                    .handle_ui_event(UiEvent::Ime(BlitzImeEvent::Disabled));
            }
        }
        true
    }

    fn snapshot(&self) -> BrowserSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.text_input = self.text_input_state();
        snapshot
    }
}

fn ime_surrounding_text(text: &str, cursor: usize, anchor: usize) -> (String, u32, u32) {
    // Keep chrome/IME updates bounded even for a large textarea. Positions in
    // Parley are UTF-8 boundaries; trim the surrounding window at boundaries too.
    let mut start = cursor.saturating_sub(2048);
    let mut end = cursor.saturating_add(2048).min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (
        text[start..end].to_owned(),
        (cursor - start) as u32,
        (anchor.clamp(start, end) - start) as u32,
    )
}

fn normalize_location(location: &str) -> Result<Url, BackendError> {
    let location = location.trim();
    if location.is_empty() {
        return Err(BackendError::new("the address is empty"));
    }

    let candidate = if location.contains("://") {
        location.to_owned()
    } else if location.starts_with("localhost")
        || location.starts_with("127.")
        || location.starts_with("0.0.0.0")
        || location.starts_with('[')
    {
        format!("http://{location}")
    } else {
        format!("https://{location}")
    };

    let url = Url::parse(&candidate)
        .map_err(|error| BackendError::new(format!("invalid address: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BackendError::new(format!(
            "unsupported URL scheme: {}",
            url.scheme()
        )));
    }
    Ok(url)
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn map_mouse_button(button: BrowserMouseButton) -> MouseEventButton {
    match button {
        BrowserMouseButton::Primary => MouseEventButton::Main,
        BrowserMouseButton::Auxiliary => MouseEventButton::Auxiliary,
        BrowserMouseButton::Secondary => MouseEventButton::Secondary,
    }
}

fn map_modifiers(modifiers: BrowserModifiers) -> Modifiers {
    let mut result = Modifiers::empty();
    if modifiers.shift {
        result.insert(Modifiers::SHIFT);
    }
    if modifiers.control {
        result.insert(Modifiers::CONTROL);
    }
    if modifiers.alt {
        result.insert(Modifiers::ALT);
    }
    if modifiers.super_key {
        result.insert(Modifiers::SUPER);
    }
    result
}

fn map_key(key: BrowserKey) -> (Key, Code) {
    match key {
        BrowserKey::Escape => (Key::Escape, Code::Escape),
        BrowserKey::Enter => (Key::Enter, Code::Enter),
        BrowserKey::Tab => (Key::Tab, Code::Tab),
        BrowserKey::Backspace => (Key::Backspace, Code::Backspace),
        BrowserKey::Space => (Key::Character(String::from(" ")), Code::Space),
        BrowserKey::Left => (Key::ArrowLeft, Code::ArrowLeft),
        BrowserKey::Right => (Key::ArrowRight, Code::ArrowRight),
        BrowserKey::Up => (Key::ArrowUp, Code::ArrowUp),
        BrowserKey::Down => (Key::ArrowDown, Code::ArrowDown),
        BrowserKey::Home => (Key::Home, Code::Home),
        BrowserKey::End => (Key::End, Code::End),
        BrowserKey::PageUp => (Key::PageUp, Code::PageUp),
        BrowserKey::PageDown => (Key::PageDown, Code::PageDown),
        BrowserKey::Insert => (Key::Insert, Code::Insert),
        BrowserKey::Delete => (Key::Delete, Code::Delete),
        BrowserKey::Character(character) => {
            (Key::Character(character.to_string()), Code::Unidentified)
        }
        BrowserKey::Unknown => (Key::Unidentified, Code::Unidentified),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{BlitzBackend, BrowserBackend as _, normalize_location};

    #[test]
    fn defaults_public_hosts_to_https() {
        assert_eq!(
            normalize_location("example.com/path").unwrap().as_str(),
            "https://example.com/path"
        );
    }

    #[test]
    fn defaults_loopback_hosts_to_http() {
        assert_eq!(
            normalize_location("127.0.0.1:8080/demo").unwrap().as_str(),
            "http://127.0.0.1:8080/demo"
        );
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(normalize_location("file:///tmp/index.html").is_err());
    }

    #[test]
    fn ime_surrounding_text_is_bounded_at_utf8_boundaries() {
        let text = "日本語".repeat(2000);
        let (surrounding, cursor, anchor) = super::ime_surrounding_text(&text, 9000, 0);
        assert!(surrounding.len() <= 4096);
        assert!(surrounding.is_char_boundary(cursor as usize));
        assert_eq!(anchor, 0);
        assert_eq!(&surrounding[cursor as usize..cursor as usize + 9], "日本語");
    }

    #[test]
    fn welcome_page_builds_sgfx_draws() {
        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        let frame = backend.render(320, 240, 1.0);

        assert!(frame.draw_count() > 0);
    }

    #[test]
    fn data_url_raster_image_is_rendered() {
        const IMAGE_HTML: &str = r#"<!doctype html>
<html><body>
<img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAABUAAAAYCAMAAAAiV0Z6AAAAPFBMVEVLoEN0wU6CzFKCzFKCzFKCzFKCzFJSo0MSczNDmkCCzFJPoUMTczNdr0gmgziCzFITczMTczMTczMTczPh00jOAAAAFHRSTlPF/+bIsms8Ad///hX+//5/tXw7aMEAx10AAACaSURBVHgBbc4HDoRQCATQ33tbvf9dF9QxaCT9UQaltLHOh/golXKhMs5Xqa0xU1lyoa2fXFyQOsDG38qsLy4TaV+sFislovyhPzLJJrBu6eQOtpW0LjbJkzTuTDLRVNKa3uxJI+VdiRqXSeu6GW+Qxi29eLIi8H7EsYrT42BD+mQtNO5JMjRuC4lSY8V4hsLX0egGijvUSEP9AbylEsOkeCgWAAAAAElFTkSuQmCC">
</body></html>"#;

        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.document = Some(backend.build_document(IMAGE_HTML, None, None, HashMap::new()));
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            backend.render(320, 240, 1.0);
            if backend.renderer.texture_count() > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for raster image resource"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn input_backend() -> BlitzBackend {
        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.document = Some(backend.build_document(
            r#"<!doctype html><style>body{margin:0}input,textarea{display:block;width:250px;height:50px;font:20px sans-serif}</style>
            <input id="single"><textarea id="multi"></textarea>"#,
            None, None, HashMap::new(),
        ));
        backend.render(400, 300, 1.0);
        backend
    }

    fn click_input(backend: &mut BlitzBackend, selector: &str) {
        let (x, y) = {
            let document = backend.document.as_ref().unwrap().inner();
            let node = document.query_selector(selector).unwrap().unwrap();
            let rect = document.get_client_bounding_rect(node).unwrap();
            ((rect.x + 8.0) as f32, (rect.y + rect.height / 2.0) as f32)
        };
        backend.handle_input(super::BrowserInput::PointerMoved { x, y });
        for pressed in [true, false] {
            backend.handle_input(super::BrowserInput::PointerButton {
                button: super::BrowserMouseButton::Primary,
                pressed,
                x,
                y,
            });
        }
    }

    fn press_key(backend: &mut BlitzBackend, key: super::BrowserKey) {
        for pressed in [true, false] {
            backend.handle_input(super::BrowserInput::Key {
                key,
                pressed,
                modifiers: super::BrowserModifiers::default(),
            });
        }
    }

    #[test]
    fn focused_input_accepts_text_once_and_supports_editing() {
        use super::{BrowserInput, BrowserKey};
        let mut backend = input_backend();
        click_input(&mut backend, "#single");
        assert!(backend.snapshot().text_input.is_some());
        // Same physical-key + character sequence produced by ScarletUI.
        for c in ['a', 'b'] {
            press_key(&mut backend, BrowserKey::Character(c));
            backend.handle_input(BrowserInput::Text(c));
        }
        assert_eq!(
            backend.snapshot().text_input.unwrap().surrounding_text,
            "ab"
        );
        press_key(&mut backend, BrowserKey::Left);
        press_key(&mut backend, BrowserKey::Backspace);
        assert_eq!(backend.snapshot().text_input.unwrap().surrounding_text, "b");
        press_key(&mut backend, BrowserKey::Delete);
        assert_eq!(backend.snapshot().text_input.unwrap().surrounding_text, "");
    }

    #[test]
    fn space_is_inserted_once_in_single_and_multiline_inputs() {
        use super::{BrowserInput, BrowserKey, BrowserModifiers};
        let mut backend = input_backend();
        for selector in ["#single", "#multi"] {
            click_input(&mut backend, selector);
            backend.handle_input(BrowserInput::Text('a'));
            for _ in 0..2 {
                backend.handle_input(BrowserInput::Key {
                    key: BrowserKey::Space,
                    pressed: true,
                    modifiers: BrowserModifiers::default(),
                });
                backend.handle_input(BrowserInput::Text(' '));
                backend.handle_input(BrowserInput::Key {
                    key: BrowserKey::Space,
                    pressed: false,
                    modifiers: BrowserModifiers::default(),
                });
            }
            backend.handle_input(BrowserInput::Text('b'));
            assert_eq!(
                backend.snapshot().text_input.unwrap().surrounding_text,
                "a  b"
            );
        }
    }

    #[test]
    fn textarea_accepts_newlines_and_japanese_ime_composition() {
        use super::{BrowserInput, BrowserKey};
        let mut backend = input_backend();
        click_input(&mut backend, "#multi");
        backend.handle_input(BrowserInput::Text('A'));
        press_key(&mut backend, BrowserKey::Enter);
        backend.handle_input(BrowserInput::ImePreedit {
            text: "にほん".into(),
            cursor: 9,
            anchor: 9,
        });
        assert_eq!(
            backend.snapshot().text_input.unwrap().surrounding_text,
            "A\nにほん"
        );
        backend.handle_input(BrowserInput::ImeCommit("日本語".into()));
        backend.render(400, 300, 1.0);
        let state = backend.snapshot().text_input.unwrap();
        assert_eq!(state.surrounding_text, "A\n日本語");
        assert_eq!(state.cursor_byte, 11);
        assert!(state.cursor_rect[3] > 0.0);
        backend.handle_input(BrowserInput::ImePreedit {
            text: "あ".into(),
            cursor: 3,
            anchor: 3,
        });
        backend.handle_input(BrowserInput::FocusLost);
        assert_eq!(
            backend.snapshot().text_input.unwrap().surrounding_text,
            "A\n日本語"
        );
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn inline_script_changes_the_document() {
        let backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        let document = backend.build_document(
            r#"<html><head><title>before</title></head><body>
            <script>document.querySelector('title').textContent = 'after';</script>
            </body></html>"#,
            None,
            None,
            HashMap::new(),
        );
        assert_eq!(
            document.inner().find_title_node().unwrap().text_content(),
            "after"
        );
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn javascript_timer_updates_the_browser_title() {
        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.document = Some(backend.build_document(
            r#"<html><head><title>waiting</title></head><body>
            <script>
              setTimeout(() => { document.querySelector('title').textContent = 'timer-ready'; }, 10);
            </script></body></html>"#,
            None,
            None,
            HashMap::new(),
        ));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            backend.tick();
            if backend.snapshot().title == "timer-ready" {
                break;
            }
            assert!(Instant::now() < deadline, "JavaScript timer never ran");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn timer_without_dom_changes_does_not_request_a_frame() {
        use blitz_vibey_script::ScriptDocument;
        use std::sync::atomic::Ordering;

        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.document = Some(backend.build_document(
            "<title>timers</title><p id='value'>before</p>",
            None,
            None,
            HashMap::new(),
        ));
        backend.render(400, 300, 1.0);
        backend.tick();
        let script = (backend.document.as_mut().unwrap().as_mut() as &mut dyn std::any::Any)
            .downcast_mut::<ScriptDocument>()
            .unwrap();
        script.eval("setTimeout(() => { globalThis.timerRan = true; }, 0);");
        backend.redraw_pending.store(false, Ordering::Release);
        thread::sleep(Duration::from_millis(5));
        assert!(
            !backend.tick(),
            "a timer with no DOM changes repainted the page"
        );
        let script = (backend.document.as_mut().unwrap().as_mut() as &mut dyn std::any::Any)
            .downcast_mut::<ScriptDocument>()
            .unwrap();
        script.eval("if (globalThis.timerRan !== true) throw Error('timer did not run');");
        assert!(script.take_js_errors().is_empty());
        script.eval(
            "setTimeout(() => { document.getElementById('value').textContent = 'after'; }, 0);",
        );
        backend.redraw_pending.store(false, Ordering::Release);
        thread::sleep(Duration::from_millis(5));
        assert!(backend.tick(), "a timer that changes the DOM must repaint");
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn external_script_is_prefetched_before_execution() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        use super::LoadState;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut served = 0;
            while served < 2 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0_u8; 2048];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let body = if request.starts_with("GET /change.js ") {
                    "document.querySelector('title').textContent = 'external-ready';"
                } else {
                    r#"<html><head><title>waiting</title></head><body>
                    <script src="/change.js"></script></body></html>"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                served += 1;
            }
            assert_eq!(served, 2, "browser did not request both page and script");
        });

        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.navigate(&format!("http://{address}/page")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            backend.tick();
            let snapshot = backend.snapshot();
            if matches!(snapshot.load_state, LoadState::Ready) {
                assert_eq!(snapshot.title, "external-ready");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out loading JavaScript page"
            );
            thread::sleep(Duration::from_millis(10));
        }
        server.join().unwrap();
    }

    #[test]
    fn shift_jis_page_uses_the_http_charset() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                assert!(Instant::now() < deadline, "no page request");
                thread::sleep(Duration::from_millis(10));
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 2048];
            let received = stream.read(&mut request).unwrap();
            assert!(received > 0, "empty page request");
            let html = "<!doctype html><title>日本語のページ</title><p id='result'>検索結果</p><script>document.querySelector('title').textContent='日本語の検索';</script>";
            let (body, _, _) = encoding_rs::SHIFT_JIS.encode(html);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=Shift_JIS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        });
        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.navigate(&format!("http://{address}/")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            backend.tick();
            if matches!(backend.snapshot().load_state, super::LoadState::Ready) {
                break;
            }
            assert!(Instant::now() < deadline, "page never finished loading");
            thread::sleep(Duration::from_millis(10));
        }
        let expected_title = if cfg!(feature = "javascript") {
            "日本語の検索"
        } else {
            "日本語のページ"
        };
        assert_eq!(backend.snapshot().title, expected_title);
        let document = backend.document.as_ref().unwrap().inner();
        let result = document.query_selector("#result").unwrap().unwrap();
        assert_eq!(
            document.get_node(result).unwrap().text_content(),
            "検索結果"
        );
        server.join().unwrap();
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn module_imports_are_prefetched_transitively() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        use super::LoadState;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut requested = Vec::new();
            let mut saw_lib = false;
            while Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    if requested.len() == 3 && saw_lib {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut buffer = [0_u8; 2048];
                let length = stream.read(&mut buffer).unwrap();
                let line = String::from_utf8_lossy(&buffer[..length]);
                let path = line.split_whitespace().nth(1).unwrap_or_default();
                requested.push(path.to_string());
                let body = match path {
                    "/main.mjs" => r#"import { setTitle } from "./lib.mjs"; setTitle();"#,
                    "/lib.mjs" => {
                        saw_lib = true;
                        r#"export function setTitle() {
                            document.querySelector("title").textContent = "module-ready";
                        }"#
                    }
                    _ => {
                        r#"<html><head><title>waiting</title></head><body>
                    <script type="module" src="/main.mjs"></script></body></html>"#
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
            assert!(
                saw_lib,
                "browser did not request the transitive module import; requests: {requested:?}"
            );
        });

        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.navigate(&format!("http://{address}/page")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            backend.tick();
            let snapshot = backend.snapshot();
            if matches!(snapshot.load_state, LoadState::Ready) {
                assert_eq!(snapshot.title, "module-ready");
                break;
            }
            assert!(Instant::now() < deadline, "timed out loading module page");
            thread::sleep(Duration::from_millis(10));
        }
        server.join().unwrap();
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn script_cookie_and_replace_continue_navigation_with_http_cookies() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut served = 0;
            while served < 3 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|s| s == b"\r\n\r\n") {
                    let mut buffer = [0; 2048];
                    let n = stream.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buffer[..n]);
                }
                let request = String::from_utf8(request).unwrap();
                let (body, headers) = match served {
                    0 => (
                        r#"<title>checking</title><script>
                        if (document.cookie === 'visible=http') {
                            document.cookie = 'script=ready; Path=/';
                            location.replace('/verified');
                        }
                        </script>"#,
                        "Set-Cookie: secret=http; HttpOnly; Path=/\r\nSet-Cookie: visible=http; Path=/\r\n",
                    ),
                    1 => {
                        assert!(request.starts_with("GET /verified "));
                        assert!(request.contains("secret=http"));
                        assert!(request.contains("script=ready"));
                        (
                            r#"<title>fetching</title><script>
                            if (document.cookie.includes('script=ready') && !document.cookie.includes('secret=')) {
                                fetch('/data').then(r => r.text()).then(t => document.querySelector('title').textContent = t);
                            }
                            </script>"#,
                            "",
                        )
                    }
                    _ => {
                        assert!(request.starts_with("GET /data "));
                        assert!(request.contains("script=ready"));
                        ("verified", "")
                    }
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}", body.len()).unwrap();
                served += 1;
            }
            assert_eq!(served, 3);
        });
        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend
            .navigate(&format!("http://{address}/check"))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            backend.tick();
            if backend.snapshot().title == "verified" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "script navigation/cookie flow stalled: {:?}",
                backend.snapshot()
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(backend.history, [format!("http://{address}/verified")]);
        server.join().unwrap();
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn javascript_fetch_resolves_with_the_http_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        use super::LoadState;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut served = 0;
            while served < 2 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0_u8; 2048];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let body = if request.starts_with("GET /data.json ") {
                    r#"{"name":"fetched-name"}"#
                } else {
                    r#"<html><head><title>waiting</title></head><body>
                    <script>
                      fetch("/data.json").then((response) => response.json()).then((data) => {
                        document.querySelector("title").textContent = data.name;
                      });
                    </script></body></html>"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                served += 1;
            }
            assert_eq!(served, 2, "browser did not request both page and data");
        });

        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.navigate(&format!("http://{address}/page")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            backend.tick();
            let snapshot = backend.snapshot();
            if matches!(snapshot.load_state, LoadState::Ready) && snapshot.title == "fetched-name" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for fetch promise"
            );
            thread::sleep(Duration::from_millis(10));
        }
        server.join().unwrap();
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn javascript_web_storage_is_available() {
        let backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        let document = backend.build_document(
            r#"<html><head><title>before</title></head><body>
            <script>
              localStorage.setItem('theme', 'dark');
              document.querySelector('title').textContent = localStorage.getItem('theme');
            </script>
            </body></html>"#,
            None,
            None,
            HashMap::new(),
        );
        assert_eq!(
            document.inner().find_title_node().unwrap().text_content(),
            "dark"
        );
    }
}
