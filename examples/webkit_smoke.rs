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
    fn cached_revision(&self) -> u64 {
        self.cached.as_ref().map_or(0, |frame| frame.revision)
    }
    fn release_returned(&mut self) {
        while let Ok(token) = self.returned.try_recv() {
            self.engine.release_frame(token);
        }
    }
    fn render(&mut self, width: u32, height: u32, scale: f32) -> Result<ProbeFrame, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
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
            if let Some(frame) = self
                .cached
                .as_ref()
                .filter(|frame| frame.width == width && frame.height == height)
            {
                return Ok(frame.clone());
            }
            // Only initial/resize acquisition waits for actual GPU completion.
            // Existing matching cached frames preserve the normal nonblocking poll.
            if Instant::now() >= deadline {
                return Err(String::from(
                    "native GPU frame completion timed out without diagnostic pixels",
                ));
            }
            self.engine.tick();
            thread::sleep(Duration::from_millis(1));
        }
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
        if std::env::args().any(|argument| argument == "--video-pages") {
            std::env::set_var("SCARLET_WEBVIEW_PROFILE", "1");
        }
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
    let compositor_only = std::env::args().any(|argument| argument == "--compositor-only");
    let painting_workers_only =
        std::env::args().any(|argument| argument == "--painting-workers-only");
    if (base == "--pages" || base == "--video-pages") && pages.is_empty() {
        return Err(String::from("page probes require URLs"));
    }
    let (sender, receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name(String::from("myrica-webkit-smoke"))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let result = engine_probe(
                &base,
                sender,
                &pages,
                fixture_only,
                media_only,
                compositor_only,
                painting_workers_only,
            );
            eprintln!("MYRICA_WEBKIT_SMOKE_TRACE stage=owner-body-returned");
            result
        })
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
    eprintln!("MYRICA_WEBKIT_SMOKE_TRACE stage=before-owner-join");
    owner.join()?;
    eprintln!("MYRICA_WEBKIT_SMOKE_TRACE stage=after-owner-join");
    // SAFETY: The concurrent-safe API must now report that genuine owner
    // shutdown revoked its VM binding; a joined thread alone is insufficient.
    eprintln!("MYRICA_WEBKIT_SMOKE_TRACE stage=before-revocation-check");
    let status = unsafe { myrica_webkit_sys::scarlet_webview_request_termination() };
    eprintln!("MYRICA_WEBKIT_SMOKE_TRACE stage=after-revocation-check status={status}");
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
    compositor_only: bool,
    painting_workers_only: bool,
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
    if painting_workers_only {
        painting_workers_probe(&mut engine, base)?;
        drop(engine);
        return Ok(());
    }
    if compositor_only {
        #[cfg(feature = "backend-webkit-vulkan")]
        {
            compositor_video_probe(&mut engine, base)?;
            drop(engine);
            return Ok(());
        }
        #[cfg(not(feature = "backend-webkit-vulkan"))]
        return Err("--compositor-only requires backend-webkit-vulkan".into());
    }
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
        #[cfg(feature = "backend-webkit-vulkan")]
        compositor_video_probe(&mut engine, base)?;
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
            let previous_revision = engine.cached_revision();
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
            let frame =
                await_rendered_frame(&mut engine, (1280, 720, 1.0), previous_revision, |frame| {
                    let nonwhite = frame
                        .rgba
                        .chunks_exact(4)
                        .filter(|pixel| pixel[3] != 0 && pixel[..3] != [255, 255, 255])
                        .count();
                    if nonwhite >= 100 {
                        Ok(())
                    } else {
                        Err(format!("page remains blank: {url}"))
                    }
                })?;
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
    sender
        .send(OwnerMessage::PageProgress)
        .map_err(|error| error.to_string())?;
    image_pipeline_probe(&mut engine, base)?;
    sender
        .send(OwnerMessage::PageProgress)
        .map_err(|error| error.to_string())?;
    let previous_revision = engine.cached_revision();
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
    let frame = await_rendered_frame(&mut engine, (800, 600, 1.0), previous_revision, |frame| {
        assert_pixel(frame, 50, 20, [0x12, 0x34, 0x56, 255])?;
        assert_pixel(frame, 405, 15, [0xAA, 0xBB, 0xCC, 255])
    })?;
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
    if engine.tick() {
        return Err(String::from(
            "idle owner still has pending native work after the unchanged-frame check",
        ));
    }
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
    if engine.render(800, 600, 1.0)?.revision != pointer_revision || engine.tick() {
        return Err(String::from("plain pointer motion repainted native pixels"));
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=pointer-move-retains-pixels");

    let previous_revision = engine.cached_revision();
    engine.render(500, 400, 2.0)?;
    wait_title(&mut engine, "width:250")?;
    let resized = await_rendered_frame(&mut engine, (500, 400, 2.0), previous_revision, |frame| {
        assert_pixel(frame, 100, 40, [0x65, 0x43, 0x21, 255])
    })?;
    engine.render(800, 600, 1.0)?;
    wait_title(&mut engine, "width:800")?;
    await_rendered_frame(&mut engine, (800, 600, 1.0), resized.revision, |frame| {
        assert_pixel(frame, 50, 20, [0x12, 0x34, 0x56, 255])
    })?;
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

    let previous_revision = engine.cached_revision();
    engine
        .navigate(&format!("{base}/damage"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "damage-ready")?;
    let initial_damage =
        await_rendered_frame(&mut engine, (800, 600, 1.0), previous_revision, |frame| {
            assert_pixel(frame, 25, 25, [0x12, 0x34, 0x56, 255])?;
            assert_pixel(frame, 300, 300, [255, 255, 255, 255])
        })?;
    let before = engine
        .paint_statistics()
        .map_err(|error| error.to_string())?;
    key(&mut engine, BrowserKey::Character('a'), true);
    key(&mut engine, BrowserKey::Character('a'), false);
    wait_title(&mut engine, "damage-changed")?;
    await_rendered_frame(
        &mut engine,
        (800, 600, 1.0),
        initial_damage.revision,
        |frame| {
            assert_pixel(frame, 25, 25, [0x65, 0x43, 0x21, 255])?;
            assert_pixel(frame, 300, 300, [255, 255, 255, 255])
        },
    )?;
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
    let previous_revision = engine.cached_revision();
    engine
        .navigate(&format!("{base}/frames"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "frames-ready")?;
    await_rendered_frame(&mut engine, (800, 600, 1.0), previous_revision, |frame| {
        assert_pixel(frame, 20, 100, [0x24, 0x68, 0xac, 255])?;
        assert_pixel(frame, 300, 100, [0xac, 0x68, 0x24, 255])
    })?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=frameset-child-pixels");

    let previous_revision = engine.cached_revision();
    engine
        .navigate(&format!("{base}/view-transition"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "view-transition-ready")?;
    let before = await_rendered_frame(&mut engine, (800, 600, 1.0), previous_revision, |frame| {
        assert_pixel(frame, 30, 30, [0x12, 0x34, 0x56, 255])
    })?;
    let composited = engine
        .compositing_statistics()
        .map_err(|error| error.to_string())?
        .is_some();
    click(&mut engine, 60.0, 155.0);
    wait_title(&mut engine, "view-transition-done")?;
    let observation = engine
        .text_content("view-transition-state")
        .map_err(|error| error.to_string())?
        .ok_or("view transition observation missing")?;
    let state: serde_json::Value =
        serde_json::from_str(&observation).map_err(|error| error.to_string())?;
    verify_view_transition(&state, composited)?;
    let after = await_rendered_frame(&mut engine, (800, 600, 1.0), before.revision, |frame| {
        assert_pixel(frame, 30, 30, [0x65, 0x43, 0x21, 255])
    })?;
    if after.revision == before.revision {
        return Err(String::from(
            "view transition did not repaint its DOM update",
        ));
    }
    let stage = if composited {
        "view-transition-composited-dom-update"
    } else {
        "view-transition-skipped-dom-update"
    };
    println!("MYRICA_WEBKIT_SMOKE_PASS stage={stage} state={state}");

    let previous_revision = engine.cached_revision();
    engine
        .navigate(&format!("{base}/js-execution"))
        .map_err(|error| error.to_string())?;
    wait_title(&mut engine, "js-execution-ready")?;
    let before = await_rendered_frame(&mut engine, (800, 600, 1.0), previous_revision, |frame| {
        assert_pixel(frame, 30, 30, [0x12, 0x34, 0x56, 255])
    })?;
    click(&mut engine, 60.0, 155.0);
    wait_title(&mut engine, "js-execution-done")?;
    let observation = engine
        .text_content("js-execution-state")
        .map_err(|error| error.to_string())?
        .ok_or("JavaScript execution observation missing")?;
    let state: serde_json::Value =
        serde_json::from_str(&observation).map_err(|error| error.to_string())?;
    verify_js_execution(&state)?;
    let after = await_rendered_frame(&mut engine, (800, 600, 1.0), before.revision, |frame| {
        assert_pixel(frame, 30, 30, [0x65, 0x43, 0x21, 255])
    })?;
    if after.revision == before.revision {
        return Err(String::from(
            "JavaScript DOM callback did not update GPU pixels",
        ));
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=js-execution-contract state={state}");

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

#[cfg(not(feature = "backend-webkit-vulkan"))]
fn painting_workers_probe(_engine: &mut ProbeEngine, _base: &str) -> Result<(), String> {
    Err("--painting-workers-only requires backend-webkit-vulkan".into())
}

#[cfg(feature = "backend-webkit-vulkan")]
fn painting_workers_probe(engine: &mut ProbeEngine, base: &str) -> Result<(), String> {
    let workers = std::env::var("WEBKIT_SKIA_CPU_PAINTING_THREADS")
        .map_err(|_| "set WEBKIT_SKIA_CPU_PAINTING_THREADS to 1, 2 or 4")?;
    if !matches!(workers.as_str(), "1" | "2" | "4") {
        return Err("painting worker comparison requires 1, 2 or 4 workers".into());
    }
    let viewport = (1600, 1200, 2.0);
    engine.render(viewport.0, viewport.1, viewport.2)?;
    let before = engine
        .compositing_statistics()
        .map_err(|error| error.to_string())?
        .ok_or("painting worker contract requires the GPU compositor")?;
    let revision = engine.cached_revision();
    engine
        .navigate(&format!("{base}/painting-workers"))
        .map_err(|error| error.to_string())?;
    wait_title(engine, "painting-workers-ready")?;
    let observation = engine
        .text_content("painting-workers-state")
        .map_err(|error| error.to_string())?
        .ok_or("painting worker DOM observation missing")?;
    let state: serde_json::Value =
        serde_json::from_str(&observation).map_err(|error| error.to_string())?;
    if state["text"] != "日本語の描画確認"
        || state["decodedImages"] != 3
        || state["canvases"] != 3
        || state["sourceWidth"] != 256
        || state["sourceHeight"] != 256
        || state["willReadFrequently"] != false
        || state["pattern"] != true
    {
        return Err(format!(
            "painting worker fixture did not initialize: {state}"
        ));
    }
    await_rendered_frame(engine, viewport, revision, inspect_painting_workers)?;
    // Drain actual pending raster/composition work rather than waiting a fixed
    // time. Hash only a settled, fully validated GPU readback.
    let deadline = Instant::now() + Duration::from_secs(20);
    let settled = loop {
        let changed = engine.tick();
        let frame = engine.render(viewport.0, viewport.1, viewport.2)?;
        inspect_painting_workers(&frame)?;
        if !changed && !engine.has_pending_frame() {
            break frame;
        }
        if Instant::now() >= deadline {
            return Err("static painting worker fixture never settled".into());
        }
        thread::sleep(Duration::from_millis(1));
    };
    let statistics = engine
        .compositing_statistics()
        .map_err(|error| error.to_string())?
        .ok_or("painting worker compositor disappeared")?;
    let tile_paints = statistics.tile_paints.saturating_sub(before.tile_paints);
    let tile_pixels = statistics.tile_pixels.saturating_sub(before.tile_pixels);
    if tile_paints < 2 || tile_pixels < 800 * 600 {
        return Err(format!(
            "fixture did not rasterize multiple real tiles: {before:?} -> {statistics:?}"
        ));
    }
    for _ in 0..8 {
        if engine.tick() || engine.has_pending_frame() {
            return Err("settled painting worker fixture scheduled more work".into());
        }
        let frame = engine.render(viewport.0, viewport.1, viewport.2)?;
        if frame.rgba != settled.rgba {
            return Err("static painting worker pixels changed after settling".into());
        }
    }
    // FNV-1a over dimensions and every RGBA byte, with a specified byte order.
    // Parent compares this value across fresh 1/2/4-worker processes. The
    // expected colour/ink checks above reject matching blank or black frames.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in settled
        .width
        .to_le_bytes()
        .into_iter()
        .chain(settled.height.to_le_bytes())
        .chain(settled.rgba.iter().copied())
    {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    println!(
        "MYRICA_WEBKIT_SMOKE_PASS stage=painting-workers workers={workers} size={}x{} scale=2 tile_paints={tile_paints} tile_pixels={tile_pixels} rgba_fnv1a64={hash:016x} canvas_gpu_requested=true owner_fallback_observed=false",
        settled.width, settled.height
    );
    Ok(())
}

#[cfg(feature = "backend-webkit-vulkan")]
fn inspect_painting_workers(frame: &ProbeFrame) -> Result<(), String> {
    if frame.width != 1600 || frame.height != 1200 || frame.rgba.len() != 1600 * 1200 * 4 {
        return Err("painting worker frame has the wrong Retina extent".into());
    }
    // Coordinates below are CSS pixels; the actual readback is DPR2.
    for (x, y, expected, tolerance) in [
        (208, 50, [64, 64, 64, 255], 3),
        (400, 50, [128, 128, 128, 255], 3),
        (592, 50, [191, 191, 191, 255], 3),
        (52, 224, [208, 32, 48, 255], 0),
        (132, 224, [32, 192, 64, 255], 4),
        (212, 224, [48, 80, 208, 255], 0),
        (400, 200, [255; 4], 0),
        (440, 240, [0, 255, 255, 255], 0),
        (500, 240, [255; 4], 0),
        (56, 360, [208, 32, 48, 255], 0),
        (136, 360, [32, 192, 64, 255], 0),
        (56, 440, [48, 80, 208, 255], 0),
        (136, 440, [224, 192, 32, 255], 0),
        (220, 330, [208, 32, 48, 255], 0),
        (240, 330, [32, 192, 64, 255], 0),
        (260, 330, [208, 32, 48, 255], 0),
        (220, 350, [48, 80, 208, 255], 0),
        (240, 350, [224, 192, 32, 255], 0),
        (752, 544, [128, 64, 192, 255], 0),
        (792, 592, [255; 4], 0),
    ] {
        assert_image_pixel(frame, x * 2, y * 2, expected, tolerance)?;
    }
    let mut ink = 0;
    for y in 136 * 2..176 * 2 {
        for x in 24 * 2..360 * 2 {
            let offset = (y * frame.width as usize + x) * 4;
            if frame.rgba[offset..offset + 3]
                .iter()
                .all(|channel| *channel < 96)
            {
                ink += 1;
            }
        }
    }
    if ink < 400 {
        return Err(format!("Japanese text GPU ink missing: {ink}"));
    }
    if frame.rgba.chunks_exact(4).any(|pixel| pixel[3] != 255) {
        return Err("painting worker frame has uninitialized/transparent pixels".into());
    }
    Ok(())
}

fn image_pipeline_probe(engine: &mut ProbeEngine, base: &str) -> Result<(), String> {
    let previous_revision = engine.cached_revision();
    engine
        .navigate(&format!("{base}/images"))
        .map_err(|error| error.to_string())?;
    let first = await_image_frame(engine, "images-ready", previous_revision, |frame| {
        inspect_image_board(frame, false, false)
    })?;
    verify_image_state(engine, 0, false, 0)?;
    println!(
        "MYRICA_WEBKIT_SMOKE_PASS stage=image-formats-flat-layer-scaled revision={}",
        first.revision
    );

    click(engine, 540.0, 38.0);
    let asynchronous = await_image_frame(engine, "images-async", first.revision, |frame| {
        inspect_image_board(frame, true, false)
    })?;
    verify_image_state(engine, 1, false, 0)?;
    if asynchronous.revision == first.revision {
        return Err("asynchronous image load retained the old GPU frame".into());
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=image-async-http-load-decode-pixels");

    click(engine, 540.0, 98.0);
    let transformed = await_image_frame(
        engine,
        "images-transformed",
        asynchronous.revision,
        |frame| {
            inspect_image_board(frame, true, true)?;
            assert_image_pixel(frame, 24, 116, [255, 255, 255, 255], 0)
        },
    )?;
    verify_image_state(engine, 1, true, 0)?;
    if transformed.revision == asynchronous.revision {
        return Err("translated image layers did not update actual GPU pixels".into());
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=image-transformed-textures-retained");

    engine.handle_input(BrowserInput::Wheel {
        delta_x: 0.0,
        delta_y: -760.0,
        x: 400.0,
        y: 440.0,
    });
    let scrolled = await_image_frame(engine, "images-scroll:760", transformed.revision, |frame| {
        for (x, colour, tolerance) in image_samples() {
            assert_image_pixel(frame, x, 64, colour, tolerance)?;
        }
        Ok(())
    })?;
    verify_image_state(engine, 1, true, 760)?;
    engine.handle_input(BrowserInput::Wheel {
        delta_x: 0.0,
        delta_y: 760.0,
        x: 400.0,
        y: 440.0,
    });
    await_image_frame(engine, "images-scroll:0", scrolled.revision, |frame| {
        inspect_image_board(frame, true, true)
    })?;
    verify_image_state(engine, 1, true, 0)?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=image-cached-scroll-return-pixels");
    Ok(())
}

fn image_samples() -> [(u32, [u8; 4], u8); 3] {
    [
        (52, [208, 32, 48, 255], 0),
        (132, [32, 192, 64, 255], 4),
        (212, [48, 80, 208, 255], 0),
    ]
}

fn inspect_image_board(frame: &ProbeFrame, asynchronous: bool, moved: bool) -> Result<(), String> {
    for (x, colour, tolerance) in image_samples() {
        assert_image_pixel(frame, x, 44, colour, tolerance)?;
        assert_image_pixel(
            frame,
            x + if moved { 24 } else { 0 },
            116,
            colour,
            tolerance,
        )?;
    }
    for (x, y, colour) in [
        (100, 225, [208, 32, 48, 255]),
        (260, 225, [32, 192, 64, 255]),
        (100, 315, [48, 80, 208, 255]),
        (260, 315, [224, 192, 32, 255]),
    ] {
        assert_image_pixel(frame, x, y, colour, 4)?;
    }
    let colour = if asynchronous {
        [48, 80, 208, 255]
    } else {
        [208, 32, 48, 255]
    };
    assert_image_pixel(frame, 312, 44, colour, 0)
}

fn assert_image_pixel(
    frame: &ProbeFrame,
    x: u32,
    y: u32,
    expected: [u8; 4],
    tolerance: u8,
) -> Result<(), String> {
    if x >= frame.width || y >= frame.height {
        return Err("image sample outside actual native frame".into());
    }
    let offset = (y as usize * frame.width as usize + x as usize) * 4;
    let actual = frame
        .rgba
        .get(offset..offset + 4)
        .ok_or("image sample outside native diagnostic pixels")?;
    if actual[..3]
        .iter()
        .zip(expected[..3].iter())
        .any(|(a, b)| a.abs_diff(*b) > tolerance)
        || actual[3] != expected[3]
    {
        return Err(format!(
            "decoded image pixel ({x},{y}) expected {expected:?} tolerance={tolerance}, got {actual:?}"
        ));
    }
    Ok(())
}

fn await_image_frame(
    engine: &mut ProbeEngine,
    title: &str,
    after_revision: u64,
    inspect: impl Fn(&ProbeFrame) -> Result<(), String>,
) -> Result<ProbeFrame, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = String::from("no decoded image frame");
    loop {
        engine.tick();
        let state = engine.snapshot();
        if matches!(state.load_state, LoadState::Failed(_))
            || state.title.starts_with("images-error:")
        {
            return Err(format!("native image fixture failed: {state:?}"));
        }
        let frame = engine.render(800, 600, 1.0)?;
        if state.title == title
            && state.load_state == LoadState::Ready
            && frame.revision > after_revision
        {
            match inspect(&frame) {
                Ok(()) => return Ok(frame),
                Err(error) => last = error,
            }
        }
        if Instant::now() >= deadline {
            let observation = engine
                .text_content("images-state")
                .map_err(|e| e.to_string())?;
            return Err(format!(
                "image stage {title:?} did not reach decoded GPU output: {last}; state={state:?} DOM={observation:?}"
            ));
        }
        // Poll a concrete DOM/GPU predicate; this is not a fixed settling delay.
        thread::sleep(Duration::from_millis(1));
    }
}

fn verify_image_state(
    engine: &mut WebKitBackend,
    async_events: u64,
    moved: bool,
    scroll: i64,
) -> Result<(), String> {
    let observation = engine
        .text_content("images-state")
        .map_err(|error| error.to_string())?
        .ok_or("image decode observation missing")?;
    let state: serde_json::Value = serde_json::from_str(&observation).map_err(|e| e.to_string())?;
    let images = state["images"]
        .as_array()
        .ok_or("image observation is not an array")?;
    if images.len() != 11
        || state["asyncLoadEvents"].as_u64() != Some(async_events)
        || state["moved"].as_bool() != Some(moved)
        || state["scrollY"].as_i64() != Some(scroll)
    {
        return Err(format!("image DOM lifecycle state mismatch: {state}"));
    }
    for id in [
        "flat-png",
        "flat-jpeg",
        "flat-webp",
        "layer-png",
        "layer-jpeg",
        "layer-webp",
        "large-image",
        "async-image",
        "cached-png",
        "cached-jpeg",
        "cached-webp",
    ] {
        let image = images
            .iter()
            .find(|image| image["id"] == id)
            .ok_or_else(|| format!("missing decoded image {id}: {state}"))?;
        let (width, height) = if id == "large-image" {
            (1280, 720)
        } else {
            (64, 48)
        };
        let path = if id == "large-image" {
            "/images/quadrants.jpg"
        } else if id.ends_with("jpeg") {
            "/images/green.jpg"
        } else if id.ends_with("webp") || (id == "async-image" && async_events == 1) {
            "/images/blue.webp"
        } else {
            "/images/red.png"
        };
        if image["complete"] != true
            || image["decoded"] != true
            || image["width"] != width
            || image["height"] != height
            || image["path"] != path
        {
            return Err(format!(
                "native image {id} did not actually decode: {image}"
            ));
        }
    }
    Ok(())
}

fn verify_js_execution(state: &serde_json::Value) -> Result<(), String> {
    let expected = serde_json::json!({
        "calls": [9, 4, 9, 13, 63],
        "arity": [[7, "undefined", 1], [7, "number", 3]],
        "recursion": 5040,
        "closure": [42, 43, 50],
        "prototype": [42, 20, true],
        "exceptions": ["js-sentinel", "finally-js", "SyntaxError", "finally-dom"],
        "typedArray": [30, 9, 7],
        "allocation": [523776, 1024],
        "rooted": ["root-live", 26, 42, true],
        "map": [13, 2, 2],
        "promise": 17,
        "order": ["sync", "after-schedule", "microtask", "await", "timer"],
        "dom": [1, 1, 17]
    });
    if state != &expected {
        return Err(format!(
            "JavaScript execution contract mismatch: expected {expected}, got {state}"
        ));
    }
    Ok(())
}

fn verify_view_transition(state: &serde_json::Value, composited: bool) -> Result<(), String> {
    // The actual engine capability determines one strict expected outcome:
    // retained compositing runs the transition; the legacy port skips it.
    let ready_matches = if composited {
        state["ready"].as_str() == Some("fulfilled")
            && state
                .as_object()
                .is_some_and(|state| state.get("readyError") == Some(&serde_json::Value::Null))
    } else {
        state["ready"].as_str() == Some("rejected")
            && state["readyError"].as_str() == Some("InvalidStateError")
    };
    if state["calls"].as_u64() != Some(1)
        || state["callbackAsync"].as_bool() != Some(true)
        || !ready_matches
        || state["updateCallbackDone"].as_str() != Some("fulfilled")
        || state["finished"].as_str() != Some("fulfilled")
        || !state
            .as_object()
            .is_some_and(|state| state.get("active") == Some(&serde_json::Value::Null))
    {
        return Err(format!(
            "view transition violated its callback or promise contract (composited={composited}): {state}"
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

fn report_video_page_state(engine: &mut ProbeEngine, url: &str, reason: &str) {
    // This existing getter also emits opt-in native currentSrc/error/buffered
    // diagnostics. Do not execute script or replace the actual site's video.
    let statistics = match engine.media_statistics(0) {
        Ok(Some(state)) => serde_json::json!({
            "videoCount": state.video_count,
            "width": state.width,
            "height": state.height,
            "paused": state.paused != 0,
            "ended": state.ended != 0,
            "readyState": state.ready_state,
            "currentTime": state.current_time,
            "duration": state.duration,
            "decodedFrames": state.decoded_frames,
            "droppedFrames": state.dropped_frames,
            "presentedFrames": state.presented_frames,
        }),
        Ok(None) => serde_json::Value::Null,
        Err(error) => serde_json::json!({ "inspectionError": error.to_string() }),
    };
    let snapshot = engine.snapshot();
    eprintln!(
        "MYRICA_WEBKIT_SMOKE_DIAGNOSTIC stage=video-state data={}",
        serde_json::json!({
            "reason": reason,
            "requestedUrl": url,
            "pageUrl": snapshot.url,
            "title": snapshot.title,
            "statistics": statistics,
        })
    );
}

fn video_page_probe(
    engine: &mut ProbeEngine,
    url: &str,
    sender: &mpsc::Sender<OwnerMessage>,
) -> Result<(), String> {
    let result = video_page_probe_inner(engine, url, sender);
    if result.is_err() {
        report_video_page_state(engine, url, "failure");
    }
    result
}

fn video_page_probe_inner(
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
                    if is_youtube_url(url) {
                        youtube_theater_probe(engine, sender, statistics, frame)?;
                    }
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
            report_video_page_state(engine, url, "wait");
            progress = Instant::now();
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn is_youtube_url(url: &str) -> bool {
    reqwest::Url::parse(url).ok().is_some_and(|url| {
        url.host_str().is_some_and(|host| {
            host == "youtube.com" || host.ends_with(".youtube.com") || host == "youtu.be"
        })
    })
}

fn video_rect_matches(
    left: &myrica_webkit_sys::MediaStatistics,
    right: &myrica_webkit_sys::MediaStatistics,
) -> bool {
    [
        (left.rect_x, right.rect_x),
        (left.rect_y, right.rect_y),
        (left.rect_width, right.rect_width),
        (left.rect_height, right.rect_height),
    ]
    .iter()
    .all(|(a, b)| a.is_finite() && b.is_finite() && (a - b).abs() <= 2.0)
}

/// Exercise YouTube's own keyboard handler and player. No diagnostic DOM or
/// replacement media element is inserted into the real site.
fn youtube_theater_probe(
    engine: &mut ProbeEngine,
    sender: &mpsc::Sender<OwnerMessage>,
    original: myrica_webkit_sys::MediaStatistics,
    initial_frame: ProbeFrame,
) -> Result<(), String> {
    let mut statistics = original;
    let mut frame = initial_frame;
    for (stage, toggle, viewport, restore) in [
        ("youtube-theater-toggle", true, (1280, 720), false),
        ("youtube-theater-return", true, (1280, 720), true),
        ("youtube-player-resize-wide", false, (1600, 900), false),
        ("youtube-player-resize-return", false, (1280, 720), true),
    ] {
        let before = statistics;
        let before_revision = frame.revision;
        if toggle {
            // This is the site's genuine 't' shortcut, not a synthetic CSS mode.
            key(engine, BrowserKey::Character('t'), true);
            key(engine, BrowserKey::Character('t'), false);
        }
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut heartbeat = Instant::now();
        let mut reference: Option<(ProbeFrame, myrica_webkit_sys::MediaStatistics)> = None;
        let mut last = String::from("no changed native video geometry");
        loop {
            engine.tick();
            frame = engine.render(viewport.0, viewport.1, 1.0)?;
            statistics = engine
                .media_statistics(0)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("{stage}: the actual site's video disappeared"))?;
            // YouTube can adapt its encoded resolution after a viewport
            // resize; require continuing decoded video without fixing its ABR.
            let geometry_changed = (statistics.rect_width - before.rect_width).abs() >= 8.0
                || (statistics.rect_height - before.rect_height).abs() >= 8.0;
            let geometry_restored = !restore || video_rect_matches(&statistics, &original);
            if geometry_changed
                && geometry_restored
                && statistics.rect_width > 0.0
                && statistics.rect_height > 0.0
                && frame.revision > before_revision
            {
                if reference
                    .as_ref()
                    .is_none_or(|(_, rect)| !video_rect_matches(&statistics, rect))
                {
                    reference = Some((frame.clone(), statistics));
                }
                let (first, _) = reference.as_ref().ok_or("missing video pixel reference")?;
                // Sample inside the real video rectangle, away from player
                // chrome. A geometry-only repaint cannot satisfy this check.
                let left = (statistics.rect_x + statistics.rect_width * 0.15)
                    .clamp(0.0, f64::from(frame.width)) as usize;
                let right = (statistics.rect_x + statistics.rect_width * 0.85)
                    .clamp(0.0, f64::from(frame.width)) as usize;
                let top = (statistics.rect_y + statistics.rect_height * 0.15)
                    .clamp(0.0, f64::from(frame.height)) as usize;
                let bottom = (statistics.rect_y + statistics.rect_height * 0.85)
                    .clamp(0.0, f64::from(frame.height)) as usize;
                let mut changed = 0;
                if frame.revision > first.revision {
                    for y in top..bottom {
                        for x in left..right {
                            let offset = (y * frame.width as usize + x) * 4;
                            if first.rgba[offset..offset + 4] != frame.rgba[offset..offset + 4] {
                                changed += 1;
                            }
                        }
                    }
                }
                if changed >= 100
                    && statistics.current_time > before.current_time + 0.5
                    && statistics.presented_frames >= before.presented_frames + 4
                    && statistics.paused == 0
                    && statistics.ended == 0
                {
                    println!(
                        "MYRICA_WEBKIT_SMOKE_PASS stage={stage} viewport={}x{} changed_video_pixels={changed} revision={}..{} before={before:?} after={statistics:?}",
                        viewport.0, viewport.1, before_revision, frame.revision
                    );
                    break;
                }
                last = format!(
                    "geometry changed but future video has not advanced: changed_video_pixels={changed} statistics={statistics:?}"
                );
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{stage}: real YouTube shortcut/resize did not produce continuing video: {last}; before={before:?}; after={statistics:?}"
                ));
            }
            if heartbeat.elapsed() >= Duration::from_secs(5) {
                sender
                    .send(OwnerMessage::PageProgress)
                    .map_err(|error| error.to_string())?;
                heartbeat = Instant::now();
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
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
    let previous_revision = engine.cached_revision();
    engine
        .navigate(&format!("{base}/media/{path}"))
        .map_err(|error| error.to_string())?;
    wait_title(engine, "media-ready")?;
    let state = media_state(engine)?;
    if state["width"] != 128 || state["height"] != 96 || state["ready"].as_u64().unwrap_or(0) < 3 {
        return Err(format!("metadata or ready state failed ({path}): {state}"));
    }
    // Metadata readiness precedes asynchronous GPU publication. Require this
    // document's completed frame before targeting its real Play button.
    await_rendered_frame(engine, (400, 240, 1.0), previous_revision, |frame| {
        assert_pixel(frame, 280, 60, [255, 255, 255, 255])
    })?;
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

/// Observe real native decoder output separately from static tile raster work.
#[cfg(feature = "backend-webkit-vulkan")]
fn compositor_video_probe(engine: &mut ProbeEngine, base: &str) -> Result<(), String> {
    engine.render(400, 240, 1.0)?;
    let previous_revision = engine.cached_revision();
    let before_fixture = engine
        .compositing_statistics()
        .map_err(|error| error.to_string())?
        .ok_or("video compositor unexpectedly disabled")?;
    engine
        .navigate(&format!("{base}/media/compositor"))
        .map_err(|error| error.to_string())?;
    wait_title(engine, "media-ready")?;
    await_rendered_frame(engine, (400, 240, 1.0), previous_revision, |frame| {
        assert_pixel(frame, 280, 60, [0x12, 0x34, 0x56, 255])
    })?;
    click(engine, 60.0, 155.0);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut baseline: Option<(
        myrica_webkit_sys::CompositingStatistics,
        u64,
        u64,
        ProbeFrame,
    )> = None;
    loop {
        engine.tick();
        let state = media_state(engine)?;
        if state["error"].as_u64().unwrap_or(0) != 0
            || engine.snapshot().title.starts_with("media-error:")
        {
            return Err(format!("compositor native video failed: {state}"));
        }
        let frame = engine.render(400, 240, 1.0)?;
        let statistics = engine
            .compositing_statistics()
            .map_err(|error| error.to_string())?
            .ok_or("video compositor unexpectedly disabled")?;
        let callbacks = state["callbacks"].as_u64().unwrap_or(0);
        let time = state["time"].as_f64().unwrap_or(0.0);
        let colour_offset = (60 * frame.width as usize + 60) * 4;
        let red = frame.rgba[colour_offset] > 200
            && frame.rgba[colour_offset + 1] < 30
            && frame.rgba[colour_offset + 2] < 30;
        assert_pixel(&frame, 280, 60, [0x12, 0x34, 0x56, 255])?;
        if baseline.is_none() && callbacks >= 1 && time > 0.0 && red {
            if statistics.tile_paints <= before_fixture.tile_paints
                || statistics.tile_pixels <= before_fixture.tile_pixels
            {
                return Err(format!(
                    "static video board has no actual tile raster work: {statistics:?}"
                ));
            }
            let page_paints = engine
                .paint_statistics()
                .map_err(|error| error.to_string())?
                .paints;
            baseline = Some((statistics, callbacks, page_paints, frame.clone()));
        }
        if let Some((initial, initial_callbacks, initial_page_paints, first)) = &baseline {
            if statistics != *initial {
                return Err(format!(
                    "video-only frames repainted static tiles: {initial:?} -> {statistics:?}, state={state}"
                ));
            }
            // The encoded fixture changes red -> blue -> green. Waiting for
            // decoded pixels and native frame callbacks avoids a fixed delay.
            let blue = frame.rgba[colour_offset] < 30
                && frame.rgba[colour_offset + 1] < 30
                && frame.rgba[colour_offset + 2] > 200;
            if blue && callbacks >= initial_callbacks + 2 {
                let page_paints = engine
                    .paint_statistics()
                    .map_err(|error| error.to_string())?
                    .paints;
                if page_paints <= *initial_page_paints || frame.revision <= first.revision {
                    return Err("video callbacks did not submit fresh GPU compositions".into());
                }
                let mut changed_video = 0;
                for y in 20..116usize {
                    for x in 20..148usize {
                        let offset = (y * frame.width as usize + x) * 4;
                        changed_video += usize::from(
                            first.rgba[offset..offset + 4] != frame.rgba[offset..offset + 4],
                        );
                    }
                }
                for y in 20..200usize {
                    for x in 200..360usize {
                        let offset = (y * frame.width as usize + x) * 4;
                        if first.rgba[offset..offset + 4] != frame.rgba[offset..offset + 4] {
                            return Err(format!(
                                "video composition mutated static board at {x},{y}"
                            ));
                        }
                    }
                }
                if changed_video < 100 {
                    return Err(format!(
                        "video advanced without changing GPU pixels: {changed_video}"
                    ));
                }
                println!(
                    "MYRICA_WEBKIT_SMOKE_PASS stage=compositor-video-static-tiles tile_delta=0 page_compositions={} callback_delta={} changed_video_pixels={changed_video}",
                    page_paints - initial_page_paints,
                    callbacks - initial_callbacks
                );
                return compositor_video_reparent_probe(engine);
            }
        }
        if state["ended"] == true || Instant::now() >= deadline {
            return Err(format!(
                "native compositor video contract incomplete: baseline={} state={state}",
                baseline.is_some()
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(feature = "backend-webkit-vulkan")]
#[derive(Clone, Copy)]
struct VideoProgress {
    callbacks: u64,
    pts: f64,
    presented: u64,
    revision: u64,
}

#[cfg(feature = "backend-webkit-vulkan")]
fn video_progress(engine: &mut ProbeEngine) -> Result<VideoProgress, String> {
    let state = media_state(engine)?;
    let native = engine
        .media_statistics(0)
        .map_err(|error| error.to_string())?
        .ok_or("missing native video presentation statistics")?;
    Ok(VideoProgress {
        callbacks: state["callbacks"]
            .as_u64()
            .ok_or("missing frame callbacks")?,
        pts: state["metadata"]["time"]
            .as_f64()
            .ok_or("missing actual rVFC mediaTime")?,
        presented: native.presented_frames,
        revision: engine.cached_revision(),
    })
}

#[cfg(feature = "backend-webkit-vulkan")]
fn video_colour(frame: &ProbeFrame, x: u32, y: u32) -> Option<usize> {
    if x >= frame.width || y >= frame.height {
        return None;
    }
    let offset = (y as usize * frame.width as usize + x as usize) * 4;
    let pixel = frame.rgba.get(offset..offset + 4)?;
    [[255u8, 0, 0, 255], [0, 0, 255, 255], [0, 255, 0, 255]]
        .iter()
        .position(|colour| pixel.iter().zip(colour).all(|(a, b)| a.abs_diff(*b) <= 30))
}

#[cfg(feature = "backend-webkit-vulkan")]
fn inspect_video_layout(
    frame: &ProbeFrame,
    state: &serde_json::Value,
    theater: bool,
    scroll: f64,
    viewport: (u32, u32, f32),
) -> Result<usize, String> {
    let layout = &state["compositor"];
    let phase = if theater { "theater" } else { "inline" };
    let parent = if theater {
        "theater-player"
    } else {
        "inline-player"
    };
    let scale = f64::from(viewport.2);
    let css_width = f64::from(viewport.0) / scale;
    let width = if theater { css_width - 32.0 } else { 128.0 };
    let height = width * 0.75;
    let top = if theater {
        56.0 - scroll
    } else {
        20.0 - scroll
    };
    if layout["phase"] != phase
        || layout["parent"] != parent
        || layout["sameVideo"] != true
        || layout["scrollY"]
            .as_f64()
            .is_none_or(|y| (y - scroll).abs() > 0.5)
        || layout["rect"]["width"]
            .as_f64()
            .is_none_or(|w| (w - width).abs() > 0.5)
        || layout["rect"]["height"]
            .as_f64()
            .is_none_or(|h| (h - height).abs() > 0.5)
        || layout["rect"]["y"]
            .as_f64()
            .is_none_or(|y| (y - top).abs() > 0.5)
    {
        return Err(format!(
            "video reparent/resize/scroll has not reached {phase}: {layout}"
        ));
    }
    let physical = |value: f64| (value * scale).round() as u32;
    if theater {
        let marker = if scroll == 0.0 {
            [0xb0, 0x40, 0x80, 255]
        } else {
            [0x20, 0x20, 0x20, 255]
        };
        assert_pixel(frame, physical(8.0), physical(8.0), marker)?;
    } else {
        assert_pixel(
            frame,
            physical(280.0),
            physical(60.0),
            [0x12, 0x34, 0x56, 255],
        )?;
        assert_pixel(
            frame,
            physical(180.0),
            physical(100.0),
            [0x24, 0x68, 0xac, 255],
        )?;
    }
    let first = if theater { (60.0, 100.0) } else { (60.0, 60.0) };
    let second = if theater {
        (css_width - 60.0, f64::from(viewport.1) / scale - 60.0)
    } else {
        (130.0, 100.0)
    };
    let first_xy = (physical(first.0), physical(first.1));
    let second_xy = (physical(second.0), physical(second.1));
    let first_colour = video_colour(frame, first_xy.0, first_xy.1);
    let second_colour = video_colour(frame, second_xy.0, second_xy.1);
    match (first_colour, second_colour) {
        (Some(colour), Some(other)) if colour == other => Ok(colour),
        _ => {
            let rgba = |(x, y): (u32, u32)| -> Option<[u8; 4]> {
                if x >= frame.width || y >= frame.height {
                    return None;
                }
                let offset = (y as usize * frame.width as usize + x as usize) * 4;
                frame.rgba.get(offset..offset + 4)?.try_into().ok()
            };
            let edge_samples =
                [2032, 2040, 2048, 2056].map(|x| (x, first_xy.1, rgba((x, first_xy.1))));
            Err(format!(
                "expanded/restored video has stale, clipped or missing GPU pixels: first_xy={first_xy:?} first_rgba={:?} first_colour={first_colour:?} second_xy={second_xy:?} second_rgba={:?} second_colour={second_colour:?} edge_samples={edge_samples:?} frame={}x{} revision={} viewport={viewport:?} layout={layout}",
                rgba(first_xy),
                rgba(second_xy),
                frame.width,
                frame.height,
                frame.revision
            ))
        }
    }
}

#[cfg(feature = "backend-webkit-vulkan")]
fn await_video_layout_progress(
    engine: &mut ProbeEngine,
    start: VideoProgress,
    stage: &str,
    theater: bool,
    scroll: f64,
    viewport: (u32, u32, f32),
) -> Result<VideoProgress, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut first_colour = None;
    let mut last = String::from("no new GPU frame after layout change");
    loop {
        engine.tick();
        let frame = engine.render(viewport.0, viewport.1, viewport.2)?;
        let state = media_state(engine)?;
        if state["error"].as_u64().unwrap_or(0) != 0
            || state["ended"] == true
            || engine.snapshot().title.starts_with("media-error:")
        {
            return Err(format!(
                "{stage}: video failed/stopped during layout changes: {state}"
            ));
        }
        if frame.revision > start.revision {
            match inspect_video_layout(&frame, &state, theater, scroll, viewport) {
                Ok(colour) => {
                    let (initial_colour, initial_revision) =
                        *first_colour.get_or_insert((colour, frame.revision));
                    let now = video_progress(engine)?;
                    if colour != initial_colour
                        && frame.revision > initial_revision
                        && now.callbacks >= start.callbacks + 2
                        && now.pts > start.pts
                        && now.presented >= start.presented + 2
                        && state["paused"] == false
                    {
                        println!(
                            "MYRICA_WEBKIT_SMOKE_PASS stage={stage} size={}x{} scale={} callback_delta={} presented_delta={} pts={}..{} revision={}..{}",
                            viewport.0,
                            viewport.1,
                            viewport.2,
                            now.callbacks - start.callbacks,
                            now.presented - start.presented,
                            start.pts,
                            now.pts,
                            start.revision,
                            frame.revision
                        );
                        return Ok(now);
                    }
                    last =
                        format!("decoded pixels/callbacks have not both advanced: state={state}");
                }
                Err(error) => last = error,
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{stage}: native video layout progression stalled: {last}"
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(feature = "backend-webkit-vulkan")]
fn compositor_video_reparent_probe(engine: &mut ProbeEngine) -> Result<(), String> {
    let small = (400, 240, 1.0);
    let wide = (2560, 1440, 2.0);
    let before_hide = engine.cached_revision();
    type_character(engine, 'h');
    await_rendered_frame(engine, small, before_hide, |frame| {
        assert_pixel(frame, 60, 60, [0x24, 0x68, 0xac, 255])?;
        assert_pixel(frame, 280, 60, [0x12, 0x34, 0x56, 255])
    })?;
    let hidden = media_state(engine)?;
    if hidden["compositor"]["phase"] != "hidden" || hidden["compositor"]["sameVideo"] != true {
        return Err(format!(
            "video was replaced instead of genuinely hidden: {hidden}"
        ));
    }
    let mut progress = video_progress(engine)?;
    type_character(engine, 't');
    progress = await_video_layout_progress(
        engine,
        progress,
        "video-reparent-wide-dpr2",
        true,
        0.0,
        wide,
    )?;
    engine.handle_input(BrowserInput::Wheel {
        delta_x: 0.0,
        delta_y: -80.0,
        x: 1260.0,
        y: 680.0,
    });
    progress =
        await_video_layout_progress(engine, progress, "video-theater-scroll", true, 80.0, wide)?;
    engine.handle_input(BrowserInput::Wheel {
        delta_x: 0.0,
        delta_y: 80.0,
        x: 1260.0,
        y: 680.0,
    });
    progress = await_video_layout_progress(
        engine,
        progress,
        "video-theater-scroll-return",
        true,
        0.0,
        wide,
    )?;
    type_character(engine, 't');
    await_video_layout_progress(engine, progress, "video-inline-restore", false, 0.0, small)?;

    type_character(engine, 'p');
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        engine.tick();
        engine.render(small.0, small.1, small.2)?;
        if media_state(engine)?["paused"] == true {
            break;
        }
        if Instant::now() >= deadline {
            return Err("video did not really pause before reparent".into());
        }
        thread::sleep(Duration::from_millis(5));
    }
    let paused_time = media_state(engine)?["time"]
        .as_f64()
        .ok_or("missing paused time")?;
    let paused_revision = engine.cached_revision();
    type_character(engine, 't');
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        engine.tick();
        let frame = engine.render(small.0, small.1, small.2)?;
        let state = media_state(engine)?;
        if state["paused"] != true
            || state["time"]
                .as_f64()
                .is_none_or(|t| (t - paused_time).abs() > 0.05)
        {
            return Err(format!(
                "reparent unexpectedly resumed the paused video: {state}"
            ));
        }
        if frame.revision > paused_revision
            && inspect_video_layout(&frame, &state, true, 0.0, small).is_ok()
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!("paused video did not survive reparent: {state}"));
        }
        thread::sleep(Duration::from_millis(5));
    }
    println!(
        "MYRICA_WEBKIT_SMOKE_PASS stage=video-paused-reparent pixels=decoded same-element=true"
    );
    // Re-read native callback counts after reparent; future playback must advance
    // beyond this point, not reuse feedback from before the layout switch.
    progress = video_progress(engine)?;
    type_character(engine, 'p');
    progress =
        await_video_layout_progress(engine, progress, "video-reparent-resume", true, 0.0, small)?;
    type_character(engine, 't');
    await_video_layout_progress(
        engine,
        progress,
        "video-final-inline-restore",
        false,
        0.0,
        small,
    )?;
    Ok(())
}

fn await_rendered_frame(
    engine: &mut ProbeEngine,
    viewport: (u32, u32, f32),
    after_revision: u64,
    inspect: impl Fn(&ProbeFrame) -> Result<(), String>,
) -> Result<ProbeFrame, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = String::from("no newer completed native frame");
    loop {
        engine.tick();
        let state = engine.snapshot();
        if matches!(state.load_state, LoadState::Failed(_)) {
            return Err(format!("native rendering failed: {state:?}"));
        }
        let frame = engine.render(viewport.0, viewport.1, viewport.2)?;
        if frame.revision > after_revision {
            match inspect(&frame) {
                Ok(()) => return Ok(frame),
                Err(error) => last = error,
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "GPU output did not advance beyond revision {after_revision} with expected pixels: {last}; latest={} state={state:?}",
                frame.revision
            ));
        }
        thread::sleep(Duration::from_millis(1));
    }
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
