//! ScarletUI application shell for Myrica.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use scarlet_ui::element::TextInputElementState;
use scarlet_ui::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent};
use scarlet_ui::graphics;
use scarlet_ui::prelude::*;
use scarlet_ui::{
    Application, ComponentElement, Element, Listenable, SgfxCanvas, SgfxCanvasFrame,
    SgfxCanvasHandle, Size, View, Window, WindowContentLayout, WindowContext, WindowDecoration,
    generate_state_id,
};
use scarlet_ui::{hstack, vstack};

use crate::backend::{
    BrowserFrame, BrowserInput, BrowserKey, BrowserModifiers, BrowserMouseButton, BrowserSnapshot,
    BrowserViewport, BrowserWorker, WakeCallback,
};

const WINDOW_WIDTH: f32 = 1_100.0;
const WINDOW_HEIGHT: f32 = 760.0;
const BROWSER_CHROME_HEIGHT: f32 = 86.0;

/// Myrica's browser chrome and selected embedded engine.
#[derive(Clone)]
pub struct MyricaApp {
    backend: Rc<RefCell<BrowserWorker>>,
    address: State<String>,
    snapshot: State<BrowserSnapshot>,
    repaint_revision: State<u64>,
    canvas_handle: SgfxCanvasHandle,
    canvas_frame: State<Arc<SgfxCanvasFrame>>,
    webview_size: State<Size>,
    webview_focused: State<bool>,
    webview_text_input: State<Option<TextInputElementState>>,
    last_webview_focused: Rc<Cell<bool>>,
    last_backend_url: Rc<RefCell<String>>,
}

impl MyricaApp {
    /// Construct the browser and begin loading its initial location.
    pub fn new(initial_location: &str) -> std::result::Result<Self, String> {
        let repaint_revision = State::new(generate_state_id(), 0_u64);
        let wake_revision = repaint_revision.clone();
        let wake: WakeCallback = Arc::new(move || {
            wake_revision.update(|revision| *revision = revision.wrapping_add(1));
        });

        let webview_size = webview_size_for_window(WINDOW_WIDTH, WINDOW_HEIGHT);
        let viewport = viewport_for_size(webview_size);
        let backend = BrowserWorker::new(initial_location, viewport, wake)
            .map_err(|error| error.to_string())?;
        let initial_snapshot = backend.snapshot();
        let initial_url = initial_snapshot.url.clone();
        let initial_frame =
            BrowserFrame::empty(viewport.width, viewport.height).into_canvas_frame();

        Ok(Self {
            backend: Rc::new(RefCell::new(backend)),
            address: State::new(generate_state_id(), initial_url.clone()),
            snapshot: State::new(generate_state_id(), initial_snapshot),
            repaint_revision,
            canvas_handle: SgfxCanvasHandle::new(),
            canvas_frame: State::new(generate_state_id(), initial_frame),
            webview_size: State::new(generate_state_id(), webview_size),
            webview_focused: State::new(generate_state_id(), false),
            webview_text_input: State::new(generate_state_id(), None),
            last_webview_focused: Rc::new(Cell::new(false)),
            last_backend_url: Rc::new(RefCell::new(initial_url)),
        })
    }

    fn content(&self) -> impl View + Clone + use<> {
        let snapshot = self.snapshot.get();

        let back_backend = Rc::clone(&self.backend);
        let forward_backend = Rc::clone(&self.backend);
        let reload_backend = Rc::clone(&self.backend);
        let submit_backend = Rc::clone(&self.backend);
        let submit_address = self.address.clone();
        let go_backend = Rc::clone(&self.backend);
        let go_address = self.address.clone();

        let header = HeaderBar::new(
            hstack! {
                Button::icon_only(Icon::ArrowLeft)
                    .header_style()
                    .on_click(move || back_backend.borrow_mut().go_back()),
                Button::icon_only(Icon::ArrowRight)
                    .header_style()
                    .on_click(move || forward_backend.borrow_mut().go_forward()),
                Button::icon_only(Icon::Refresh)
                    .header_style()
                    .on_click(move || reload_backend.borrow_mut().reload()),
                TextField::new(self.address.clone())
                    .placeholder("Enter an HTTP(S) address")
                    .blur_on_submit(true)
                    .on_submit(move || navigate_from_address(&submit_backend, &submit_address))
                    .frame_width(f32::INFINITY),
                Button::new("Go")
                    .header_style()
                    .on_click(move || navigate_from_address(&go_backend, &go_address)),
                Text::new(snapshot.backend_name).font_size(12.0),
            }
            .spacing(8.0)
            .padding(10.0),
        )
        .height(56.0);

        let event_backend = Rc::clone(&self.backend);
        let webview_size = self.webview_size.get();
        let canvas = SgfxCanvas::from_state(
            self.canvas_handle,
            webview_size.width,
            webview_size.height,
            self.canvas_frame.clone(),
        )
        .on_event(move |event| dispatch_webview_event(&event_backend, event))
        .focusable(self.webview_focused.clone())
        .text_input(self.webview_text_input.clone())
        .frame(f32::INFINITY, f32::INFINITY);

        let status = Text::new(format!(
            "{} · {}",
            snapshot.load_state.label(),
            snapshot.url
        ))
        .font_size(12.0)
        .padding_insets(EdgeInsets::symmetric(6.0, 10.0))
        .frame(f32::INFINITY, 30.0);

        vstack! {
            header,
            canvas.background(Color::rgb(247, 245, 242)),
            status,
        }
        .frame(f32::INFINITY, f32::INFINITY)
    }

    fn synchronize_backend_state(&self) {
        let focused = self.webview_focused.get();
        if self.last_webview_focused.replace(focused) && !focused {
            self.backend.borrow().handle_input(BrowserInput::FocusLost);
        }
        let frame = {
            let mut backend = self.backend.borrow_mut();
            backend.resize(viewport_for_size(self.webview_size.get()));
            backend.poll()
        };
        if let Some(frame) = frame {
            self.canvas_frame.set(frame.into_canvas_frame());
        }
        let latest = self.backend.borrow().snapshot();
        if latest.text_input != self.snapshot.get().text_input {
            self.webview_text_input
                .set(latest.text_input.as_ref().map(|input| {
                    let [x, y, width, height] = input.cursor_rect;
                    TextInputElementState {
                        cursor_rect: scarlet_ui::Rect::new(
                            scarlet_ui::Point::new(x, y),
                            Size::new(width, height),
                        ),
                        surrounding_text: input.surrounding_text.clone(),
                        cursor_byte: input.cursor_byte,
                        anchor_byte: input.anchor_byte,
                    }
                }));
        }
        if latest != self.snapshot.get() {
            self.snapshot.set(latest.clone());
        }

        let mut last_url = self.last_backend_url.borrow_mut();
        if latest.url != *last_url {
            self.address.set(latest.url.clone());
            *last_url = latest.url;
        }
    }
}

impl View for MyricaApp {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(ComponentElement::new(self.clone()))
    }

    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.address,
            &self.snapshot,
            &self.repaint_revision,
            &self.canvas_frame,
            &self.webview_size,
            &self.webview_focused,
        ]
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Application for MyricaApp {
    fn scenes(&self) -> impl Scene {
        let snapshot = self.snapshot.get();
        let window_title = if snapshot.title.is_empty() {
            String::from("Myrica")
        } else {
            format!("{} — Myrica", snapshot.title)
        };
        Window::new(window_title, self.content())
            .app_id("org.scarlet-os.myrica")
            .size(Size::new(WINDOW_WIDTH, WINDOW_HEIGHT))
    }

    fn on_idle(&mut self) {
        self.synchronize_backend_state();
    }

    fn on_window_resize(&mut self, _ctx: &WindowContext, width: u32, height: u32) {
        self.webview_size
            .set(webview_size_for_window(width as f32, height as f32));
        self.repaint_revision
            .update(|revision| *revision = revision.wrapping_add(1));
    }

    fn debug_logging(&self) -> bool {
        false
    }
}

fn viewport_for_size(size: Size) -> BrowserViewport {
    let scale = graphics::current_scale_milli().max(1) as f32 / 1_000.0;
    BrowserViewport {
        width: (size.width.max(1.0) * scale).ceil() as u32,
        height: (size.height.max(1.0) * scale).ceil() as u32,
        scale,
    }
}

fn webview_size_for_window(width: f32, height: f32) -> Size {
    let decoration =
        WindowContentLayout::for_decoration(WindowDecoration::CUSTOM).decoration_size();
    Size::new(
        (width - decoration.width).max(1.0),
        (height - decoration.height - BROWSER_CHROME_HEIGHT).max(1.0),
    )
}

fn navigate_from_address(backend: &Rc<RefCell<BrowserWorker>>, address: &State<String>) {
    if let Err(error) = backend.borrow_mut().navigate(&address.get()) {
        eprintln!("myrica: {error}");
    }
}

fn dispatch_webview_event(backend: &Rc<RefCell<BrowserWorker>>, event: &Event) -> bool {
    let input = match event {
        Event::Mouse(MouseEvent::Moved { x, y }) | Event::Mouse(MouseEvent::Entered { x, y }) => {
            BrowserInput::PointerMoved {
                x: *x as f32,
                y: *y as f32,
            }
        }
        Event::Mouse(MouseEvent::Exited { .. }) => BrowserInput::PointerExited,
        Event::Mouse(MouseEvent::ButtonPressed { button, x, y, .. }) => {
            BrowserInput::PointerButton {
                button: map_mouse_button(*button),
                pressed: true,
                x: *x as f32,
                y: *y as f32,
            }
        }
        Event::Mouse(MouseEvent::ButtonReleased { button, x, y, .. })
        | Event::Mouse(MouseEvent::ButtonCancelled { button, x, y }) => {
            BrowserInput::PointerButton {
                button: map_mouse_button(*button),
                pressed: false,
                x: *x as f32,
                y: *y as f32,
            }
        }
        Event::Mouse(MouseEvent::Wheel {
            delta_x,
            delta_y,
            x,
            y,
            ..
        }) => BrowserInput::Wheel {
            delta_x: f64::from(*delta_x),
            delta_y: f64::from(*delta_y),
            x: *x as f32,
            y: *y as f32,
        },
        Event::Keyboard(KeyEvent::Pressed { keycode, modifiers }) => BrowserInput::Key {
            key: map_key(*keycode),
            pressed: true,
            modifiers: map_modifiers(*modifiers),
        },
        Event::Keyboard(KeyEvent::Released { keycode, modifiers }) => BrowserInput::Key {
            key: map_key(*keycode),
            pressed: false,
            modifiers: map_modifiers(*modifiers),
        },
        Event::Keyboard(KeyEvent::Char { c }) => BrowserInput::Text(*c),
        Event::TextInputPreedit {
            text,
            cursor_byte,
            anchor_byte,
            ..
        } => BrowserInput::ImePreedit {
            text: text.clone(),
            cursor: *cursor_byte as usize,
            anchor: *anchor_byte as usize,
        },
        Event::TextInputCommit { text, .. } => BrowserInput::ImeCommit(text.clone()),
        Event::TextInputDone { .. } => return true,
        _ => return false,
    };
    backend.borrow_mut().handle_input(input)
}

fn map_mouse_button(button: MouseButton) -> BrowserMouseButton {
    match button {
        MouseButton::Left => BrowserMouseButton::Primary,
        MouseButton::Middle => BrowserMouseButton::Auxiliary,
        MouseButton::Right => BrowserMouseButton::Secondary,
    }
}

fn map_modifiers(modifiers: KeyModifiers) -> BrowserModifiers {
    BrowserModifiers {
        shift: modifiers.shift,
        control: modifiers.control,
        alt: modifiers.alt,
        super_key: modifiers.super_key,
    }
}

fn map_key(key: KeyCode) -> BrowserKey {
    match key {
        KeyCode::Escape => BrowserKey::Escape,
        KeyCode::Enter => BrowserKey::Enter,
        KeyCode::Tab => BrowserKey::Tab,
        KeyCode::Backspace => BrowserKey::Backspace,
        KeyCode::Space => BrowserKey::Space,
        KeyCode::Left => BrowserKey::Left,
        KeyCode::Right => BrowserKey::Right,
        KeyCode::Up => BrowserKey::Up,
        KeyCode::Down => BrowserKey::Down,
        KeyCode::Home => BrowserKey::Home,
        KeyCode::End => BrowserKey::End,
        KeyCode::PageUp => BrowserKey::PageUp,
        KeyCode::PageDown => BrowserKey::PageDown,
        KeyCode::Insert => BrowserKey::Insert,
        KeyCode::Delete => BrowserKey::Delete,
        KeyCode::Char(character) => BrowserKey::Character(character),
        KeyCode::Unknown | KeyCode::F(_) => BrowserKey::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webview_size_excludes_window_and_browser_chrome() {
        let decoration =
            WindowContentLayout::for_decoration(WindowDecoration::CUSTOM).decoration_size();
        let webview = webview_size_for_window(WINDOW_WIDTH, WINDOW_HEIGHT);

        assert_eq!(webview.width + decoration.width, WINDOW_WIDTH);
        assert_eq!(
            webview.height + decoration.height + BROWSER_CHROME_HEIGHT,
            WINDOW_HEIGHT
        );
    }
}
