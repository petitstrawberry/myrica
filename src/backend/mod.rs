//! Browser-engine boundary used by the Myrica application shell.

use std::fmt;
use std::sync::Arc;

mod frame;
mod frame_lease;
mod worker;

pub use frame::BrowserFrame;
pub(crate) use frame::{BrowserDraw, BrowserTexture, BrowserTextureSource, GpuImageCache};
pub(crate) use frame_lease::{FrameLease, PresentationLeases};
pub use worker::{BrowserViewport, BrowserWorker};

#[cfg(feature = "backend-blitz")]
mod blitz;

#[cfg(feature = "backend-blitz")]
pub use blitz::BlitzBackend;

#[cfg(feature = "backend-webkit")]
mod webkit;

// Exercise the texture cache without linking the native C++ engine on the host.
#[cfg(all(test, not(feature = "backend-webkit")))]
mod webkit {
    mod frame;
    mod network;
    mod pixels;
}

#[cfg(feature = "backend-webkit")]
pub use webkit::WebKitBackend;

/// Engine selected for this browser window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Blitz,
    WebKit,
}

impl BackendKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Blitz => "Blitz",
            Self::WebKit => "WebKit",
        }
    }

    pub fn parse(name: &str) -> Result<Self, BackendError> {
        let kind = match name {
            "blitz" => Self::Blitz,
            "webkit" => Self::WebKit,
            _ => {
                return Err(BackendError::new(format!(
                    "unknown browser backend: {name}"
                )));
            }
        };
        if kind.enabled() {
            Ok(kind)
        } else {
            Err(BackendError::new(format!(
                "{} backend was not enabled at build time",
                kind.name()
            )))
        }
    }

    pub const fn enabled(self) -> bool {
        match self {
            Self::Blitz => cfg!(feature = "backend-blitz"),
            Self::WebKit => cfg!(feature = "backend-webkit"),
        }
    }
}

impl Default for BackendKind {
    fn default() -> Self {
        if cfg!(feature = "backend-blitz") {
            Self::Blitz
        } else {
            Self::WebKit
        }
    }
}

/// Thread-safe callback used by a backend to request another application tick.
pub type WakeCallback = Arc<dyn Fn() + Send + Sync + 'static>;

/// Current top-level document loading state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    /// No top-level navigation is active.
    Idle,
    /// A top-level document is being fetched.
    Loading,
    /// The current top-level document loaded successfully.
    Ready,
    /// The current top-level document could not be loaded completely.
    Failed(String),
}

impl LoadState {
    /// Return a concise user-facing status label.
    pub fn label(&self) -> String {
        match self {
            Self::Idle => String::from("Idle"),
            Self::Loading => String::from("Loading…"),
            Self::Ready => String::from("Ready"),
            Self::Failed(message) => format!("Failed: {message}"),
        }
    }
}

/// Native fullscreen transition requested by the document owner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BrowserFullscreen {
    pub request_id: u64,
    pub requested: bool,
    pub active: bool,
}

/// Browser state exposed to chrome without leaking engine-specific types.
#[derive(Clone, Debug, PartialEq)]
pub struct BrowserSnapshot {
    /// Human-readable backend name.
    pub backend_name: &'static str,
    /// Current or pending top-level URL.
    pub url: String,
    /// Current document title.
    pub title: String,
    /// Current loading state.
    pub load_state: LoadState,
    /// Whether backward navigation is currently available.
    pub can_go_back: bool,
    /// Whether forward navigation is currently available.
    pub can_go_forward: bool,
    /// IME state of the page's focused editable control, in viewport CSS pixels.
    pub text_input: Option<BrowserTextInput>,
    pub fullscreen: BrowserFullscreen,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserTextInput {
    pub cursor_rect: [f32; 4],
    pub surrounding_text: String,
    pub cursor_byte: u32,
    pub anchor_byte: u32,
}

/// Mouse buttons understood by browser backends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserMouseButton {
    /// Primary button.
    Primary,
    /// Middle or auxiliary button.
    Auxiliary,
    /// Secondary button.
    Secondary,
}

/// Backend-neutral keyboard key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserKey {
    Escape,
    Enter,
    Tab,
    Backspace,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Character(char),
    Unknown,
}

/// Modifier state accompanying a backend-neutral keyboard event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BrowserModifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// Input delivered to the embedded web view.
#[derive(Clone, Debug, PartialEq)]
pub enum BrowserInput {
    PointerMoved {
        x: f32,
        y: f32,
    },
    PointerExited,
    PointerButton {
        button: BrowserMouseButton,
        pressed: bool,
        x: f32,
        y: f32,
    },
    Wheel {
        delta_x: f64,
        delta_y: f64,
        x: f32,
        y: f32,
    },
    Key {
        key: BrowserKey,
        pressed: bool,
        modifiers: BrowserModifiers,
    },
    Text(char),
    ImePreedit {
        text: String,
        cursor: usize,
        anchor: usize,
    },
    ImeCommit(String),
    FocusLost,
    /// Window-server acknowledgement. Zero identifies a forced platform exit.
    FullscreenChanged {
        request_id: u64,
        fullscreen: bool,
    },
    ExitFullscreen,
}

/// Error returned when a browser backend rejects an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendError(String);

impl BackendError {
    /// Create a backend error with the supplied message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BackendError {}

/// A completed compositor frame, independent of document execution.
pub struct PresentedFrame {
    pub generation: u64,
    pub viewport: BrowserViewport,
    pub frame: BrowserFrame,
}

/// This endpoint may exchange completed images and leases only. It must never
/// enter the document, execute script, or wait for the engine/GPU.
pub trait BrowserFrameSource: Send {
    /// Immutable producer limit, queried on construction rather than per frame.
    fn maximum_surface_dimension(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> Result<Option<PresentedFrame>, BackendError>;
}

/// Optional input routing independent of the document owner. An accepted event
/// must not also be enqueued on the owner. False preserves the ordinary path.
pub trait BrowserInputSink: Send + Sync {
    fn dispatch(&self, input: &BrowserInput, generation: u64, viewport: BrowserViewport) -> bool;
}

/// Narrow WebView-like interface implemented by each embedded browser engine.
pub trait BrowserBackend {
    fn take_input_sink(&mut self) -> Option<Arc<dyn BrowserInputSink>> {
        None
    }

    fn take_frame_source(&mut self) -> Option<Box<dyn BrowserFrameSource>> {
        None
    }

    fn set_frame_generation(&mut self, _generation: u64) {}

    /// Optional thread-safe interruption used before joining a native engine
    /// during shutdown. The callback must not enter owner-only engine APIs.
    fn shutdown_callback(&self) -> Option<WakeCallback> {
        None
    }

    fn set_frame_releaser(&mut self, _release: Arc<dyn Fn(u64) + Send + Sync>) {}
    fn release_frame(&mut self, _token: u64) {}
    fn can_render(&mut self) -> bool {
        true
    }
    fn render_if_ready(&mut self, width: u32, height: u32, scale: f32) -> Option<BrowserFrame> {
        Some(self.render(width, height, scale))
    }

    /// Whether an earlier render has an uncollected asynchronous submission.
    /// This query must neither tick the engine nor begin another paint.
    fn has_pending_frame(&mut self) -> bool {
        false
    }

    /// Collect only an earlier submission, without starting layout or painting.
    fn poll_pending_frame(&mut self) -> Option<BrowserFrame> {
        None
    }

    /// Start a new top-level navigation.
    fn navigate(&mut self, location: &str) -> Result<(), BackendError>;

    /// Reload the current top-level document.
    fn reload(&mut self);

    /// Navigate to the previous history entry when one exists.
    fn go_back(&mut self);

    /// Navigate to the next history entry when one exists.
    fn go_forward(&mut self);

    /// Advance engine work queued since the last application tick.
    ///
    /// Returns `true` when externally visible state changed.
    fn tick(&mut self) -> bool;

    /// Build a retained SGFX frame for the current web view.
    fn render(&mut self, width: u32, height: u32, scale: f32) -> BrowserFrame;

    /// Deliver input localized to the web view.
    ///
    /// Returns `true` when the event was consumed.
    fn handle_input(&mut self, input: BrowserInput) -> bool;

    /// Return browser state suitable for chrome and diagnostics.
    fn snapshot(&self) -> BrowserSnapshot;
}

/// Construct the backend selected at compile time.
pub fn create_backend(wake: WakeCallback) -> Result<Box<dyn BrowserBackend>, BackendError> {
    create_backend_selected(BackendKind::default(), wake)
}

pub fn create_backend_selected(
    kind: BackendKind,
    wake: WakeCallback,
) -> Result<Box<dyn BrowserBackend>, BackendError> {
    match kind {
        #[cfg(feature = "backend-blitz")]
        BackendKind::Blitz => Ok(Box::new(BlitzBackend::new(wake)?)),
        #[cfg(feature = "backend-webkit")]
        BackendKind::WebKit => Ok(Box::new(WebKitBackend::new(wake)?)),
        #[allow(unreachable_patterns)]
        _ => Err(BackendError::new(format!(
            "{} backend was not enabled at build time",
            kind.name()
        ))),
    }
}
