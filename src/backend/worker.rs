//! The browser engine is confined to one worker; UI calls only exchange messages.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    BackendError, BackendKind, BrowserBackend, BrowserFrame, BrowserFrameSource, BrowserInput,
    BrowserInputSink, BrowserSnapshot, LoadState, WakeCallback, create_backend,
    create_backend_selected,
};

const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
// Before the worker split, Application::on_idle drove tick every UI cycle.
// Preserve that contract: a backend wake is an early-poll hint, not a promise
// that every pending operation will notify us before it can make progress.
const ENGINE_POLL_INTERVAL: Duration = FRAME_INTERVAL;
// Fence collection has no JS/layout work and must not consume a frame deadline.
const GPU_COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

impl BrowserViewport {
    /// Fit backing storage to the producer's limit while keeping CSS dimensions
    /// and input coordinates stable. Normal-sized windows keep their native DPR.
    fn fit_surface(self, maximum: u32) -> Self {
        if maximum == 0
            || self.width.max(self.height) <= maximum
            || !self.scale.is_finite()
            || self.scale <= 0.0
        {
            return self;
        }
        let css_width = (f64::from(self.width) / f64::from(self.scale)).ceil();
        let css_height = (f64::from(self.height) / f64::from(self.scale)).ceil();
        let limit_scale = f64::from(maximum) / css_width.max(css_height);
        let scale = limit_scale as f32;
        if scale < 0.25 {
            return self;
        }
        Self {
            width: (css_width * f64::from(scale))
                .floor()
                .max(1.0)
                .min(f64::from(maximum)) as u32,
            height: (css_height * f64::from(scale))
                .floor()
                .max(1.0)
                .min(f64::from(maximum)) as u32,
            scale,
        }
    }
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
    returned_frames: VecDeque<u64>,
    viewport: BrowserViewport,
    generation: u64,
    engine_woken: bool,
    shutdown: bool,
    shutdown_hook: Option<WakeCallback>,
    update: Option<Update>,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    ready: Condvar,
    frame_source: Mutex<Option<Box<dyn BrowserFrameSource>>>,
    input_sink: Mutex<Option<Arc<dyn BrowserInputSink>>>,
}

/// UI-side proxy. No document, JS runtime, or layout code runs in these methods.
pub struct BrowserWorker {
    shared: Arc<Shared>,
    snapshot: BrowserSnapshot,
    worker: Option<thread::JoinHandle<()>>,
    join_on_shutdown: bool,
    frame_source: Option<Box<dyn BrowserFrameSource>>,
    source_needs_full_damage: Cell<bool>,
}

impl BrowserWorker {
    pub fn new(location: &str, viewport: BrowserViewport) -> Result<Self, BackendError> {
        let kind = BackendKind::default();
        Self::with_factory_named(
            location,
            viewport,
            kind.name(),
            kind == BackendKind::WebKit,
            create_backend,
        )
    }

    pub fn with_backend(
        location: &str,
        viewport: BrowserViewport,
        kind: BackendKind,
    ) -> Result<Self, BackendError> {
        Self::with_factory_named(
            location,
            viewport,
            kind.name(),
            kind == BackendKind::WebKit,
            move |wake| create_backend_selected(kind, wake),
        )
    }

    #[cfg(test)]
    fn with_factory(
        location: &str,
        viewport: BrowserViewport,
        factory: impl FnOnce(WakeCallback) -> Result<Box<dyn BrowserBackend>, BackendError>
        + Send
        + 'static,
    ) -> Result<Self, BackendError> {
        Self::with_factory_named(
            location,
            viewport,
            BackendKind::default().name(),
            false,
            factory,
        )
    }

    fn with_factory_named(
        location: &str,
        viewport: BrowserViewport,
        backend_name: &'static str,
        join_on_shutdown: bool,
        factory: impl FnOnce(WakeCallback) -> Result<Box<dyn BrowserBackend>, BackendError>
        + Send
        + 'static,
    ) -> Result<Self, BackendError> {
        let snapshot = BrowserSnapshot {
            backend_name,
            url: location.to_owned(),
            title: String::from("Myrica"),
            load_state: LoadState::Loading,
            can_go_back: false,
            can_go_forward: false,
            fullscreen: Default::default(),
            text_input: None,
        };
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox {
                commands: VecDeque::from([(1, Command::Navigate(location.to_owned()))]),
                returned_frames: VecDeque::new(),
                viewport,
                generation: 1,
                engine_woken: true,
                shutdown: false,
                shutdown_hook: None,
                update: None,
            }),
            ready: Condvar::new(),
            frame_source: Mutex::new(None),
            input_sink: Mutex::new(None),
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
                    Ok(mut backend) => {
                        *worker_shared.frame_source.lock().unwrap() = backend.take_frame_source();
                        *worker_shared.input_sink.lock().unwrap() = backend.take_input_sink();
                        let weak = Arc::downgrade(&worker_shared);
                        backend.set_frame_releaser(Arc::new(move |token| {
                            if let Some(shared) = weak.upgrade() {
                                let mut mailbox = shared.mailbox.lock().unwrap();
                                if !mailbox.shutdown {
                                    mailbox.returned_frames.push_back(token);
                                    shared.ready.notify_one();
                                }
                            }
                        }));
                        run_engine(backend, &worker_shared);
                    }
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
            worker: Some(worker),
            join_on_shutdown,
            frame_source: None,
            source_needs_full_damage: Cell::new(true),
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
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
        {
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
        let viewport = self
            .frame_source
            .as_ref()
            .and_then(|source| source.maximum_surface_dimension())
            .map_or(viewport, |maximum| viewport.fit_surface(maximum));
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        if mailbox.viewport != viewport {
            self.source_needs_full_damage.set(true);
            mailbox.viewport = viewport;
            mailbox.engine_woken = true;
            self.shared.ready.notify_one();
        }
    }

    pub fn handle_input(&self, input: BrowserInput) -> bool {
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
        {
            return false;
        }
        let mut mailbox = self.shared.mailbox.lock().unwrap();
        let generation = mailbox.generation;
        // Navigation and geometry are checked again by the native endpoint;
        // input cannot slip into an old document while its owner is busy.
        if (!matches!(input, BrowserInput::Wheel { .. })
            || !mailbox
                .commands
                .iter()
                .any(|(_, command)| !matches!(command, Command::Input(_))))
            && self
                .shared
                .input_sink
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|sink| sink.dispatch(&input, generation, mailbox.viewport))
        {
            return true;
        }
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
        let mut frame = None;
        if let Some(update) = update.filter(|update| update.generation == generation) {
            self.snapshot = update.snapshot;
            frame = update.frame.and_then(|(rendered_viewport, frame)| {
                (rendered_viewport == viewport).then_some(frame)
            });
        }
        if self.frame_source.is_none() {
            self.frame_source = self.shared.frame_source.lock().unwrap().take();
        }
        if let Some(source) = &mut self.frame_source {
            match source.poll() {
                Ok(Some(mut completed)) => {
                    if completed.generation == generation && completed.viewport == viewport {
                        if self.source_needs_full_damage.replace(false) {
                            completed.frame.damage = None;
                        }
                        frame = Some(completed.frame);
                    } else {
                        // Native damage is relative to acquisition. A rejected
                        // old epoch/viewport never reached the UI, so the next
                        // accepted image must refresh all previously stale pixels.
                        self.source_needs_full_damage.set(true);
                    }
                }
                Ok(None) => {}
                Err(error) => self.snapshot.load_state = LoadState::Failed(error.to_string()),
            }
        }
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
            && !matches!(self.snapshot.load_state, LoadState::Failed(_))
        {
            self.snapshot.load_state = LoadState::Failed(String::from("browser worker stopped"));
        }
        frame
    }
    /// Close the engine explicitly when the application's event loop ends.
    /// WebKit's owner is joined before native process finalization.
    pub fn shutdown(&mut self) {
        self.shared.mailbox.lock().unwrap().shutdown = true;
        self.shared.ready.notify_one();
        if !self.join_on_shutdown {
            return;
        }
        // WebCore owns C++ process globals. Its Page and owner-thread TLS must
        // be destroyed before main returns and the native CRT finalizes them.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut interrupted = false;
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            if !interrupted {
                let hook = self.shared.mailbox.lock().unwrap().shutdown_hook.clone();
                if let Some(hook) = hook {
                    hook();
                    interrupted = true;
                }
            }
            if Instant::now() >= deadline {
                eprintln!("myrica: WebKit owner did not finish shutdown within 30 seconds");
                // Do not run C++ global finalizers concurrently with a live
                // engine, or report this shutdown as a normal process exit.
                std::process::abort();
            }
            thread::sleep(Duration::from_millis(1));
        }
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                eprintln!("myrica: WebKit owner panicked during shutdown");
                std::process::abort();
            }
        }
    }
}

impl Drop for BrowserWorker {
    fn drop(&mut self) {
        self.shutdown();
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

fn run_engine(backend: Box<dyn BrowserBackend>, shared: &Shared) {
    run_engine_polling(backend, shared, ENGINE_POLL_INTERVAL);
}

fn run_engine_polling(
    mut backend: Box<dyn BrowserBackend>,
    shared: &Shared,
    poll_interval: Duration,
) {
    shared.mailbox.lock().unwrap().shutdown_hook = backend.shutdown_callback();
    let mut generation = 0;
    let mut dirty = false;
    let mut next_frame_at = Instant::now();
    let mut next_poll_at = Instant::now();
    let mut next_completion_at = Instant::now();
    let mut pending_frame = None;
    let mut waiting_for_lease = false;
    let mut last_snapshot = None;
    let mut rendered_viewport = None;
    let mut published_frame = None;
    loop {
        let (commands, returned_frames, viewport, tick_due) = {
            let mut mailbox = shared.mailbox.lock().unwrap();
            loop {
                if mailbox.shutdown {
                    return;
                }
                if !mailbox.returned_frames.is_empty()
                    || !mailbox.commands.is_empty()
                    || mailbox.engine_woken
                    || Instant::now() >= next_poll_at
                    || (pending_frame.is_some() && Instant::now() >= next_completion_at)
                    || (dirty
                        && pending_frame.is_none()
                        && !waiting_for_lease
                        && Instant::now() >= next_frame_at)
                {
                    break;
                }
                let mut deadline = next_poll_at;
                if pending_frame.is_some() {
                    deadline = deadline.min(next_completion_at);
                } else if dirty && !waiting_for_lease {
                    deadline = deadline.min(next_frame_at);
                }
                mailbox = shared
                    .ready
                    .wait_timeout(mailbox, deadline.saturating_duration_since(Instant::now()))
                    .unwrap()
                    .0;
            }
            // A frame deadline or returned GPU lease only makes painting
            // runnable. Neither is evidence of new engine work, and neither
            // should postpone the next scheduled timer/network poll.
            let tick_due = mailbox.engine_woken
                || !mailbox.commands.is_empty()
                || Instant::now() >= next_poll_at;
            mailbox.engine_woken = false;
            (
                std::mem::take(&mut mailbox.commands),
                std::mem::take(&mut mailbox.returned_frames),
                mailbox.viewport,
                tick_due,
            )
        };

        let snapshot_due = tick_due || !commands.is_empty() || !returned_frames.is_empty();
        if !returned_frames.is_empty() {
            waiting_for_lease = false;
        }
        for token in returned_frames {
            backend.release_frame(token);
        }

        // Publish an already-completed image before input handlers or JS can
        // block the owner. Navigation must cancel its obsolete submission first;
        // a concurrent navigation is also rejected by Shared::publish's generation
        // check. Keep the submitted viewport so resize filtering remains on the UI.
        if commands
            .iter()
            .all(|(_, command)| matches!(command, Command::Input(_)))
            && let Some((submitted_generation, submitted_viewport)) = pending_frame
            && (tick_due || Instant::now() >= next_completion_at)
        {
            if let Some(frame) = backend.poll_pending_frame() {
                let identity = (submitted_generation, submitted_viewport, frame.revision);
                if published_frame != Some(identity) {
                    shared.publish(
                        submitted_generation,
                        backend.snapshot(),
                        Some((submitted_viewport, frame)),
                    );
                    published_frame = Some(identity);
                }
            }
            if !backend.has_pending_frame() {
                pending_frame = None;
            }
            next_completion_at = Instant::now() + GPU_COMPLETION_POLL_INTERVAL;
        }

        // A wake asks us to poll asynchronous work. Only the backend can tell
        // whether that work changed the page (a JS timer may change nothing).
        dirty |= rendered_viewport != Some(viewport);
        for (command_generation, command) in commands {
            match command {
                Command::Input(input) => {
                    if command_generation == generation {
                        dirty |= backend.handle_input(input);
                    }
                }
                navigation => {
                    pending_frame = None;
                    generation = command_generation;
                    backend.set_frame_generation(generation);
                    dirty = true;
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
        if tick_due {
            dirty |= backend.tick();
            next_poll_at = Instant::now() + poll_interval;
        }
        if snapshot_due {
            let snapshot = backend.snapshot();
            if last_snapshot
                .as_ref()
                .is_none_or(|(previous_generation, previous)| {
                    *previous_generation != generation || *previous != snapshot
                })
            {
                shared.publish(generation, snapshot.clone(), None);
                last_snapshot = Some((generation, snapshot));
            }
        }
        if dirty && pending_frame.is_none() && !waiting_for_lease && Instant::now() >= next_frame_at
        {
            if backend.can_render() {
                // Charge the submission start, never its completion/collection.
                next_frame_at = Instant::now() + FRAME_INTERVAL;
                if let Some(frame) =
                    backend.render_if_ready(viewport.width, viewport.height, viewport.scale)
                {
                    let identity = (generation, viewport, frame.revision);
                    if published_frame != Some(identity) {
                        shared.publish(generation, backend.snapshot(), Some((viewport, frame)));
                        published_frame = Some(identity);
                    }
                }
                if backend.has_pending_frame() {
                    pending_frame = Some((generation, viewport));
                    next_completion_at = Instant::now() + GPU_COMPLETION_POLL_INTERVAL;
                }
                rendered_viewport = Some(viewport);
                dirty = false;
            } else {
                // Capacity is restored by the UI's returned-frame wake, not by
                // repeated capacity probes or extra engine ticks.
                waiting_for_lease = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::sync::mpsc::{self, Receiver, Sender};

    #[test]
    fn large_retina_viewports_fit_the_gpu_limit_and_keep_css_dimensions() {
        let native = BrowserViewport {
            width: 5120,
            height: 2880,
            scale: 2.0,
        };
        let fitted = native.fit_surface(4096);
        assert_eq!((fitted.width, fitted.height), (4096, 2304));
        assert_eq!((fitted.width as f64 / fitted.scale as f64).ceil(), 2560.0);
        assert_eq!((fitted.height as f64 / fitted.scale as f64).ceil(), 1440.0);
        for scale in [1.0, 1.25, 1.5, 2.0, 2.5] {
            for (width, height) in [(2200, 1400), (5120, 2880), (3000, 6000)] {
                let viewport = BrowserViewport {
                    width,
                    height,
                    scale,
                };
                let fit = viewport.fit_surface(4096);
                assert!(fit.width <= 4096 && fit.height <= 4096);
                assert!(fit.scale > 0.0 && fit.scale <= scale);
                if width.max(height) <= 4096 {
                    assert_eq!(fit, viewport);
                }
            }
        }
    }

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
                    fullscreen: Default::default(),
                    text_input: None,
                },
            }))
        })
        .unwrap();
        (worker, entered_rx, release, dropped_rx)
    }

    struct CompositorFrames(Receiver<super::super::PresentedFrame>);

    impl BrowserFrameSource for CompositorFrames {
        fn poll(&mut self) -> Result<Option<super::super::PresentedFrame>, BackendError> {
            Ok(self.0.try_recv().ok())
        }
    }

    struct ScrollSink(mpsc::Sender<(u64, BrowserViewport)>);
    impl BrowserInputSink for ScrollSink {
        fn dispatch(
            &self,
            input: &BrowserInput,
            generation: u64,
            viewport: BrowserViewport,
        ) -> bool {
            if matches!(input, BrowserInput::Wheel { .. }) && viewport == VIEWPORT {
                self.0.send((generation, viewport)).unwrap();
                true
            } else {
                false
            }
        }
    }

    #[test]
    fn scrolling_bypasses_busy_engine_and_rejected_events_keep_owner_order() {
        let (mut worker, entered, release, dropped) = gated_worker(Stage::Tick);
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (send, receive) = mpsc::channel();
        *worker.shared.input_sink.lock().unwrap() = Some(Arc::new(ScrollSink(send)));
        let wheel = BrowserInput::Wheel {
            delta_x: 0.0,
            delta_y: -40.0,
            x: 20.0,
            y: 30.0,
        };
        worker.handle_input(wheel.clone());
        assert_eq!(receive.try_recv().unwrap(), (1, VIEWPORT));
        assert!(worker.shared.mailbox.lock().unwrap().commands.is_empty());
        worker.handle_input(BrowserInput::Text('a'));
        assert!(matches!(
            worker.shared.mailbox.lock().unwrap().commands.back(),
            Some((1, Command::Input(BrowserInput::Text('a'))))
        ));
        worker.resize(BrowserViewport {
            width: 777,
            ..VIEWPORT
        });
        worker.handle_input(wheel.clone());
        assert!(receive.try_recv().is_err());
        assert!(matches!(
            worker.shared.mailbox.lock().unwrap().commands.back(),
            Some((1, Command::Input(BrowserInput::Wheel { .. })))
        ));
        worker.resize(VIEWPORT);
        worker.navigate("second").unwrap();
        worker.handle_input(wheel);
        assert!(receive.try_recv().is_err());
        assert!(matches!(
            worker.shared.mailbox.lock().unwrap().commands.back(),
            Some((2, Command::Input(BrowserInput::Wheel { .. })))
        ));
        drop(worker);
        release.send(()).unwrap();
        dropped.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn independent_compositor_keeps_presenting_while_engine_tick_is_blocked() {
        let (mut worker, entered, release, dropped) = gated_worker(Stage::Tick);
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (send, receive) = mpsc::channel();
        *worker.shared.frame_source.lock().unwrap() = Some(Box::new(CompositorFrames(receive)));
        let present = |generation, viewport: BrowserViewport, revision| {
            let mut frame = BrowserFrame::empty(viewport.width, viewport.height);
            frame.revision = revision;
            frame.damage = Some([4, 5, 8, 9]);
            send.send(super::super::PresentedFrame {
                generation,
                viewport,
                frame,
            })
            .unwrap();
        };
        for revision in 1..=8 {
            present(1, VIEWPORT, revision);
            let frame = worker.poll().unwrap();
            assert_eq!(frame.revision, revision);
            assert_eq!(frame.damage, (revision != 1).then_some([4, 5, 8, 9]));
        }
        // Navigation and resize take effect immediately on the UI, even though
        // the blocked owner cannot yet consume either command.
        worker.navigate("second").unwrap();
        present(1, VIEWPORT, 9);
        assert!(worker.poll().is_none());
        present(2, VIEWPORT, 10);
        let frame = worker.poll().unwrap();
        assert_eq!(frame.revision, 10);
        assert_eq!(frame.damage, None);
        let resized = BrowserViewport {
            width: 777,
            ..VIEWPORT
        };
        worker.resize(resized);
        present(2, VIEWPORT, 11);
        assert!(worker.poll().is_none());
        present(2, resized, 12);
        let frame = worker.poll().unwrap();
        assert_eq!(frame.width, 777);
        assert_eq!(frame.damage, None);
        drop(worker);
        release.send(()).unwrap();
        dropped.recv_timeout(Duration::from_secs(5)).unwrap();
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
                    fullscreen: Default::default(),
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
    fn blocked_frame_deadlines_and_returned_images_do_not_poll_the_engine() {
        #[derive(Debug, PartialEq)]
        enum Signal {
            Tick,
            Capacity,
            Released(u64),
            Snapshot(u32),
        }
        struct BlockedBackend {
            signals: Sender<Signal>,
            ticks: u32,
            released: bool,
        }
        impl BrowserBackend for BlockedBackend {
            fn navigate(&mut self, _: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn reload(&mut self) {}
            fn go_back(&mut self) {}
            fn go_forward(&mut self) {}
            fn tick(&mut self) -> bool {
                self.ticks += 1;
                self.signals.send(Signal::Tick).unwrap();
                false
            }
            fn can_render(&mut self) -> bool {
                self.signals.send(Signal::Capacity).unwrap();
                false
            }
            fn release_frame(&mut self, token: u64) {
                self.released = true;
                self.signals.send(Signal::Released(token)).unwrap();
            }
            fn render(&mut self, _: u32, _: u32, _: f32) -> BrowserFrame {
                panic!("cannot paint while both GPU images are leased")
            }
            fn handle_input(&mut self, _: BrowserInput) -> bool {
                false
            }
            fn snapshot(&self) -> BrowserSnapshot {
                if self.released {
                    self.signals.send(Signal::Snapshot(self.ticks)).unwrap();
                }
                BrowserSnapshot {
                    backend_name: "test",
                    url: String::new(),
                    title: String::new(),
                    load_state: LoadState::Ready,
                    can_go_back: false,
                    can_go_forward: false,
                    fullscreen: Default::default(),
                    text_input: None,
                }
            }
        }
        let (signals, received) = mpsc::channel();
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox {
                commands: VecDeque::from([(1, Command::Navigate("first".into()))]),
                returned_frames: VecDeque::new(),
                viewport: VIEWPORT,
                generation: 1,
                engine_woken: true,
                shutdown: false,
                shutdown_hook: None,
                update: None,
            }),
            ready: Condvar::new(),
            frame_source: Mutex::new(None),
            input_sink: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            // Keep periodic polling beyond the assertion window, so this
            // checks actual wake reasons rather than scheduler timing jitter.
            run_engine_polling(
                Box::new(BlockedBackend {
                    signals,
                    ticks: 0,
                    released: false,
                }),
                &worker_shared,
                Duration::from_secs(5),
            );
        });
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Signal::Tick
        );
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Signal::Capacity
        );
        // No repeated capacity probes, rendering, or ticks while lease-blocked.
        assert_eq!(
            received.recv_timeout(FRAME_INTERVAL * 3),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        shared.mailbox.lock().unwrap().returned_frames.push_back(7);
        shared.ready.notify_one();
        loop {
            match received.recv_timeout(Duration::from_secs(2)).unwrap() {
                Signal::Capacity | Signal::Released(7) => {}
                Signal::Snapshot(ticks) => {
                    assert_eq!(ticks, 1);
                    break;
                }
                other => panic!("a paint-only wake polled the engine: {other:?}"),
            }
        }
        shared.mailbox.lock().unwrap().engine_woken = true;
        shared.ready.notify_one();
        loop {
            match received.recv_timeout(Duration::from_secs(2)).unwrap() {
                Signal::Tick => break,
                Signal::Capacity | Signal::Snapshot(1) => {}
                other => panic!("unexpected wake result: {other:?}"),
            }
        }
        shared.mailbox.lock().unwrap().shutdown = true;
        shared.ready.notify_one();
        worker.join().unwrap();
    }

    #[derive(Debug, PartialEq)]
    enum AsyncSignal {
        Tick,
        Submit(u64),
        Poll,
        Complete(u64),
        Snapshot,
        Input,
        Navigation,
    }

    // Models only the asynchronous scheduling protocol. No pixels/device/GPU
    // success is manufactured or asserted by these worker tests.
    struct AsyncBackend {
        signals: Sender<AsyncSignal>,
        completed: Receiver<()>,
        pending: Option<(u64, u32, u32)>,
        revision: u64,
        ticks: u32,
        render_gate: Option<Receiver<()>>,
        tick_gate: Option<Receiver<()>>,
        input_gate: Option<Receiver<()>>,
    }
    impl AsyncBackend {
        fn cancel_for_navigation(&mut self) {
            self.pending = None;
            while self.completed.try_recv().is_ok() {}
            self.signals.send(AsyncSignal::Navigation).unwrap();
        }
    }
    impl BrowserBackend for AsyncBackend {
        fn navigate(&mut self, _: &str) -> Result<(), BackendError> {
            self.cancel_for_navigation();
            Ok(())
        }
        fn reload(&mut self) {
            self.cancel_for_navigation();
        }
        fn go_back(&mut self) {
            self.cancel_for_navigation();
        }
        fn go_forward(&mut self) {
            self.cancel_for_navigation();
        }
        fn tick(&mut self) -> bool {
            self.ticks += 1;
            self.signals.send(AsyncSignal::Tick).unwrap();
            if self.ticks == 2
                && let Some(gate) = self.tick_gate.take()
            {
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            false
        }
        fn render(&mut self, _: u32, _: u32, _: f32) -> BrowserFrame {
            panic!("asynchronous backend requires submit/collect")
        }
        fn render_if_ready(&mut self, width: u32, height: u32, _: f32) -> Option<BrowserFrame> {
            assert!(
                self.pending.is_none(),
                "paint must not overwrite an in-flight target"
            );
            self.revision += 1;
            self.pending = Some((self.revision, width, height));
            self.signals
                .send(AsyncSignal::Submit(self.revision))
                .unwrap();
            if let Some(gate) = self.render_gate.take() {
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            None
        }
        fn has_pending_frame(&mut self) -> bool {
            self.pending.is_some()
        }
        fn poll_pending_frame(&mut self) -> Option<BrowserFrame> {
            self.signals.send(AsyncSignal::Poll).unwrap();
            if self.completed.try_recv().is_err() {
                return None;
            }
            let (revision, width, height) = self.pending.take().unwrap();
            self.signals.send(AsyncSignal::Complete(revision)).unwrap();
            let mut frame = BrowserFrame::empty(width, height);
            frame.revision = revision;
            Some(frame)
        }
        fn handle_input(&mut self, _: BrowserInput) -> bool {
            self.signals.send(AsyncSignal::Input).unwrap();
            if let Some(gate) = self.input_gate.take() {
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            true
        }
        fn snapshot(&self) -> BrowserSnapshot {
            self.signals.send(AsyncSignal::Snapshot).unwrap();
            BrowserSnapshot {
                backend_name: "scheduling-protocol",
                url: String::new(),
                title: String::new(),
                load_state: LoadState::Ready,
                can_go_back: false,
                can_go_forward: false,
                fullscreen: Default::default(),
                text_input: None,
            }
        }
    }
    fn async_worker() -> (
        Arc<Shared>,
        thread::JoinHandle<()>,
        Receiver<AsyncSignal>,
        Sender<()>,
    ) {
        async_worker_with_gates(None, None, None)
    }
    fn async_worker_with_gates(
        render_gate: Option<Receiver<()>>,
        tick_gate: Option<Receiver<()>>,
        input_gate: Option<Receiver<()>>,
    ) -> (
        Arc<Shared>,
        thread::JoinHandle<()>,
        Receiver<AsyncSignal>,
        Sender<()>,
    ) {
        let (signals, received) = mpsc::channel();
        let (complete, completed) = mpsc::channel();
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox {
                commands: VecDeque::from([(1, Command::Navigate("first".into()))]),
                returned_frames: VecDeque::new(),
                viewport: VIEWPORT,
                generation: 1,
                engine_woken: true,
                shutdown: false,
                shutdown_hook: None,
                update: None,
            }),
            ready: Condvar::new(),
            frame_source: Mutex::new(None),
            input_sink: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            run_engine_polling(
                Box::new(AsyncBackend {
                    signals,
                    completed,
                    pending: None,
                    revision: 0,
                    ticks: 0,
                    render_gate,
                    tick_gate,
                    input_gate,
                }),
                &worker_shared,
                Duration::from_secs(5),
            )
        });
        (shared, worker, received, complete)
    }
    fn stop_test_worker(shared: &Shared, worker: thread::JoinHandle<()>) {
        shared.mailbox.lock().unwrap().shutdown = true;
        shared.ready.notify_one();
        worker.join().unwrap();
    }
    fn await_async(received: &Receiver<AsyncSignal>, expected: AsyncSignal) {
        loop {
            if received.recv_timeout(Duration::from_secs(2)).unwrap() == expected {
                return;
            }
        }
    }

    #[test]
    fn completed_frame_is_published_before_a_blocked_engine_tick() {
        let (resume_submit, submit_gate) = mpsc::channel();
        let (resume_tick, tick_gate) = mpsc::channel();
        let (shared, worker, received, complete) =
            async_worker_with_gates(Some(submit_gate), Some(tick_gate), None);
        await_async(&received, AsyncSignal::Submit(1));
        // The owner is still inside the first submit. Queue completion and a
        // timer/network wake together before allowing its next iteration.
        shared.mailbox.lock().unwrap().engine_woken = true;
        complete.send(()).unwrap();
        resume_submit.send(()).unwrap();
        await_async(&received, AsyncSignal::Tick);
        // Tick is blocked, just as a long native JS callback would block it.
        // This observes actual worker publication, not an elapsed-time target.
        let published = {
            let mailbox = shared.mailbox.lock().unwrap();
            mailbox
                .update
                .as_ref()
                .and_then(|update| update.frame.as_ref())
                .map(|(viewport, frame)| (*viewport, frame.revision))
        };
        resume_tick.send(()).unwrap();
        stop_test_worker(&shared, worker);
        assert_eq!(published, Some((VIEWPORT, 1)));
    }

    #[test]
    fn completed_frame_is_published_before_a_blocked_input_handler() {
        let (resume_submit, submit_gate) = mpsc::channel();
        let (resume_input, input_gate) = mpsc::channel();
        let (shared, worker, received, complete) =
            async_worker_with_gates(Some(submit_gate), None, Some(input_gate));
        await_async(&received, AsyncSignal::Submit(1));
        shared.mailbox.lock().unwrap().commands.push_back((
            1,
            Command::Input(BrowserInput::Wheel {
                delta_x: 0.0,
                delta_y: 32.0,
                x: 10.0,
                y: 10.0,
            }),
        ));
        complete.send(()).unwrap();
        resume_submit.send(()).unwrap();
        await_async(&received, AsyncSignal::Input);
        // The handler is blocked, not merely slow: the UI must already be able
        // to consume the completed frame without finishing this input or tick.
        let published = {
            let mailbox = shared.mailbox.lock().unwrap();
            mailbox
                .update
                .as_ref()
                .and_then(|update| update.frame.as_ref())
                .map(|(viewport, frame)| (*viewport, frame.revision))
        };
        resume_input.send(()).unwrap();
        stop_test_worker(&shared, worker);
        assert_eq!(published, Some((VIEWPORT, 1)));
    }

    #[test]
    fn navigation_cancels_completed_frames_before_input_publication() {
        for navigation in [
            Command::Navigate("second".into()),
            Command::Reload,
            Command::Back,
            Command::Forward,
        ] {
            let (resume_submit, submit_gate) = mpsc::channel();
            let (resume_input, input_gate) = mpsc::channel();
            let (shared, worker, received, complete) =
                async_worker_with_gates(Some(submit_gate), None, Some(input_gate));
            await_async(&received, AsyncSignal::Submit(1));
            {
                let mut mailbox = shared.mailbox.lock().unwrap();
                mailbox.generation = 2;
                mailbox.update = None;
                mailbox.commands.push_back((2, navigation));
                mailbox
                    .commands
                    .push_back((2, Command::Input(BrowserInput::Text('x'))));
            }
            complete.send(()).unwrap();
            resume_submit.send(()).unwrap();
            let mut polled_obsolete_frame = false;
            loop {
                match received.recv_timeout(Duration::from_secs(2)).unwrap() {
                    AsyncSignal::Input => break,
                    AsyncSignal::Poll | AsyncSignal::Complete(1) => polled_obsolete_frame = true,
                    _ => {}
                }
            }
            let published = shared
                .mailbox
                .lock()
                .unwrap()
                .update
                .as_ref()
                .is_some_and(|update| update.frame.is_some());
            resume_input.send(()).unwrap();
            stop_test_worker(&shared, worker);
            assert!(
                !polled_obsolete_frame,
                "navigation must cancel before collection"
            );
            assert!(!published, "an obsolete generation must not reach the UI");
        }
    }

    #[test]
    fn delayed_submission_collection_does_not_tick_or_charge_another_frame_interval() {
        let (shared, worker, received, complete) = async_worker();
        await_async(&received, AsyncSignal::Submit(1));
        // A real input invalidates the page while the previous submission is
        // pending. Hold that submission beyond its existing frame deadline.
        shared
            .mailbox
            .lock()
            .unwrap()
            .commands
            .push_back((1, Command::Input(BrowserInput::Text('x'))));
        shared.ready.notify_one();
        await_async(&received, AsyncSignal::Tick);
        await_async(&received, AsyncSignal::Snapshot);
        thread::sleep(FRAME_INTERVAL * 2);
        complete.send(()).unwrap();
        loop {
            match received.recv_timeout(Duration::from_secs(2)).unwrap() {
                AsyncSignal::Poll => {}
                AsyncSignal::Complete(1) => break,
                other => panic!("completion-only polling ran engine work: {other:?}"),
            }
        }
        // Publishing the completed frame takes one snapshot. The next submit
        // must then happen in this same worker iteration, without another
        // snapshot/tick/deadline wait before it.
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            AsyncSignal::Snapshot
        );
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            AsyncSignal::Submit(2)
        );
        stop_test_worker(&shared, worker);
    }

    #[test]
    fn pending_completion_keeps_the_submitted_viewport_after_resize() {
        let (shared, worker, received, complete) = async_worker();
        await_async(&received, AsyncSignal::Submit(1));
        let resized = BrowserViewport {
            width: 640,
            height: 480,
            ..VIEWPORT
        };
        {
            let mut mailbox = shared.mailbox.lock().unwrap();
            mailbox.viewport = resized;
            mailbox.engine_woken = true;
        }
        shared.ready.notify_one();
        await_async(&received, AsyncSignal::Tick);
        await_async(&received, AsyncSignal::Snapshot);
        complete.send(()).unwrap();
        await_async(&received, AsyncSignal::Complete(1));
        await_async(&received, AsyncSignal::Submit(2));
        {
            let mailbox = shared.mailbox.lock().unwrap();
            let (submitted, frame) = mailbox.update.as_ref().unwrap().frame.as_ref().unwrap();
            assert_eq!(*submitted, VIEWPORT);
            assert_ne!(*submitted, mailbox.viewport);
            assert_eq!(
                (frame.width, frame.height),
                (VIEWPORT.width, VIEWPORT.height)
            );
        }
        complete.send(()).unwrap();
        await_async(&received, AsyncSignal::Complete(2));
        await_async(&received, AsyncSignal::Snapshot);
        // Wait for publication, using the worker mutex rather than a timing delay.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mailbox = shared.mailbox.lock().unwrap();
            let (submitted, frame) = mailbox.update.as_ref().unwrap().frame.as_ref().unwrap();
            if *submitted == resized {
                assert_eq!((frame.width, frame.height), (640, 480));
                break;
            }
            drop(mailbox);
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        stop_test_worker(&shared, worker);
    }

    #[test]
    fn collected_submission_stops_completion_polling_when_idle() {
        let (shared, worker, received, complete) = async_worker();
        await_async(&received, AsyncSignal::Submit(1));
        complete.send(()).unwrap();
        await_async(&received, AsyncSignal::Complete(1));
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            AsyncSignal::Snapshot
        );
        assert_eq!(
            received.recv_timeout(Duration::from_millis(40)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        stop_test_worker(&shared, worker);
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
