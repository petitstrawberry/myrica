//! The browser engine is confined to one worker; UI calls only exchange messages.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    BackendError, BrowserBackend, BrowserFrame, BrowserInput, BrowserSnapshot, LoadState,
    WakeCallback, create_backend,
};

const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
// Before the worker split, Application::on_idle drove tick every UI cycle.
// Preserve that contract: a backend wake is an early-poll hint, not a promise
// that every pending operation will notify us before it can make progress.
const ENGINE_POLL_INTERVAL: Duration = FRAME_INTERVAL;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

enum Command {
    Navigate(String),
    Reload,
    Back,
    Forward,
    Input(BrowserInput),
}

struct Update {
    generation: u64,
    snapshot: BrowserSnapshot,
    frame: Option<(BrowserViewport, BrowserFrame)>,
}

struct Mailbox {
    commands: VecDeque<(u64, Command)>,
    viewport: BrowserViewport,
    generation: u64,
    engine_woken: bool,
    shutdown: bool,
    update: Option<Update>,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    ready: Condvar,
}

/// UI-side proxy. No document, JS runtime, or layout code runs in these methods.
pub struct BrowserWorker {
    shared: Arc<Shared>,
    snapshot: BrowserSnapshot,
    worker: thread::JoinHandle<()>,
}

impl BrowserWorker {
    pub fn new(location: &str, viewport: BrowserViewport) -> Result<Self, BackendError> {
        Self::with_factory(location, viewport, create_backend)
    }

    fn with_factory(
        location: &str,
        viewport: BrowserViewport,
        factory: impl FnOnce(WakeCallback) -> Result<Box<dyn BrowserBackend>, BackendError>
        + Send
        + 'static,
    ) -> Result<Self, BackendError> {
        let snapshot = BrowserSnapshot {
            backend_name: "Blitz",
            url: location.to_owned(),
            title: String::from("Myrica"),
            load_state: LoadState::Loading,
            can_go_back: false,
            can_go_forward: false,
            text_input: None,
        };
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox {
                commands: VecDeque::from([(1, Command::Navigate(location.to_owned()))]),
                viewport,
                generation: 1,
                engine_woken: true,
                shutdown: false,
                update: None,
            }),
            ready: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let failure_snapshot = snapshot.clone();
        let worker = thread::Builder::new()
            .name(String::from("myrica-engine"))
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                // Construct even non-Send engine state on its owning thread.
                let weak = Arc::downgrade(&worker_shared);
                let wake_engine: WakeCallback = Arc::new(move || {
                    if let Some(shared) = weak.upgrade() {
                        shared.mailbox.lock().unwrap().engine_woken = true;
                        shared.ready.notify_one();
                    }
                });
                match factory(wake_engine) {
                    Ok(backend) => run_engine(backend, &worker_shared),
                    Err(error) => {
                        let mut snapshot = failure_snapshot;
                        snapshot.load_state = LoadState::Failed(error.to_string());
                        worker_shared.publish(1, snapshot, None);
                    }
                }
            })
            .map_err(|error| BackendError::new(format!("start browser worker: {error}")))?;
        Ok(Self {
            shared,
            snapshot,
            worker,
        })
    }

    pub fn snapshot(&self) -> BrowserSnapshot {
        self.snapshot.clone()
    }

    pub fn navigate(&mut self, location: &str) -> Result<(), BackendError> {
        self.send_navigation(Command::Navigate(location.to_owned()))?;
        self.snapshot.url = location.to_owned();
        Ok(())
    }

    pub fn reload(&mut self) {
        let _ = self.send_navigation(Command::Reload);
    }

    pub fn go_back(&mut self) {
        if self.snapshot.can_go_back {
            let _ = self.send_navigation(Command::Back);
        }
    }

    pub fn go_forward(&mut self) {
        if self.snapshot.can_go_forward {
            let _ = self.send_navigation(Command::Forward);
        }
    }

    fn send_navigation(&mut self, command: Command) -> Result<(), BackendError> {
        if self.worker.is_finished() {
            let error = BackendError::new("browser worker stopped");
            self.snapshot.load_state = LoadState::Failed(error.to_string());
            return Err(error);
        }
        let old_update = {
            let mut mailbox = self.shared.mailbox.lock().unwrap();
            mailbox.generation = mailbox.generation.wrapping_add(1);
            let generation = mailbox.generation;
            mailbox.commands.push_back((generation, command));
            mailbox.update.take()
        };
        drop(old_update);
        self.snapshot.load_state = LoadState::Loading;
        self.snapshot.text_input = None;
        self.shared.ready.notify_one();
        Ok(())
    }

    pub fn resize(&self, viewport: BrowserViewport) {
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        if mailbox.viewport != viewport {
            mailbox.viewport = viewport;
            mailbox.engine_woken = true;
            self.shared.ready.notify_one();
        }
    }

    pub fn handle_input(&self, input: BrowserInput) -> bool {
        if self.worker.is_finished() {
            return false;
        }
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        let generation = mailbox.generation;
        enqueue_input(&mut mailbox.commands, generation, input);
        self.shared.ready.notify_one();
        true
    }

    /// Consume at most one completed frame. This never waits for engine work.
    pub fn poll(&mut self) -> Option<BrowserFrame> {
        let (update, generation, viewport) = {
            let mut mailbox = self.shared.mailbox.lock().unwrap();
            (mailbox.update.take(), mailbox.generation, mailbox.viewport)
        };
        if let Some(update) = update.filter(|update| update.generation == generation) {
            self.snapshot = update.snapshot;
            return update.frame.and_then(|(rendered_viewport, frame)| {
                (rendered_viewport == viewport).then_some(frame)
            });
        }
        if self.worker.is_finished() && !matches!(self.snapshot.load_state, LoadState::Failed(_)) {
            self.snapshot.load_state = LoadState::Failed(String::from("browser worker stopped"));
        }
        None
    }
}

impl Drop for BrowserWorker {
    fn drop(&mut self) {
        self.shared.mailbox.lock().unwrap().shutdown = true;
        self.shared.ready.notify_one();
        // Closing the window must not join a worker currently executing page JS.
    }
}

fn enqueue_input(commands: &mut VecDeque<(u64, Command)>, generation: u64, input: BrowserInput) {
    if let Some((previous_generation, Command::Input(previous))) = commands.back_mut()
        && *previous_generation == generation
    {
        match (previous, &input) {
            (previous @ BrowserInput::PointerMoved { .. }, BrowserInput::PointerMoved { .. }) => {
                *previous = input;
                return;
            }
            (
                BrowserInput::Wheel {
                    delta_x,
                    delta_y,
                    x,
                    y,
                },
                BrowserInput::Wheel {
                    delta_x: dx,
                    delta_y: dy,
                    x: new_x,
                    y: new_y,
                },
            ) if x == new_x && y == new_y => {
                *delta_x += dx;
                *delta_y += dy;
                return;
            }
            _ => {}
        }
    }
    commands.push_back((generation, Command::Input(input)));
}

impl Shared {
    fn publish(
        &self,
        generation: u64,
        snapshot: BrowserSnapshot,
        frame: Option<(BrowserViewport, BrowserFrame)>,
    ) {
        let mut update = Update {
            generation,
            snapshot,
            frame,
        };
        let previous = {
            let mut mailbox = self.mailbox.lock().unwrap();
            if mailbox.shutdown || generation != mailbox.generation {
                return;
            }
            // Keep the last completed frame if this update only changes status.
            if update.frame.is_none()
                && let Some(previous) = &mut mailbox.update
                && previous.generation == generation
            {
                update.frame = previous.frame.take();
            }
            mailbox.update.replace(update)
        };
        // Destruction of obsolete scene data must happen outside the shared lock.
        drop(previous);
    }
}

fn run_engine(mut backend: Box<dyn BrowserBackend>, shared: &Shared) {
    let mut generation = 0;
    let mut dirty = false;
    let mut next_frame_at = Instant::now();
    let mut next_poll_at = Instant::now();
    let mut last_snapshot = None;
    let mut rendered_viewport = None;
    loop {
        let (commands, viewport) = {
            let mut mailbox = shared.mailbox.lock().unwrap();
            loop {
                if mailbox.shutdown {
                    return;
                }
                if !mailbox.commands.is_empty()
                    || mailbox.engine_woken
                    || Instant::now() >= next_poll_at
                    || (dirty && Instant::now() >= next_frame_at)
                {
                    break;
                }
                let deadline = if dirty {
                    next_frame_at.min(next_poll_at)
                } else {
                    next_poll_at
                };
                mailbox = shared
                    .ready
                    .wait_timeout(mailbox, deadline.saturating_duration_since(Instant::now()))
                    .unwrap()
                    .0;
            }
            mailbox.engine_woken = false;
            (std::mem::take(&mut mailbox.commands), mailbox.viewport)
        };

        // A wake asks us to poll asynchronous work. Only the backend can tell
        // whether that work changed the page (a JS timer may change nothing).
        dirty |= !commands.is_empty() || rendered_viewport != Some(viewport);
        for (command_generation, command) in commands {
            match command {
                Command::Input(input) => {
                    if command_generation == generation {
                        dirty |= backend.handle_input(input);
                    }
                }
                navigation => {
                    generation = command_generation;
                    match navigation {
                        Command::Navigate(location) => {
                            if let Err(error) = backend.navigate(&location) {
                                eprintln!("myrica: {error}");
                            }
                        }
                        Command::Reload => backend.reload(),
                        Command::Back => backend.go_back(),
                        Command::Forward => backend.go_forward(),
                        Command::Input(_) => unreachable!(),
                    }
                }
            }
        }
        dirty |= backend.tick();
        next_poll_at = Instant::now() + ENGINE_POLL_INTERVAL;
        let snapshot = backend.snapshot();
        if last_snapshot.as_ref() != Some(&(generation, snapshot.clone())) {
            shared.publish(generation, snapshot.clone(), None);
            last_snapshot = Some((generation, snapshot));
        }
        if dirty && Instant::now() >= next_frame_at {
            next_frame_at = Instant::now() + FRAME_INTERVAL;
            let frame = backend.render(viewport.width, viewport.height, viewport.scale);
            shared.publish(generation, backend.snapshot(), Some((viewport, frame)));
            rendered_viewport = Some(viewport);
            dirty = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::sync::mpsc::{self, Receiver, Sender};

    const VIEWPORT: BrowserViewport = BrowserViewport {
        width: 320,
        height: 240,
        scale: 1.0,
    };

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Stage {
        Init,
        Navigate,
        Tick,
        Render,
        Input,
    }

    struct Gate {
        stage: Stage,
        entered: Sender<()>,
        release: Receiver<()>,
        used: bool,
    }

    impl Gate {
        fn enter(&mut self, stage: Stage) {
            if self.stage == stage && !self.used {
                self.used = true;
                self.entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
    }

    // Rc deliberately makes this backend !Send, just like Blitz/Boa documents.
    struct TestBackend {
        gate: Gate,
        snapshot: BrowserSnapshot,
        dropped: Sender<()>,
        pending: Option<Receiver<String>>,
        revision: u64,
        _thread_local: Rc<()>,
    }

    impl BrowserBackend for TestBackend {
        fn navigate(&mut self, location: &str) -> Result<(), BackendError> {
            self.gate.enter(Stage::Navigate);
            self.snapshot.url = location.into();
            self.snapshot.title = location.into();
            self.snapshot.load_state = LoadState::Ready;
            Ok(())
        }
        fn reload(&mut self) {}
        fn go_back(&mut self) {}
        fn go_forward(&mut self) {}
        fn tick(&mut self) -> bool {
            self.gate.enter(Stage::Tick);
            if let Some(title) = self.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
                self.snapshot.title = title;
                return true;
            }
            false
        }
        fn render(&mut self, width: u32, height: u32, _: f32) -> BrowserFrame {
            self.gate.enter(Stage::Render);
            self.revision += 1;
            let mut frame = BrowserFrame::empty(width, height);
            frame.revision = self.revision;
            frame
        }
        fn handle_input(&mut self, _: BrowserInput) -> bool {
            self.gate.enter(Stage::Input);
            true
        }
        fn snapshot(&self) -> BrowserSnapshot {
            self.snapshot.clone()
        }
    }

    impl Drop for TestBackend {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }

    fn gated_worker(stage: Stage) -> (BrowserWorker, Receiver<()>, Sender<()>, Receiver<()>) {
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (dropped, dropped_rx) = mpsc::channel();
        let worker = BrowserWorker::with_factory("first", VIEWPORT, move |_| {
            let mut gate = Gate {
                stage,
                entered,
                release: release_rx,
                used: false,
            };
            gate.enter(Stage::Init);
            Ok(Box::new(TestBackend {
                gate,
                dropped,
                pending: None,
                revision: 0,
                _thread_local: Rc::new(()),
                snapshot: BrowserSnapshot {
                    backend_name: "test",
                    url: String::new(),
                    title: String::new(),
                    load_state: LoadState::Idle,
                    can_go_back: false,
                    can_go_forward: false,
                    text_input: None,
                },
            }))
        })
        .unwrap();
        (worker, entered_rx, release, dropped_rx)
    }

    #[test]
    fn ui_calls_and_shutdown_do_not_wait_for_any_engine_phase() {
        for stage in [
            Stage::Init,
            Stage::Navigate,
            Stage::Tick,
            Stage::Render,
            Stage::Input,
        ] {
            let start = Instant::now();
            let (mut worker, entered, release, dropped) = gated_worker(stage);
            assert!(
                start.elapsed() < Duration::from_millis(250),
                "startup waited for {stage:?}"
            );
            if stage == Stage::Input {
                worker.handle_input(BrowserInput::Text('a'));
            }
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            let start = Instant::now();
            for index in 0..100 {
                worker.resize(BrowserViewport {
                    width: 400 + index,
                    ..VIEWPORT
                });
                worker.handle_input(BrowserInput::PointerMoved {
                    x: index as f32,
                    y: 20.0,
                });
                worker.poll();
            }
            worker.navigate("second").unwrap();
            assert_eq!(worker.snapshot().url, "second");
            drop(worker);
            let elapsed = start.elapsed();
            release.send(()).unwrap();
            dropped.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(
                elapsed < Duration::from_millis(250),
                "UI blocked during {stage:?}: {elapsed:?}"
            );
        }
    }

    #[test]
    fn delayed_frame_cannot_restore_old_navigation_or_viewport() {
        let (mut worker, entered, release, dropped) = gated_worker(Stage::Render);
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let viewport = BrowserViewport {
            width: 777,
            height: 555,
            scale: 2.0,
        };
        worker.resize(viewport);
        worker.navigate("second").unwrap();
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = worker.poll() {
                assert_eq!((frame.width, frame.height), (777, 555));
                assert_eq!(worker.snapshot().url, "second");
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        drop(worker);
        dropped.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn idle_wakes_do_not_render_but_resize_does() {
        let (mut worker, entered, release, dropped) = gated_worker(Stage::Render);
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.poll().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..10 {
            worker.shared.mailbox.lock().unwrap().engine_woken = true;
            worker.shared.ready.notify_one();
            thread::sleep(Duration::from_millis(20));
            assert!(
                worker.poll().is_none(),
                "a poll wake generated an unchanged frame"
            );
        }
        worker.resize(BrowserViewport {
            width: 200,
            height: 150,
            ..VIEWPORT
        });
        loop {
            if let Some(frame) = worker.poll() {
                assert_eq!((frame.width, frame.height), (200, 150));
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        drop(worker);
        dropped.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn pending_engine_work_reaches_ui_without_another_input_or_notification() {
        let (pending, pending_rx) = mpsc::channel();
        let (entered, _) = mpsc::channel();
        let (_, release) = mpsc::channel();
        let (dropped, dropped_rx) = mpsc::channel();
        let mut worker = BrowserWorker::with_factory("first", VIEWPORT, move |_| {
            Ok(Box::new(TestBackend {
                gate: Gate {
                    stage: Stage::Tick,
                    entered,
                    release,
                    used: true,
                },
                snapshot: BrowserSnapshot {
                    backend_name: "test",
                    url: String::new(),
                    title: String::new(),
                    load_state: LoadState::Idle,
                    can_go_back: false,
                    can_go_forward: false,
                    text_input: None,
                },
                pending: Some(pending_rx),
                revision: 0,
                dropped,
                _thread_local: Rc::new(()),
            }))
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let initial_frame = loop {
            if let Some(frame) = worker.poll() {
                break frame;
            }
            assert!(
                Instant::now() < deadline,
                "initial frame never reached the UI"
            );
            thread::sleep(Duration::from_millis(2));
        };
        // Work becomes available after the worker has gone idle. Its contract
        // is to make progress on tick, even if no event-loop wake was emitted.
        thread::sleep(ENGINE_POLL_INTERVAL * 2);
        pending.send("loaded after idle".into()).unwrap();
        loop {
            if let Some(frame) = worker.poll() {
                assert!(frame.revision > initial_frame.revision);
                assert_eq!(worker.snapshot().title, "loaded after idle");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "engine work stalled waiting for a notification"
            );
            thread::sleep(Duration::from_millis(2));
        }
        drop(worker);
        dropped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn motion_coalescing_preserves_drag_and_text_order() {
        use super::super::{BrowserKey, BrowserModifiers, BrowserMouseButton};
        let mut commands = VecDeque::new();
        let press = BrowserInput::PointerButton {
            button: BrowserMouseButton::Primary,
            pressed: true,
            x: 1.0,
            y: 2.0,
        };
        let release = BrowserInput::PointerButton {
            button: BrowserMouseButton::Primary,
            pressed: false,
            x: 99.0,
            y: 2.0,
        };
        enqueue_input(&mut commands, 1, press.clone());
        for x in 0..100 {
            enqueue_input(
                &mut commands,
                1,
                BrowserInput::PointerMoved {
                    x: x as f32,
                    y: 2.0,
                },
            );
        }
        enqueue_input(&mut commands, 1, release.clone());
        let key = BrowserInput::Key {
            key: BrowserKey::Backspace,
            pressed: true,
            modifiers: BrowserModifiers::default(),
        };
        for input in [
            BrowserInput::Text('a'),
            key.clone(),
            BrowserInput::ImeCommit("日本語".into()),
        ] {
            enqueue_input(&mut commands, 1, input);
        }
        let inputs: Vec<_> = commands
            .into_iter()
            .map(|(_, command)| {
                let Command::Input(input) = command else {
                    panic!("expected input")
                };
                input
            })
            .collect();
        assert_eq!(
            inputs,
            vec![
                press,
                BrowserInput::PointerMoved { x: 99.0, y: 2.0 },
                release,
                BrowserInput::Text('a'),
                key,
                BrowserInput::ImeCommit("日本語".into())
            ]
        );
    }
}
