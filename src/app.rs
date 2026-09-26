//! ScarletUI application shell for Myrica.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use scarlet_ui::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent};
use scarlet_ui::graphics;
use scarlet_ui::prelude::*;
use scarlet_ui::{
    Application, ComponentElement, Element, Listenable, SgfxCanvas, SgfxCanvasFrame,
    SgfxCanvasHandle, Size, View, Window, WindowContext, generate_state_id,
};
use scarlet_ui::{hstack, vstack};

use crate::backend::{
    BrowserBackend, BrowserInput, BrowserKey, BrowserModifiers, BrowserMouseButton,
    BrowserSnapshot, WakeCallback, create_backend,
};

const WINDOW_WIDTH: f32 = 1_100.0;
const WINDOW_HEIGHT: f32 = 760.0;
const BROWSER_CHROME_HEIGHT: f32 = 86.0;
const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// Myrica's browser chrome and selected embedded engine.
#[derive(Clone)]
pub struct MyricaApp {
    backend: Rc<RefCell<Box<dyn BrowserBackend>>>,
    address: State<String>,
    snapshot: State<BrowserSnapshot>,
    repaint_revision: State<u64>,
    rendered_revision: Rc<Cell<u64>>,
    last_rendered_at: Rc<Cell<Instant>>,
    canvas_handle: SgfxCanvasHandle,
    canvas_frame: State<Arc<SgfxCanvasFrame>>,
    webview_size: State<Size>,
    webview_focused: State<bool>,
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

        let mut backend = create_backend(wake).map_err(|error| error.to_string())?;
        if let Err(error) = backend.navigate(initial_location) {
            eprintln!("myrica: {error}");
        }
        let initial_snapshot = backend.snapshot();
        let initial_url = initial_snapshot.url.clone();
        let webview_size = Size::new(WINDOW_WIDTH, WINDOW_HEIGHT - BROWSER_CHROME_HEIGHT);
        let initial_frame =
            backend.render(webview_size.width as u32, webview_size.height as u32, 1.0);
        let initial_revision = repaint_revision.get();

        Ok(Self {
            backend: Rc::new(RefCell::new(backend)),
            address: State::new(generate_state_id(), initial_url.clone()),
            snapshot: State::new(generate_state_id(), initial_snapshot),
            repaint_revision,
            rendered_revision: Rc::new(Cell::new(initial_revision)),
            last_rendered_at: Rc::new(Cell::new(Instant::now())),
            canvas_handle: SgfxCanvasHandle::new(),
            canvas_frame: State::new(generate_state_id(), initial_frame),
            webview_size: State::new(generate_state_id(), webview_size),
            webview_focused: State::new(generate_state_id(), false),
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
        let backend_changed = self.backend.borrow_mut().tick();
        let latest = self.backend.borrow().snapshot();
        if latest != self.snapshot.get() {
            self.snapshot.set(latest.clone());
        }

        let mut last_url = self.last_backend_url.borrow_mut();
        if latest.url != *last_url {
            self.address.set(latest.url.clone());
            *last_url = latest.url;
        }

        let requested_revision = self.repaint_revision.get();
        let animation_frame_due = requested_revision != self.rendered_revision.get()
            && self.last_rendered_at.get().elapsed() >= ANIMATION_FRAME_INTERVAL;
        if backend_changed || animation_frame_due {
            let size = self.webview_size.get();
            let scale = graphics::current_scale_milli().max(1) as f32 / 1_000.0;
            let width = (size.width.max(1.0) * scale).ceil() as u32;
            let height = (size.height.max(1.0) * scale).ceil() as u32;
            let frame = self.backend.borrow_mut().render(width, height, scale);
            self.canvas_frame.set(frame);
            self.rendered_revision.set(requested_revision);
            self.last_rendered_at.set(Instant::now());
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
        self.webview_size.set(Size::new(
            width.max(1) as f32,
            (height as f32 - BROWSER_CHROME_HEIGHT).max(1.0),
        ));
        self.repaint_revision
            .update(|revision| *revision = revision.wrapping_add(1));
    }

    fn debug_logging(&self) -> bool {
        false
    }
}

fn navigate_from_address(backend: &Rc<RefCell<Box<dyn BrowserBackend>>>, address: &State<String>) {
    if let Err(error) = backend.borrow_mut().navigate(&address.get()) {
        eprintln!("myrica: {error}");
    }
}

fn dispatch_webview_event(backend: &Rc<RefCell<Box<dyn BrowserBackend>>>, event: &Event) -> bool {
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
