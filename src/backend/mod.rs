//! Browser-engine boundary used by the Myrica application shell.

use std::fmt;
use std::sync::Arc;

mod frame;
mod worker;

pub use frame::BrowserFrame;
pub(crate) use frame::{BrowserDraw, BrowserTexture};
pub use worker::{BrowserViewport, BrowserWorker};

#[cfg(feature = "backend-blitz")]
mod blitz;

#[cfg(feature = "backend-blitz")]
pub use blitz::BlitzBackend;

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

/// Narrow WebView-like interface implemented by each embedded browser engine.
pub trait BrowserBackend {
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
    #[cfg(feature = "backend-blitz")]
    {
        return Ok(Box::new(BlitzBackend::new(wake)?));
    }

    #[allow(unreachable_code)]
    Err(BackendError::new("no browser backend is enabled"))
}
