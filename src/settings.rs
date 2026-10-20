//! Settings use a separate scene and ScarletUI's standard navigation.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use scarlet_ui::event::{Event, KeyCode, KeyEvent};
use scarlet_ui::prelude::*;
use scarlet_ui::{NavigationLink, NavigationView, vstack};

pub const SCENE_KEY: &str = "settings";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    Settings,
    Widevine,
}

pub struct Settings {
    pub widevine: Rc<RefCell<crate::widevine::Setup>>,
    navigation: NavigationView<(NavigationLink, NavigationLink)>,
    focus_requested: Cell<bool>,
}

impl Settings {
    pub fn new(engine: &'static str) -> Self {
        let widevine = Rc::new(RefCell::new(crate::widevine::Setup::default()));
        let media = Rc::clone(&widevine);
        let navigation = NavigationView::new((
            NavigationLink::new("メディア", move || {
                crate::widevine::panel(Rc::clone(&media))
            })
            .icon(Icon::PlayerPlay),
            NavigationLink::new("このアプリについて", move || about(engine)).icon(Icon::InfoCircle),
        ))
        .presentation(NavigationPresentation::Automatic)
        .shows_icons(true)
        .sidebar_width(190.0);
        Self {
            widevine,
            navigation,
            focus_requested: Cell::new(false),
        }
    }

    pub fn open(&self) {
        self.focus_requested.set(true);
        open_window(SCENE_KEY);
    }

    /// Retain the existing command-line entry point as a settings destination.
    pub fn open_widevine(&self) {
        self.navigation.selected_index_state().set(0);
        self.widevine.borrow_mut().start();
        self.open();
    }

    pub fn take_focus_request(&self) -> bool {
        self.focus_requested.replace(false)
    }

    pub fn window(&self) -> impl Scene {
        Window::new(
            "Myrica — 設定",
            self.navigation.clone().on_event_capture(|event| {
                if matches!(
                    event,
                    Event::Keyboard(KeyEvent::Pressed {
                        keycode: KeyCode::Escape,
                        ..
                    })
                ) {
                    dismiss_window(SCENE_KEY);
                    return true;
                }
                false
            }),
        )
        .scene_key(SCENE_KEY)
        .open_at_launch(false)
        .app_id("org.scarlet-os.myrica")
        .size(Size::new(900.0, 640.0))
    }
}

fn about(engine: &'static str) -> impl View + Clone {
    vstack! {
        Text::new("Myrica").font_size(26.0),
        Text::new(format!("バージョン {}", env!("CARGO_PKG_VERSION"))).font_size(14.0),
        Text::new(format!("ブラウザエンジン: {engine}")).font_size(14.0),
    }
    .spacing(16.0)
    .alignment(Alignment::Leading)
    .padding(24.0)
    .frame(f32::INFINITY, f32::INFINITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_settings_does_not_start_a_cdm_check_or_reset_navigation() {
        let settings = Settings::new("WebKit");
        settings.navigation.selected_index_state().set(1);
        settings.open();
        assert_eq!(settings.navigation.selected_index_state().get(), 1);
        assert_eq!(settings.widevine.borrow().status.get(), "未確認");
        assert!(settings.take_focus_request());
        assert!(!settings.take_focus_request());
    }

    #[test]
    fn settings_window_is_declared_separately_and_opens_only_on_request() {
        let settings = Settings::new("WebKit");
        let mut builder = scarlet_ui::SceneBuilder::new();
        settings.window().build(&mut builder);
        let windows = builder.into_declarations();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].key.as_str(), SCENE_KEY);
        assert!(!windows[0].opens_at_launch);
    }
}
