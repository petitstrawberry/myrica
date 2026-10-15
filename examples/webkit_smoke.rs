//! Native integration probe for the persistent Myrica WebKit backend.
//! Run `tests/webkit_server.py` on the host and supply its HTTP base URL.

#[allow(dead_code, unused_imports)]
#[path = "../src/backend/mod.rs"]
mod backend;
#[cfg(feature = "backend-blitz")]
#[path = "../src/network.rs"]
mod network;
#[path = "../src/platform/mod.rs"]
mod platform;
#[cfg(feature = "backend-blitz")]
#[path = "../src/sgfx_scene.rs"]
mod sgfx_scene;

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use backend::{
    BrowserBackend, BrowserInput, BrowserKey, BrowserModifiers, BrowserMouseButton, LoadState,
    WakeCallback, WebKitBackend,
};

#[derive(Clone)]
struct ProbeFrame {
    revision: u64,
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

/// Diagnostic frame copies belong only to this probe. Real browser rendering
/// never performs readback; its GPU capabilities go straight to ScarletUI.
struct ProbeEngine {
    engine: WebKitBackend,
    returned: mpsc::Receiver<u64>,
    cached: Option<ProbeFrame>,
}
impl ProbeEngine {
    fn new(wake: WakeCallback) -> Result<Self, String> {
        let mut engine = WebKitBackend::new(wake).map_err(|error| error.to_string())?;
        let (sender, returned) = mpsc::channel();
        engine.set_frame_releaser(Arc::new(move |token| {
            let _ = sender.send(token);
        }));
        Ok(Self {
            engine,
            returned,
            cached: None,
        })
    }
    fn release_returned(&mut self) {
        while let Ok(token) = self.returned.try_recv() {
            self.engine.release_frame(token);
        }
    }
    fn render(&mut self, width: u32, height: u32, scale: f32) -> Result<ProbeFrame, String> {
        self.release_returned();
        if let Some(frame) = self.engine.render_if_ready(width, height, scale) {
            let result = (|| {
                #[cfg(feature = "backend-webkit-vulkan")]
                let rgba: Arc<[u8]> = self
                    .engine
                    .diagnostic_gpu_frame_rgba(&frame)
                    .map_err(|error| error.to_string())?
                    .into();
                #[cfg(not(feature = "backend-webkit-vulkan"))]
                let rgba = {
                    let texture = frame
                        .draws
                        .first()
                        .and_then(|draw| draw.texture.as_ref())
                        .ok_or("native frame has no real texture")?;
                    match &texture.source {
                        backend::BrowserTextureSource::Rgba8(pixels) => Arc::clone(pixels),
                        #[cfg(target_os = "scarlet")]
                        _ => return Err(String::from("CPU probe received a GPU capability")),
                    }
                };
                Ok::<_, String>(ProbeFrame {
                    revision: frame.revision,
                    width: frame.width,
                    height: frame.height,
                    rgba,
                })
            })();
            // Close the exported capability, then return its storage lease on
            // the genuine WebCore owner even when diagnostic readback failed.
            drop(frame);
            self.release_returned();
            self.cached = Some(result?);
        }
        if let LoadState::Failed(error) = self.engine.snapshot().load_state {
            return Err(format!("native paint failed: {error}"));
        }
        self.cached
            .as_ref()
            .filter(|frame| frame.width == width && frame.height == height)
            .cloned()
            .ok_or_else(|| String::from("native frame has no diagnostic pixels"))
    }
}
impl std::ops::Deref for ProbeEngine {
    type Target = WebKitBackend;
    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}
impl std::ops::DerefMut for ProbeEngine {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.engine
    }
}
impl Drop for ProbeEngine {
    fn drop(&mut self) {
        self.release_returned();
    }
}

enum OwnerMessage {
    InterruptHook(WakeCallback),
    PageProgress,
    EnteringInfiniteScript,
}

struct Owner {
    worker: Option<thread::JoinHandle<Result<(), String>>>,
    hook: Option<WakeCallback>,
}

impl Owner {
    fn join(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            if Instant::now() >= deadline {
                eprintln!(
                    "MYRICA_WEBKIT_SMOKE_FAIL native owner failed to stop after interruption"
                );
                std::process::abort();
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.worker
            .take()
            .unwrap()
            .join()
            .map_err(|_| String::from("native owner panicked"))?
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        if self.worker.is_some() {
            if let Some(hook) = &self.hook {
                hook();
            }
            // Error paths must also finish actual owner teardown before C++
            // global finalizers. A stalled owner is an explicit failed exit.
            if let Err(error) = self.join() {
                eprintln!("MYRICA_WEBKIT_SMOKE_FAIL {error}");
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    // Configure diagnostics before creating either native owner or HTTP worker.
    // SAFETY: The probe is still single-threaded during process initialization.
    unsafe {
        std::env::set_var("MYRICA_WEBKIT_TRACE_SHUTDOWN", "1");
        std::env::set_var("SCARLET_GC_DIAGNOSTICS", "1");
    }
    match run() {
        Ok(()) => {
            println!("MYRICA_WEBKIT_SMOKE_PASS stage=owner-joined");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("MYRICA_WEBKIT_SMOKE_FAIL {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let base = std::env::args()
        .nth(1)
        .ok_or("usage: webkit-smoke http://HOST:PORT | --pages URL...")?;
    let pages: Vec<String> = if base == "--pages" || base == "--video-pages" {
        std::env::args().skip(2).collect()
    } else {
        Vec::new()
    };
    let fixture_only = std::env::args().any(|argument| argument == "--fixture-only");
    let media_only = std::env::args().any(|argument| argument == "--media-only");
    if (base == "--pages" || base == "--video-pages") && pages.is_empty() {
        return Err(String::from("page probes require URLs"));
    }
    let (sender, receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name(String::from("myrica-webkit-smoke"))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || engine_probe(&base, sender, &pages, fixture_only, media_only))
        .map_err(|error| error.to_string())?;
    let mut owner = Owner {
        worker: Some(worker),
        hook: None,
    };
    let hook = match receiver
        .recv_timeout(Duration::from_secs(30))
        .map_err(|error| format!("engine initialization: {error}"))?
    {
        OwnerMessage::InterruptHook(hook) => hook,
        _ => return Err(String::from("engine did not publish its interruption hook")),
    };
    owner.hook = Some(Arc::clone(&hook));
    // Normal stages run on the genuine owner. The final stage executes an
    // actual `while(true){}` script and must be interrupted from this thread.
    loop {
        match receiver.recv_timeout(Duration::from_secs(60)) {
            Ok(OwnerMessage::EnteringInfiniteScript) => {
                thread::sleep(Duration::from_secs(2));
                hook();
                break;
            }
            Ok(OwnerMessage::PageProgress) => continue,
            Ok(_) => return Err(String::from("unexpected owner message")),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(error) => return Err(format!("native owner stalled: {error}")),
        }
    }
    owner.join()?;
    // SAFETY: The concurrent-safe API must now report that genuine owner
    // shutdown revoked its VM binding; a joined thread alone is insufficient.
    let status = unsafe { myrica_webkit_sys::scarlet_webview_request_termination() };
    if status != myrica_webkit_sys::NOT_INITIALIZED {
        return Err(format!(
            "owner returned without native shutdown (status {status})"
        ));
    }
    Ok(())
}

fn engine_probe(
    base: &str,
    sender: mpsc::Sender<OwnerMessage>,
    pages: &[String],
    fixture_only: bool,
    media_only: bool,
) -> Result<(), String> {
    let wake: WakeCallback = Arc::new(|| {});
    let mut engine = ProbeEngine::new(wake)?;
    sender
        .send(OwnerMessage::InterruptHook(
            engine
                .shutdown_callback()
                .ok_or("missing native interruption hook")?,
        ))
        .map_err(|error| error.to_string())?;
    if media_only {
        for path in [
            "normal",
            "normal-main",
            "normal-high",
            "normal-sniff",
            "normal-sniff-reload",
            "mse",
            "mse-big",
        ] {
            sender
                .send(OwnerMessage::PageProgress)
                .map_err(|error| error.to_string())?;
            media_probe(&mut engine, base, path)?;
        }
        drop(engine);
        return Ok(());
    }
    if base == "--video-pages" {
        for url in pages {
            sender
                .send(OwnerMessage::PageProgress)
                .map_err(|error| error.to_string())?;
            video_page_probe(&mut engine, url, &sender)?;
        }
        drop(engine);
        return Ok(());
    }
    if !pages.is_empty() {
        for url in pages {
            // Each live page has its own bounded wait. Let the control thread
            // distinguish several slow pages from a stalled native owner.
            sender
                .send(OwnerMessage::PageProgress)
                .map_err(|error| error.to_string())?;
            engine.render(1280, 720, 1.0)?;
            engine.navigate(url).map_err(|error| error.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                engine.tick();
                let state = engine.snapshot();
                if matches!(state.load_state, LoadState::Failed(_)) {
                    return Err(format!("page failed: {state:?}"));
                }
                if state.load_state == LoadState::Ready && !state.title.is_empty() {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(format!("page stalled: {state:?}"));
                }
                thread::sleep(Duration::from_millis(10));
            }
            sustain_live_page(&mut engine, url)?;
            if url == "https://ichigo.dev/" {
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    engine.tick();
                    let text = engine
                        .text_content("lastfm-placeholder")
                        .map_err(|error| error.to_string())?;
                    if let Some(text) = text.as_ref() {
                        if !text.contains("Loading...") && !text.trim().is_empty() {
                            println!(
                                "MYRICA_WEBKIT_DOM url={url} id=lastfm-placeholder text={text:?}"
                            );
                            break;
                        }
                    }
                    if Instant::now() >= deadline {
                        return Err(format!("Recent Tracks was not replaced: {text:?}"));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            let frame = engine.render(1280, 720, 1.0)?;
            let nonwhite = frame
                .rgba
                .chunks_exact(4)
                .filter(|pixel| pixel[3] != 0 && pixel[..3] != [255, 255, 255])
                .count();
            println!(
                "MYRICA_WEBKIT_PAGE url={url} nonwhite={nonwhite} revision={} state={:?}",
                frame.revision,
                engine.snapshot()
            );
            if nonwhite < 100 {
                return Err(format!("page remains blank: {url}"));
            }
        }
        drop(engine);
        return Ok(());
    }
    engine.render(800, 600, 1.0)?;
    engine
        .navigate(&format!("{base}/redirect"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "main-ready")?;
    if !engine.snapshot().url.ends_with("/main") {
        return Err(format!(
            "native redirect was not followed: {:?}",
            engine.snapshot()
        ));
    }
    let frame = engine.render(800, 600, 1.0)?;
    assert_pixel(&frame, 50, 20, [0x12, 0x34, 0x56, 255])?;
    assert_pixel(&frame, 405, 15, [0xAA, 0xBB, 0xCC, 255])?;
    println!(
        "MYRICA_WEBKIT_SMOKE_PASS stage=live-navigation-css-image frame={}",
        frame.revision
    );

    let revision = frame.revision;
    let idle_paints_before = engine
        .paint_statistics()
        .map_err(|error| error.to_string())?
        .paints;
    for _ in 0..20 {
        engine.tick();
        thread::sleep(Duration::from_millis(17));
    }
    let idle = engine.render(800, 600, 1.0)?;
    if idle.revision != revision {
        // Diagnose a pending first-paint update separately from an idle
        // repaint loop. Preserve the failure: a stable baseline must not be
        // assumed just because native loading and the title completed.
        let changed_bytes = frame
            .rgba
            .iter()
            .zip(idle.rgba.iter())
            .filter(|(before, after)| before != after)
            .count();
        let mut previous = idle.clone();
        let mut followup_paints = 0;
        let mut followup_changed_bytes = 0;
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(17));
            engine.tick();
            let current = engine.render(800, 600, 1.0)?;
            followup_paints += usize::from(current.revision != previous.revision);
            followup_changed_bytes += previous
                .rgba
                .iter()
                .zip(current.rgba.iter())
                .filter(|(before, after)| before != after)
                .count();
            previous = current;
        }
        eprintln!(
            "MYRICA_WEBKIT_IDLE_DIAGNOSTIC first={revision} after={} changed_bytes={changed_bytes} followup_paints={followup_paints} followup_changed_bytes={followup_changed_bytes} last={}",
            idle.revision, previous.revision
        );
        return Err(format!(
            "idle frame changed from {revision} to {}",
            idle.revision
        ));
    }
    let idle_paints_after = engine
        .paint_statistics()
        .map_err(|error| error.to_string())?
        .paints;
    if idle_paints_after != idle_paints_before {
        return Err(format!(
            "unchanged idle pixels were repainted: paints {idle_paints_before} to {idle_paints_after}"
        ));
    }
    println!(
        "MYRICA_WEBKIT_SMOKE_PASS stage=idle-frame revision={revision} paints={idle_paints_after}"
    );

    // Plain pointer movement over an unchanged background must retain pixels.
    engine.handle_input(BrowserInput::PointerMoved { x: 350.0, y: 300.0 });
    engine.tick();
    let pointer_revision = engine.render(800, 600, 1.0)?.revision;
    for index in 0..100 {
        engine.handle_input(BrowserInput::PointerMoved {
            x: 350.0 + (index % 10) as f32,
            y: 300.0,
        });
        engine.tick();
    }
    if engine.render(800, 600, 1.0)?.revision != pointer_revision {
        return Err(String::from("plain pointer motion repainted native pixels"));
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=pointer-move-retains-pixels");

    engine.render(500, 400, 2.0)?;
    wait_title(&mut engine, "width:250")?;
    assert_pixel(
        &engine.render(500, 400, 2.0)?,
        100,
        40,
        [0x65, 0x43, 0x21, 255],
    )?;
    engine.render(800, 600, 1.0)?;
    wait_title(&mut engine, "width:800")?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=resize-retina-media-query");

    click(&mut engine, 40.0, 235.0);
    wait_title(&mut engine, "state:one")?;
    click(&mut engine, 190.0, 235.0);
    wait_title(&mut engine, "state:two")?;
    if !engine.snapshot().can_go_back {
        return Err(String::from("native history cannot go back"));
    }
    engine.go_back();
    wait_title(&mut engine, "state:one")?;
    if !engine.snapshot().can_go_forward {
        return Err(String::from("native history cannot go forward"));
    }
    engine.go_forward();
    wait_title(&mut engine, "state:two")?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=history-state-restoration");

    click(&mut engine, 40.0, 74.0);
    type_character(&mut engine, 'a');
    wait_title(&mut engine, "input:a")?;
    type_character(&mut engine, 'x');
    pump(&mut engine, Duration::from_millis(100));
    if engine.snapshot().title != "input:a" {
        return Err(format!(
            "keydown.preventDefault failed: {:?}",
            engine.snapshot()
        ));
    }
    for character in ['β', 'a'] {
        for pressed in [true, false] {
            engine.handle_input(BrowserInput::Key {
                key: BrowserKey::Character('a'),
                pressed,
                modifiers: BrowserModifiers {
                    control: true,
                    ..Default::default()
                },
            });
        }
        type_character(&mut engine, character);
        wait_title(&mut engine, &format!("input:{character}"))?;
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=prevent-default-text");

    click(&mut engine, 40.0, 140.0);
    key(&mut engine, BrowserKey::Enter, true);
    key(&mut engine, BrowserKey::Enter, false);
    wait_title(&mut engine, "textarea:\\n")?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=enter-keypress-newline");

    click(&mut engine, 40.0, 74.0);
    key(&mut engine, BrowserKey::Enter, true);
    key(&mut engine, BrowserKey::Enter, false);
    wait_title(&mut engine, "posted:a")?;
    engine.reload();
    wait_title(&mut engine, "posted:a")?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=post-navigation-reload");

    engine
        .navigate(&format!("{base}/main"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "main-ready")?;
    engine.handle_input(BrowserInput::Wheel {
        delta_x: 0.0,
        delta_y: -240.0,
        x: 350.0,
        y: 300.0,
    });
    wait_title(&mut engine, "scroll:240")?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=native-scroll");

    engine
        .navigate(&format!("{base}/damage"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "damage-ready")?;
    engine.render(800, 600, 1.0)?;
    let before = engine
        .paint_statistics()
        .map_err(|error| error.to_string())?;
    key(&mut engine, BrowserKey::Character('a'), true);
    key(&mut engine, BrowserKey::Character('a'), false);
    wait_title(&mut engine, "damage-changed")?;
    let frame = engine.render(800, 600, 1.0)?;
    assert_pixel(&frame, 25, 25, [0x65, 0x43, 0x21, 255])?;
    assert_pixel(&frame, 300, 300, [255, 255, 255, 255])?;
    let after = engine
        .paint_statistics()
        .map_err(|error| error.to_string())?;
    let painted = after.painted_pixels - before.painted_pixels;
    #[cfg(not(feature = "backend-webkit-vulkan"))]
    {
        if painted >= 800 * 600 {
            return Err(format!(
                "small DOM change repainted the entire viewport: {painted}"
            ));
        }
        println!("MYRICA_WEBKIT_SMOKE_PASS stage=bounded-damage painted_pixels={painted}");
    }
    #[cfg(feature = "backend-webkit-vulkan")]
    {
        // Current two-image GPU renderer repaints a dirty page once into a
        // free image; it does not copy damaged regions from the leased image.
        if painted == 0 || painted > 800 * 600 {
            return Err(format!("GPU DOM mutation painted invalid work: {painted}"));
        }
        println!("MYRICA_WEBKIT_SMOKE_PASS stage=gpu-damage-single-paint painted_pixels={painted}");
    }

    engine
        .navigate(&format!("{base}/storage"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "storage-blob-iframe-ready")?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=storage-cookie-blob-iframe");
    engine
        .navigate(&format!("{base}/frames"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "frames-ready")?;
    let frame = engine.render(800, 600, 1.0)?;
    assert_pixel(&frame, 20, 100, [0x24, 0x68, 0xac, 255])?;
    assert_pixel(&frame, 300, 100, [0xac, 0x68, 0x24, 255])?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=frameset-child-pixels");

    engine
        .navigate(&format!("{base}/view-transition"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "view-transition-ready")?;
    let before = engine.render(800, 600, 1.0)?;
    assert_pixel(&before, 30, 30, [0x12, 0x34, 0x56, 255])?;
    click(&mut engine, 60.0, 155.0);
    wait_title(&mut engine, "view-transition-done")?;
    let observation = engine
        .text_content("view-transition-state")
        .map_err(|error| error.to_string())?
        .ok_or("view transition observation missing")?;
    let state: serde_json::Value =
        serde_json::from_str(&observation).map_err(|error| error.to_string())?;
    verify_skipped_view_transition(&state)?;
    let after = engine.render(800, 600, 1.0)?;
    assert_pixel(&after, 30, 30, [0x65, 0x43, 0x21, 255])?;
    if after.revision == before.revision {
        return Err(String::from(
            "skipped view transition did not repaint its DOM update",
        ));
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=view-transition-skipped-dom-update state={state}");

    if fixture_only {
        drop(engine);
        println!("MYRICA_WEBKIT_SMOKE_PASS stage=owner-destroyed");
        return Ok(());
    }

    engine
        .navigate(&format!("{base}/hung"))
        .map_err(|error| error.to_string())?;
    sender
        .send(OwnerMessage::EnteringInfiniteScript)
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "hung-entered")?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=infinite-script-interrupted");
    // Drop runs genuine Page destruction and native WebCore owner shutdown.
    drop(engine);
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=owner-destroyed");
    Ok(())
}

fn verify_skipped_view_transition(state: &serde_json::Value) -> Result<(), String> {
    if state["calls"].as_u64() != Some(1)
        || state["callbackAsync"].as_bool() != Some(true)
        || state["ready"].as_str() != Some("rejected")
        || state["readyError"].as_str() != Some("InvalidStateError")
        || state["updateCallbackDone"].as_str() != Some("fulfilled")
        || state["finished"].as_str() != Some("fulfilled")
        || !state
            .as_object()
            .is_some_and(|state| state.get("active") == Some(&serde_json::Value::Null))
    {
        return Err(format!(
            "skipped view transition violated its callback or promise contract: {state}"
        ));
    }
    Ok(())
}

fn sustain_live_page(engine: &mut ProbeEngine, url: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ticks = 0;
    let mut revision_changes = 0;
    let mut previous_revision = None;
    // Ready is only a loading milestone. Continue the real owner event loop
    // and GPU presentation while queued media/loader callbacks can still run.
    while Instant::now() < deadline {
        engine.tick();
        let state = engine.snapshot();
        if matches!(state.load_state, LoadState::Failed(_)) {
            return Err(format!(
                "post-Ready page callback failed ({url}): {state:?}"
            ));
        }
        let frame = engine.render(1280, 720, 1.0)?;
        if previous_revision.is_some_and(|previous| previous != frame.revision) {
            revision_changes += 1;
        }
        previous_revision = Some(frame.revision);
        ticks += 1;
        thread::sleep(Duration::from_millis(17));
    }
    if ticks < 2 {
        return Err(format!(
            "post-Ready page owner did not sustain progress ({url}): ticks={ticks}"
        ));
    }
    println!(
        "MYRICA_WEBKIT_SMOKE_PASS stage=live-page-post-ready url={url} seconds=10 ticks={ticks} revision_changes={revision_changes}"
    );
    Ok(())
}

fn verify_sniff_reload_observation(state: &serde_json::Value) -> Result<(), String> {
    let requests = state["sniffReload"]["requests"].as_array().ok_or_else(|| {
        format!(
            "missing ordinary HTTP sniff/reload observations: {}",
            state["sniffReload"]
        )
    })?;
    let first = requests
        .first()
        .ok_or("sniff/reload resource was not fetched")?;
    if first["method"] != "GET"
        || !first["range"].is_null()
        || first["content_type"] != "video/webm"
    {
        return Err(format!(
            "sniff regression did not begin with a misdeclared full GET: {requests:?}"
        ));
    }
    let sniff = requests
        .iter()
        .position(|request| request["method"] == "GET" && request["range"] == "bytes=0-1445")
        .ok_or_else(|| {
            format!("WebCore did not fetch its genuine MediaResourceSniffer prefix: {requests:?}")
        })?;
    if sniff == 0
        || !requests.iter().skip(sniff + 1).any(|request| {
            request["method"] == "GET"
                && request["range"].is_null()
                && request["content_type"] == "application/octet-stream"
        })
    {
        return Err(format!(
            "sniffer prefix was not followed by a genuine engine reload GET: {requests:?}"
        ));
    }
    Ok(())
}

fn media_state(engine: &mut ProbeEngine) -> Result<serde_json::Value, String> {
    let text = engine
        .text_content("media-state")
        .map_err(|error| error.to_string())?
        .ok_or("missing actual HTMLMediaElement state")?;
    serde_json::from_str(&text).map_err(|error| format!("media state {text:?}: {error}"))
}

fn video_page_probe(
    engine: &mut ProbeEngine,
    url: &str,
    sender: &mpsc::Sender<OwnerMessage>,
) -> Result<(), String> {
    engine.render(1280, 720, 1.0)?;
    engine.navigate(url).map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut progress = Instant::now();
    let mut gesture_sent = false;
    let mut start_time = None;
    let mut first_frame = None;
    let mut initial_presented = 0;
    let mut last_statistics = None;
    loop {
        engine.tick();
        if let LoadState::Failed(error) = engine.snapshot().load_state {
            return Err(format!("video page navigation failed: {error}"));
        }
        // Each real video frame is submitted to WebCore's GPU page paint.
        // Only this diagnostic probe reads it back; browser presentation does not.
        let frame = engine.render(1280, 720, 1.0)?;
        if let Some(statistics) = engine
            .media_statistics(0)
            .map_err(|error| error.to_string())?
        {
            if statistics.width > 0 && statistics.height > 0 && statistics.ready_state >= 2 {
                if start_time.is_none() {
                    println!("MYRICA_WEBKIT_VIDEO_READY url={url} statistics={statistics:?}");
                    start_time = Some(statistics.current_time);
                    initial_presented = statistics.presented_frames;
                    first_frame = Some(frame.clone());
                }
                if statistics.paused != 0 && !gesture_sent {
                    // Deliver genuine native pointer/keyboard input to the page.
                    // DOM, source URLs and the site's player are never replaced.
                    if statistics.rect_width > 0.0 && statistics.rect_height > 0.0 {
                        let x = (statistics.rect_x + statistics.rect_width * 0.5).clamp(1.0, 1278.0)
                            as f32;
                        let y = (statistics.rect_y + statistics.rect_height * 0.5).clamp(1.0, 718.0)
                            as f32;
                        click(engine, x, y);
                    } else {
                        key(engine, BrowserKey::Character('k'), true);
                        key(engine, BrowserKey::Character('k'), false);
                    }
                    gesture_sent = true;
                }
                if statistics.current_time >= start_time.unwrap_or(0.0) + 10.0
                    && statistics.decoded_frames >= 24
                    && statistics.presented_frames >= initial_presented + 8
                {
                    let before = first_frame
                        .as_ref()
                        .ok_or("missing initial real video frame")?;
                    let left = statistics.rect_x.clamp(0.0, 1279.0) as usize;
                    let top = statistics.rect_y.clamp(0.0, 719.0) as usize;
                    let right = (statistics.rect_x + statistics.rect_width)
                        .clamp(left as f64, 1280.0) as usize;
                    let bottom = (statistics.rect_y + statistics.rect_height)
                        .clamp(top as f64, 720.0) as usize;
                    let mut changed = 0;
                    for y in top..bottom {
                        for x in left..right {
                            let offset = (y * 1280 + x) * 4;
                            if before.rgba[offset..offset + 4] != frame.rgba[offset..offset + 4] {
                                changed += 1;
                            }
                        }
                    }
                    if changed < 100 {
                        return Err(format!(
                            "video clock advanced without actual changing GPU pixels: {statistics:?}, changed={changed}"
                        ));
                    }
                    println!(
                        "MYRICA_WEBKIT_SMOKE_PASS stage=actual-video-page url={url} changed_video_pixels={changed} statistics={statistics:?} state={:?}",
                        engine.snapshot()
                    );
                    return Ok(());
                }
            }
            last_statistics = Some(statistics);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "actual page video did not play: {url}, statistics={last_statistics:?}, state={:?}",
                engine.snapshot()
            ));
        }
        if progress.elapsed() >= Duration::from_secs(5) {
            sender
                .send(OwnerMessage::PageProgress)
                .map_err(|error| error.to_string())?;
            println!("MYRICA_WEBKIT_VIDEO_WAIT url={url} statistics={last_statistics:?}");
            progress = Instant::now();
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn verify_media_presentation_history(state: &serde_json::Value, path: &str) -> Result<(), String> {
    let encoding = &state["encoding"];
    let profile = match path {
        "normal-main" => "Main",
        "normal-high" => "High",
        _ => "Constrained Baseline",
    };
    let reordered = matches!(path, "normal-main" | "normal-high");
    if encoding["profile"] != profile
        || encoding["frames"] != 24
        || (encoding["b_frames"].as_u64().unwrap_or(0) > 0) != reordered
        || (encoding["reorder_depth"].as_u64().unwrap_or(0) > 0) != reordered
    {
        return Err(format!(
            "missing actual encoded profile/reorder evidence ({path}): {encoding}"
        ));
    }
    let pts = encoding["pts"]
        .as_array()
        .filter(|values| values.len() == 24)
        .ok_or_else(|| format!("missing actual ffprobe PTS ({path})"))?;
    let history = state["frames"]
        .as_array()
        .filter(|frames| frames.len() >= 3)
        .ok_or_else(|| format!("missing native video frame callback history ({path})"))?;
    let mut previous: Option<(u64, f64, u64)> = None;
    for metadata in history {
        let time = metadata["time"]
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| format!("missing native callback mediaTime ({path}): {metadata}"))?;
        let epoch = metadata["epoch"]
            .as_u64()
            .ok_or_else(|| format!("missing callback seek epoch ({path}): {metadata}"))?;
        let count = metadata["count"]
            .as_u64()
            .ok_or_else(|| format!("missing presented-frame count ({path}): {metadata}"))?;
        if metadata["width"] != 128
            || metadata["height"] != 96
            || !pts
                .iter()
                .any(|pts| pts.as_f64().is_some_and(|pts| (time - pts).abs() < 0.00001))
        {
            return Err(format!(
                "native frame lost original encoded PTS/extent ({path}): {metadata}"
            ));
        }
        if let Some((previous_epoch, previous_time, previous_count)) = previous {
            if epoch < previous_epoch
                || (epoch == previous_epoch && (time <= previous_time || count <= previous_count))
            {
                return Err(format!(
                    "native frames duplicated or presented in decode order ({path}): {previous:?} -> {metadata}"
                ));
            }
        }
        previous = Some((epoch, time, count));
    }
    Ok(())
}

fn media_probe(engine: &mut ProbeEngine, base: &str, path: &str) -> Result<(), String> {
    engine.render(400, 240, 1.0)?;
    engine
        .navigate(&format!("{base}/media/{path}"))
        .map_err(|error| error.to_string())?;
    wait_title(engine, "media-ready")?;
    let state = media_state(engine)?;
    if state["width"] != 128 || state["height"] != 96 || state["ready"].as_u64().unwrap_or(0) < 3 {
        return Err(format!("metadata or ready state failed ({path}): {state}"));
    }
    // Paint the loaded document before targeting its actual Play button.
    // Metadata readiness alone does not flush the embedding's pending layout.
    engine.render(400, 240, 1.0)?;
    click(engine, 60.0, 155.0);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut colours = [false; 3];
    let mut previous_time = 0.0;
    let mut last_revision = 0;
    let mut changed_frames = 0;
    let mut sought = false;
    loop {
        engine.tick();
        let state = media_state(engine)?;
        if state["error"].as_u64().unwrap_or(0) != 0
            || engine.snapshot().title.starts_with("media-error:")
        {
            return Err(format!(
                "media playback failed ({path}): {state}, {:?}",
                engine.snapshot()
            ));
        }
        let time = state["time"].as_f64().unwrap_or(0.0);
        if time + 0.05 < previous_time {
            return Err(format!(
                "media clock went backwards: {previous_time} -> {time}"
            ));
        }
        previous_time = time;
        let frame = engine.render(400, 240, 1.0)?;
        if frame.revision != last_revision {
            changed_frames += 1;
            last_revision = frame.revision;
        }
        let offset = (60 * frame.width as usize + 60) * 4;
        let pixel = &frame.rgba[offset..offset + 4];
        for (channel, colour) in [(0, 0), (2, 1), (1, 2)] {
            if pixel[channel] > 200 && (0..3).filter(|c| *c != channel).all(|c| pixel[c] < 30) {
                colours[colour] = true;
            }
        }
        if !sought && colours[1] && time > 1.1 {
            key(engine, BrowserKey::Character('p'), true);
            key(engine, BrowserKey::Character('p'), false);
            wait_title(engine, "media-paused")?;
            pump(engine, Duration::from_millis(100));
            let paused_time = media_state(engine)?["time"]
                .as_f64()
                .ok_or("missing paused media clock")?;
            pump(engine, Duration::from_millis(250));
            let paused = media_state(engine)?;
            if paused["paused"] != true
                || (paused["time"].as_f64().unwrap_or(-1.0) - paused_time).abs() > 0.03
            {
                return Err(format!(
                    "pause did not stop actual media clock ({path}): {paused_time} -> {paused}"
                ));
            }
            key(engine, BrowserKey::Character('s'), true);
            key(engine, BrowserKey::Character('s'), false);
            let seek_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                engine.tick();
                let seek_state = media_state(engine)?;
                let seek_frame = engine.render(400, 240, 1.0)?;
                let offset = (60 * seek_frame.width as usize + 60) * 4;
                let pixel = &seek_frame.rgba[offset..offset + 4];
                if (seek_state["time"].as_f64().unwrap_or(-1.0) - 0.25).abs() < 0.03
                    && seek_state["paused"] == true
                    && pixel[0] > 200
                    && pixel[1] < 30
                    && pixel[2] < 30
                {
                    break;
                }
                if Instant::now() >= seek_deadline {
                    return Err(format!(
                        "seek failed to decode/present red frame ({path}): state={seek_state} pixel={pixel:?}"
                    ));
                }
                thread::sleep(Duration::from_millis(20));
            }
            key(engine, BrowserKey::Character('p'), true);
            key(engine, BrowserKey::Character('p'), false);
            previous_time = 0.25;
            sought = true;
            println!("MYRICA_WEBKIT_SMOKE_PASS stage=media-{path}-pause-seek-resume");
            continue;
        }
        if state["ended"] == true {
            if !sought || !colours.iter().all(|seen| *seen) || changed_frames < 3 || time < 2.9 {
                return Err(format!(
                    "media ended without actual decoded GPU frames ({path}): colours={colours:?} frames={changed_frames} state={state}"
                ));
            }
            let metadata = &state["metadata"];
            if state["callbacks"].as_u64().unwrap_or(0) < 3
                || state["quality"].as_u64().unwrap_or(0) < 3
                || metadata["count"].as_u64().unwrap_or(0) < 3
                || metadata["width"] != 128
                || metadata["height"] != 96
                || !metadata["time"]
                    .as_f64()
                    .is_some_and(|time| time.is_finite() && time >= 0.0)
            {
                return Err(format!(
                    "media frame callbacks/quality lack actual frame metadata ({path}): {state}"
                ));
            }
            verify_media_presentation_history(&state, path)?;
            if path == "normal-sniff-reload" {
                verify_sniff_reload_observation(&state)?;
                println!(
                    "MYRICA_WEBKIT_SMOKE_PASS stage=media-sniff-engine-reload requests={}",
                    state["sniffReload"]["requests"]
                );
            }
            println!(
                "MYRICA_WEBKIT_SMOKE_PASS stage=media-{path} colours={colours:?} frames={changed_frames} state={state}"
            );
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "media playback stalled ({path}): colours={colours:?}, state={state}"
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn wait_title(engine: &mut WebKitBackend, expected: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        engine.tick();
        let snapshot = engine.snapshot();
        if matches!(snapshot.load_state, LoadState::Failed(_)) {
            return Err(format!("native navigation failed: {snapshot:?}"));
        }
        if snapshot.title == expected && snapshot.load_state == LoadState::Ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("wanted title {expected:?}, got {snapshot:?}"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn pump(engine: &mut WebKitBackend, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        engine.tick();
        thread::sleep(Duration::from_millis(5));
    }
}

fn click(engine: &mut WebKitBackend, x: f32, y: f32) {
    engine.handle_input(BrowserInput::PointerMoved { x, y });
    engine.handle_input(BrowserInput::PointerButton {
        button: BrowserMouseButton::Primary,
        pressed: true,
        x,
        y,
    });
    engine.handle_input(BrowserInput::PointerButton {
        button: BrowserMouseButton::Primary,
        pressed: false,
        x,
        y,
    });
}

fn key(engine: &mut WebKitBackend, key: BrowserKey, pressed: bool) {
    engine.handle_input(BrowserInput::Key {
        key,
        pressed,
        modifiers: BrowserModifiers::default(),
    });
}

fn type_character(engine: &mut WebKitBackend, character: char) {
    key(engine, BrowserKey::Character(character), true);
    engine.handle_input(BrowserInput::Text(character));
    key(engine, BrowserKey::Character(character), false);
}

fn assert_pixel(frame: &ProbeFrame, x: u32, y: u32, expected: [u8; 4]) -> Result<(), String> {
    if x >= frame.width || y >= frame.height {
        return Err(String::from("pixel outside native frame"));
    }
    let offset = (y as usize * frame.width as usize + x as usize) * 4;
    let pixel = frame
        .rgba
        .get(offset..offset + 4)
        .ok_or("pixel outside native frame")?;
    if pixel != expected {
        return Err(format!(
            "pixel ({x},{y}) expected {expected:?}, got {pixel:?}"
        ));
    }
    Ok(())
}
