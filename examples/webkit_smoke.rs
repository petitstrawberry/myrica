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
    BrowserBackend, BrowserFrame, BrowserInput, BrowserKey, BrowserModifiers, BrowserMouseButton,
    LoadState, WakeCallback, WebKitBackend,
};

enum OwnerMessage {
    InterruptHook(WakeCallback),
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
    unsafe { std::env::set_var("MYRICA_WEBKIT_TRACE_SHUTDOWN", "1") };
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
    let base = std::env::args().nth(1).ok_or("usage: webkit-smoke http://HOST:PORT | --pages URL...")?;
    let pages: Vec<String> = if base == "--pages" { std::env::args().skip(2).collect() } else { Vec::new() };
    if base == "--pages" && pages.is_empty() { return Err(String::from("--pages requires URLs")); }
    let (sender, receiver) = mpsc::channel();
    let worker = thread::Builder::new()
        .name(String::from("myrica-webkit-smoke"))
        .stack_size(8 * 1024 * 1024)
        .spawn(move || engine_probe(&base, sender, &pages))
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

fn engine_probe(base: &str, sender: mpsc::Sender<OwnerMessage>, pages: &[String]) -> Result<(), String> {
    let wake: WakeCallback = Arc::new(|| {});
    let mut engine = WebKitBackend::new(wake).map_err(|error| error.to_string())?;
    sender
        .send(OwnerMessage::InterruptHook(
            engine
                .shutdown_callback()
                .ok_or("missing native interruption hook")?,
        ))
        .map_err(|error| error.to_string())?;
    if !pages.is_empty() {
        for url in pages {
            engine.render(1280, 720, 1.0);
            engine.navigate(url).map_err(|error| error.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                engine.tick();
                let state = engine.snapshot();
                if matches!(state.load_state, LoadState::Failed(_)) { return Err(format!("page failed: {state:?}")); }
                if state.load_state == LoadState::Ready && !state.title.is_empty() { break; }
                if Instant::now() >= deadline { return Err(format!("page stalled: {state:?}")); }
                thread::sleep(Duration::from_millis(10));
            }
            pump(&mut engine, Duration::from_secs(3));
            if url == "https://ichigo.dev/" {
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    engine.tick();
                    let text = engine.text_content("lastfm-placeholder").map_err(|error| error.to_string())?;
                    if let Some(text) = text.as_ref() {
                        if !text.contains("Loading...") && !text.trim().is_empty() {
                            println!("MYRICA_WEBKIT_DOM url={url} id=lastfm-placeholder text={text:?}");
                            break;
                        }
                    }
                    if Instant::now() >= deadline { return Err(format!("Recent Tracks was not replaced: {text:?}")); }
                    thread::sleep(Duration::from_millis(10));
                }
            }
            let frame = engine.render(1280, 720, 1.0);
            let texture = frame.draws.first().and_then(|draw| draw.texture.as_ref()).ok_or("page has no pixels")?;
            let nonwhite = texture.pixels.chunks_exact(4).filter(|pixel| pixel[3] != 0 && pixel[..3] != [255, 255, 255]).count();
            println!("MYRICA_WEBKIT_PAGE url={url} nonwhite={nonwhite} revision={} state={:?}", frame.revision, engine.snapshot());
            if nonwhite < 100 { return Err(format!("page remains blank: {url}")); }
        }
        drop(engine);
        return Ok(());
    }
    engine.render(800, 600, 1.0);
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
    let frame = engine.render(800, 600, 1.0);
    assert_pixel(&frame, 50, 20, [0x12, 0x34, 0x56, 255])?;
    assert_pixel(&frame, 405, 15, [0xAA, 0xBB, 0xCC, 255])?;
    println!(
        "MYRICA_WEBKIT_SMOKE_PASS stage=live-navigation-css-image frame={}",
        frame.revision
    );

    let revision = frame.revision;
    for _ in 0..20 {
        engine.tick();
        thread::sleep(Duration::from_millis(17));
    }
    let idle = engine.render(800, 600, 1.0);
    if idle.revision != revision {
        return Err(format!(
            "idle frame changed from {revision} to {}",
            idle.revision
        ));
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=idle-frame revision={revision}");

    // Plain pointer movement over an unchanged background must retain pixels.
    engine.handle_input(BrowserInput::PointerMoved { x: 350.0, y: 300.0 });
    engine.tick();
    let pointer_revision = engine.render(800, 600, 1.0).revision;
    for index in 0..100 {
        engine.handle_input(BrowserInput::PointerMoved { x: 350.0 + (index % 10) as f32, y: 300.0 });
        engine.tick();
    }
    if engine.render(800, 600, 1.0).revision != pointer_revision {
        return Err(String::from("plain pointer motion repainted native pixels"));
    }
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=pointer-move-retains-pixels");

    engine.render(500, 400, 2.0);
    wait_title(&mut engine, "width:250")?;
    assert_pixel(
        &engine.render(500, 400, 2.0),
        100,
        40,
        [0x65, 0x43, 0x21, 255],
    )?;
    engine.render(800, 600, 1.0);
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

    engine.navigate(&format!("{base}/storage")).map_err(|error| error.to_string())?;
    wait_title(&mut engine, "storage-blob-iframe-ready")?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=storage-cookie-blob-iframe");
    engine.navigate(&format!("{base}/frames")).map_err(|error| error.to_string())?;
    wait_title(&mut engine, "frames-ready")?;
    let frame = engine.render(800, 600, 1.0);
    assert_pixel(&frame, 20, 100, [0x24, 0x68, 0xac, 255])?;
    assert_pixel(&frame, 300, 100, [0xac, 0x68, 0x24, 255])?;
    println!("MYRICA_WEBKIT_SMOKE_PASS stage=frameset-child-pixels");

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

fn assert_pixel(frame: &BrowserFrame, x: u32, y: u32, expected: [u8; 4]) -> Result<(), String> {
    let texture = frame
        .draws
        .first()
        .and_then(|draw| draw.texture.as_ref())
        .ok_or("native frame has no real texture")?;
    let offset = (y as usize * texture.width as usize + x as usize) * 4;
    let pixel = texture
        .pixels
        .get(offset..offset + 4)
        .ok_or("pixel outside native frame")?;
    if pixel != expected {
        return Err(format!(
            "pixel ({x},{y}) expected {expected:?}, got {pixel:?}"
        ));
    }
    Ok(())
}
