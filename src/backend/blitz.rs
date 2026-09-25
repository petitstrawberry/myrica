//! Blitz implementation of Myrica's browser-engine boundary.

use std::collections::HashMap;
#[cfg(feature = "javascript")]
use std::collections::HashSet;
use std::sync::Arc;
#[cfg(feature = "javascript")]
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::task::{Context, Wake, Waker};
use std::time::Instant;

use anyrender::{ImageRenderer as _, PaintScene as _};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{Document, DocumentConfig, FontContext, util::Color as BlitzColor};
#[cfg(not(feature = "javascript"))]
use blitz_html::HtmlDocument;
use blitz_html::HtmlProvider;
use blitz_paint::paint_scene;
use blitz_traits::events::{
    BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta, BlitzWheelEvent, KeyState,
    MouseEventButton, MouseEventButtons, Point, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::net::{AbortController, AbortSignal, Request, Url};
use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};
#[cfg(feature = "javascript")]
use blitz_vibey_script::{DefaultScriptFetcher, FetchError, ScriptDocument, ScriptFetcher};
use keyboard_types::{Code, Key, Location, Modifiers};
use peniko::{Fill, kurbo::Rect};

use super::{
    BackendError, BrowserBackend, BrowserInput, BrowserKey, BrowserModifiers, BrowserMouseButton,
    BrowserSnapshot, LoadState, WakeCallback,
};
use crate::network::{FetchResponse, NetworkService};

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
    Navigate(Request),
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
struct PendingScripts {
    generation: u64,
    requested_url: String,
    history_action: HistoryAction,
    response: Option<FetchResponse>,
    remaining: usize,
    sources: HashMap<String, String>,
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
        if self
            .sender
            .send(BackendMessage::Navigate(options.into_request()))
            .is_ok()
        {
            (self.wake)();
        }
    }
}

struct RedrawSink {
    wake: WakeCallback,
}

impl ShellProvider for RedrawSink {
    fn request_redraw(&self) {
        (self.wake)();
    }
}

/// CPU-rendered Blitz backend used for browser bring-up.
pub struct BlitzBackend {
    wake: WakeCallback,
    network: NetworkService,
    message_sender: Sender<BackendMessage>,
    message_receiver: Receiver<BackendMessage>,
    navigation_provider: Arc<dyn NavigationProvider>,
    shell_provider: Arc<dyn ShellProvider>,
    font_context: FontContext,
    document: Option<Box<dyn Document>>,
    waker: Waker,
    renderer: Option<VelloCpuImageRenderer>,
    rgba: Vec<u8>,
    renderer_size: (u32, u32),
    snapshot: BrowserSnapshot,
    history: Vec<String>,
    history_index: Option<usize>,
    load_generation: u64,
    current_abort: Option<AbortController>,
    started_at: Instant,
    mouse_buttons: MouseEventButtons,
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
        let shell_provider: Arc<dyn ShellProvider> = Arc::new(RedrawSink {
            wake: Arc::clone(&wake),
        });

        let mut backend = Self {
            waker: Waker::from(Arc::new(BackendWaker(Arc::clone(&wake)))),
            wake,
            network,
            message_sender,
            message_receiver,
            navigation_provider,
            shell_provider,
            font_context: fonts::load_font_context(),
            document: None,
            renderer: None,
            rgba: Vec::new(),
            renderer_size: (0, 0),
            snapshot: BrowserSnapshot {
                backend_name: BACKEND_NAME,
                url: String::from("about:myrica"),
                title: String::from("Myrica"),
                load_state: LoadState::Idle,
                can_go_back: false,
                can_go_forward: false,
            },
            history: Vec::new(),
            history_index: None,
            load_generation: 0,
            current_abort: None,
            started_at: Instant::now(),
            mouse_buttons: MouseEventButtons::None,
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

        let html = String::from_utf8_lossy(&response.body);
        let probe = ScriptDocument::from_html(
            &html,
            DocumentConfig {
                base_url: Some(response.final_url.clone()),
                ..Default::default()
            },
        );
        let mut seen = HashSet::new();
        let urls: Vec<_> = probe
            .external_script_urls()
            .into_iter()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .filter(|url| seen.insert(url.to_string()))
            .collect();
        if urls.is_empty() {
            return self.apply_root_response(
                generation,
                requested_url,
                history_action,
                Ok(response),
                HashMap::new(),
            );
        }

        let pending = Arc::new(Mutex::new(PendingScripts {
            generation,
            requested_url,
            history_action,
            response: Some(response),
            remaining: urls.len(),
            sources: HashMap::new(),
        }));
        for url in urls {
            let requested_script = url.to_string();
            let mut request = Request::get(url);
            request.signal = self.current_abort.as_ref().map(|c| c.signal.clone());
            let pending = Arc::clone(&pending);
            let sender = self.message_sender.clone();
            let wake = Arc::clone(&self.wake);
            self.network.submit(
                request,
                Box::new(move |result| {
                    let mut batch = pending.lock().expect("script batch lock poisoned");
                    match result {
                        Ok(response) if (200..400).contains(&response.status) => {
                            batch.sources.insert(
                                requested_script,
                                String::from_utf8_lossy(&response.body).into_owned(),
                            );
                        }
                        Ok(response) => eprintln!(
                            "[myrica:javascript] script {} returned HTTP {}",
                            requested_script, response.status
                        ),
                        Err(error) => eprintln!(
                            "[myrica:javascript] failed to load script {}: {}",
                            requested_script, error
                        ),
                    }
                    batch.remaining -= 1;
                    if batch.remaining == 0 {
                        let message = BackendMessage::ScriptsLoaded {
                            generation: batch.generation,
                            requested_url: std::mem::take(&mut batch.requested_url),
                            history_action: batch.history_action,
                            response: batch.response.take().expect("batch response exists"),
                            sources: std::mem::take(&mut batch.sources),
                        };
                        drop(batch);
                        if sender.send(message).is_ok() {
                            wake();
                        }
                    }
                }),
            );
        }
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
                let resolved_url = response.final_url;
                let html = String::from_utf8_lossy(&response.body);
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

    fn render_document(&mut self, buffer: &mut [u8], width: u32, height: u32, scale: f32) {
        let expected_len = width as usize * height as usize * 4;
        if width == 0 || height == 0 || buffer.len() < expected_len {
            return;
        }

        let animation_time = self.animation_time();
        let Some(document) = self.document.as_mut() else {
            fill_bgra(buffer, [242, 240, 237, 255]);
            return;
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

        if self.renderer.is_none() {
            self.renderer = Some(VelloCpuImageRenderer::new(width, height));
            self.renderer_size = (width, height);
        }
        let renderer = self.renderer.as_mut().expect("renderer was initialized");
        if self.renderer_size != (width, height) {
            renderer.resize(width, height);
            self.renderer_size = (width, height);
        }
        renderer.reset();
        self.rgba.resize(expected_len, 0);

        renderer.render(
            |scene| {
                scene.fill(
                    Fill::NonZero,
                    Default::default(),
                    BlitzColor::WHITE,
                    Default::default(),
                    &Rect::new(0.0, 0.0, f64::from(width), f64::from(height)),
                );
                paint_scene(scene, &mut inner, f64::from(scale), width, height, 0, 0);
            },
            &mut self.rgba,
        );

        for (source, destination) in self
            .rgba
            .chunks_exact(4)
            .zip(buffer[..expected_len].chunks_exact_mut(4))
        {
            destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
        }

        if inner.is_animating() {
            (self.wake)();
        }
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
                BackendMessage::Navigate(request) => {
                    self.start_request(request, HistoryAction::Push);
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
            changed |= document.poll(Some(Context::from_waker(&self.waker)));
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
        changed
    }

    fn render(&mut self, buffer: &mut [u8], width: u32, height: u32, scale: f32) {
        self.tick();
        self.render_document(buffer, width, height, scale);
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
            } => Self::dispatch_key(
                self.document
                    .as_mut()
                    .expect("document existence was checked")
                    .as_mut(),
                key,
                pressed,
                modifiers,
                None,
            ),
            BrowserInput::Text(character) => {
                let text = character.to_string();
                Self::dispatch_key(
                    self.document
                        .as_mut()
                        .expect("document existence was checked")
                        .as_mut(),
                    BrowserKey::Character(character),
                    true,
                    BrowserModifiers::default(),
                    Some(text),
                );
                Self::dispatch_key(
                    self.document
                        .as_mut()
                        .expect("document existence was checked")
                        .as_mut(),
                    BrowserKey::Character(character),
                    false,
                    BrowserModifiers::default(),
                    None,
                );
            }
        }
        true
    }

    fn snapshot(&self) -> BrowserSnapshot {
        self.snapshot.clone()
    }
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

fn fill_bgra(buffer: &mut [u8], color: [u8; 4]) {
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.copy_from_slice(&color);
    }
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
    fn welcome_page_renders_an_opaque_non_uniform_frame() {
        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        let mut buffer = vec![0_u8; 320 * 240 * 4];
        backend.render(&mut buffer, 320, 240, 1.0);

        assert!(buffer.chunks_exact(4).all(|pixel| pixel[3] == 255));
        let first = &buffer[..4];
        assert!(buffer.chunks_exact(4).any(|pixel| pixel != first));
    }

    #[test]
    fn data_url_raster_image_is_rendered() {
        const IMAGE_HTML: &str = r#"<!doctype html>
<html><body>
<img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAABUAAAAYCAMAAAAiV0Z6AAAAPFBMVEVLoEN0wU6CzFKCzFKCzFKCzFKCzFJSo0MSczNDmkCCzFJPoUMTczNdr0gmgziCzFITczMTczMTczMTczPh00jOAAAAFHRSTlPF/+bIsms8Ad///hX+//5/tXw7aMEAx10AAACaSURBVHgBbc4HDoRQCATQ33tbvf9dF9QxaCT9UQaltLHOh/golXKhMs5Xqa0xU1lyoa2fXFyQOsDG38qsLy4TaV+sFislovyhPzLJJrBu6eQOtpW0LjbJkzTuTDLRVNKa3uxJI+VdiRqXSeu6GW+Qxi29eLIi8H7EsYrT42BD+mQtNO5JMjRuC4lSY8V4hsLX0egGijvUSEP9AbylEsOkeCgWAAAAAElFTkSuQmCC">
</body></html>"#;

        let mut backend = BlitzBackend::new(Arc::new(|| {})).unwrap();
        backend.document = Some(backend.build_document(IMAGE_HTML, None, None, HashMap::new()));
        let mut buffer = vec![0_u8; 320 * 240 * 4];
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            backend.render(&mut buffer, 320, 240, 1.0);
            let has_green = buffer
                .chunks_exact(4)
                .any(|pixel| pixel[1] > 100 && pixel[1] > pixel[0] && pixel[1] > pixel[2]);
            if has_green {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for raster image resource"
            );
            thread::sleep(Duration::from_millis(10));
        }
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
}
