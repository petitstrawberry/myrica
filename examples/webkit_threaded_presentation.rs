//! A real native-video integration check: block the Page in a three-second JS
//! click handler while the other thread acquires completed GPU page images.
#[allow(dead_code, unused_imports)]
#[path = "../src/backend/mod.rs"]
mod backend;
#[path = "../src/platform/mod.rs"]
mod platform;

use backend::{
    BrowserBackend, BrowserFrameSource, BrowserInput, BrowserMouseButton, WebKitBackend,
};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

enum Command {
    Play,
    Busy,
    Stop,
}
enum Event {
    Source(Box<dyn BrowserFrameSource>),
    Title(String),
    Entering(u64),
    Returned(Duration, u64),
}

fn click(engine: &mut WebKitBackend, x: f32, y: f32) {
    engine.handle_input(BrowserInput::PointerMoved { x, y });
    for pressed in [true, false] {
        engine.handle_input(BrowserInput::PointerButton {
            button: BrowserMouseButton::Primary,
            pressed,
            x,
            y,
        });
    }
}

fn run() -> Result<(), String> {
    let url = std::env::args()
        .nth(1)
        .ok_or("supply the /media/threaded fixture URL")?;
    let expect_video = !std::env::args().any(|arg| arg == "--css-only");
    let (commands, receive) = mpsc::channel();
    let (events, updates) = mpsc::channel();
    let owner = thread::Builder::new()
        .name("webkit-test-owner".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || -> Result<(), String> {
            let mut engine = WebKitBackend::new(Arc::new(|| {})).map_err(|e| e.to_string())?;
            engine.set_frame_generation(1);
            events
                .send(Event::Source(
                    engine
                        .take_frame_source()
                        .ok_or("missing compositor endpoint")?,
                ))
                .map_err(|e| e.to_string())?;
            engine.navigate(&url).map_err(|e| e.to_string())?;
            let mut title = String::new();
            loop {
                while let Ok(command) = receive.try_recv() {
                    match command {
                        Command::Stop => return Ok(()),
                        Command::Play => click(&mut engine, 40.0, 235.0),
                        Command::Busy => {
                            let before = engine.media_statistics(0).map_err(|e| e.to_string())?;
                            if expect_video && before.is_none() {
                                return Err("missing video".into());
                            }
                            let before = before.map_or(0, |video| video.presented_frames);
                            events
                                .send(Event::Entering(before))
                                .map_err(|e| e.to_string())?;
                            let start = Instant::now();
                            click(&mut engine, 200.0, 235.0);
                            let elapsed = start.elapsed();
                            engine.tick(); // Deliver queued presentation callbacks on the owner.
                            let after = engine.media_statistics(0).map_err(|e| e.to_string())?;
                            if expect_video && after.is_none() {
                                return Err("missing video after JS".into());
                            }
                            let after = after.map_or(0, |video| video.presented_frames);
                            events
                                .send(Event::Returned(elapsed, after))
                                .map_err(|e| e.to_string())?;
                        }
                    }
                }
                engine.tick();
                engine.render_if_ready(800, 600, 1.0);
                let snapshot = engine.snapshot();
                if let backend::LoadState::Failed(error) = snapshot.load_state {
                    return Err(error);
                }
                if snapshot.title != title {
                    title = snapshot.title;
                    events
                        .send(Event::Title(title.clone()))
                        .map_err(|e| e.to_string())?;
                }
                thread::sleep(Duration::from_millis(2));
            }
        })
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let mut source = match updates
            .recv_timeout(Duration::from_secs(30))
            .map_err(|e| e.to_string())?
        {
            Event::Source(source) => source,
            _ => return Err("first owner message was not the output endpoint".into()),
        };
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut playing = false;
        let mut warm_frames = 0;
        let mut requested_busy = false;
        let mut busy = None;
        let mut frames = 0;
        let mut last_frame = None;
        let mut max_gap = Duration::ZERO;
        let mut revision = 0;
        loop {
            while let Ok(event) = updates.try_recv() {
                match event {
                    Event::Title(title) if title == "threaded-ready" => {
                        commands.send(Command::Play).map_err(|e| e.to_string())?;
                    }
                    Event::Title(title) if title == "threaded-playing" => playing = true,
                    Event::Title(title) if title.starts_with("threaded-error:") => {
                        return Err(title);
                    }
                    Event::Entering(before) => {
                        busy = Some(before);
                        frames = 0;
                        last_frame = Some(Instant::now());
                        max_gap = Duration::ZERO;
                    }
                    Event::Returned(elapsed, after) => {
                        let before = busy.ok_or("missing busy entry")?;
                        if let Some(last) = last_frame {
                            max_gap = max_gap.max(Instant::now() - last);
                        }
                        if elapsed < Duration::from_millis(2900) {
                            return Err(format!("JS handler did not block: {elapsed:?}"));
                        }
                        if frames < 18
                            || (expect_video && after < before + 18)
                            || max_gap > Duration::from_millis(300)
                        {
                            return Err(format!(
                                "video stalled during JS: output={frames} presented={before}->{after} gap={max_gap:?}"
                            ));
                        }
                        println!(
                            "MYRICA_THREADED_PRESENTATION_PASS kind={} js_ms={} output_frames={} presented_delta={} max_output_gap_ms={}",
                            if expect_video { "video" } else { "css" },
                            elapsed.as_millis(),
                            frames,
                            after - before,
                            max_gap.as_millis()
                        );
                        commands.send(Command::Stop).map_err(|e| e.to_string())?;
                        owner_finished(&owner)?;
                        // This endpoint deliberately outlives Page destruction.
                        drop(source.poll().map_err(|e| e.to_string())?);
                        return Ok(());
                    }
                    _ => {}
                }
            }
            if let Some(completed) = source.poll().map_err(|e| e.to_string())? {
                if completed.frame.revision <= revision {
                    return Err("non-monotonic output revision".into());
                }
                revision = completed.frame.revision;
                if playing {
                    warm_frames += 1;
                }
                if busy.is_some() {
                    let now = Instant::now();
                    if let Some(last) = last_frame {
                        max_gap = max_gap.max(now - last);
                    }
                    last_frame = Some(now);
                    frames += 1;
                }
            }
            if playing && warm_frames >= 8 && !requested_busy {
                commands.send(Command::Busy).map_err(|e| e.to_string())?;
                requested_busy = true;
            }
            if owner.is_finished() {
                return Err("owner exited before independent presentation completed".into());
            }
            if Instant::now() > deadline {
                return Err("threaded presentation timed out".into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    })();
    let _ = commands.send(Command::Stop);
    owner_finished(&owner)?;
    let joined = owner.join().map_err(|_| "native owner panicked")?;
    joined?;
    result
}

fn owner_finished(owner: &thread::JoinHandle<Result<(), String>>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !owner.is_finished() {
        if Instant::now() > deadline {
            return Err("native owner failed to join".into());
        }
        thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("MYRICA_THREADED_PRESENTATION_FAIL {error}");
        std::process::exit(1);
    }
}
