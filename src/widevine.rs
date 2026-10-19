//! Native Widevine setup UI. Filesystem/CDM work stays off the UI thread.
use scarlet_ui::prelude::*;
use scarlet_ui::{generate_state_id, hstack, vstack};
use std::result::Result;
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub const INSTRUCTIONS: &str = "Debianビューのシェルで実行してください。\n\nabi-run linux-aarch64 /bin/bash\nexport PATH=/usr/sbin:/usr/bin:/sbin:/bin\n\ntrixie-backports の contrib をまだ有効にしていない場合:\ncat > /etc/apt/sources.list.d/widevine-backports.sources <<'EOF'\nTypes: deb\nURIs: http://deb.debian.org/debian\nSuites: trixie-backports\nComponents: main contrib\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\nEOF\n\napt-get update\napt-get install -t trixie-backports widevine-installer libnspr4\nwidevine-installer\n\nCDMはインストーラーの案内と利用条件を確認して導入してください。\n導入後に「再確認」を押してください。\n\n既存のCDMも自動検出します。独自の配置には\nMYRICA_WIDEVINE_HOST / MYRICA_WIDEVINE_CDM /\nMYRICA_WIDEVINE_LD_PATH を指定できます。";

pub struct Setup {
    pub open: State<bool>,
    pub status: State<String>,
    pending: Option<Receiver<Result<String, String>>>,
}
impl Default for Setup {
    fn default() -> Self {
        Self {
            open: State::new(generate_state_id(), false),
            status: State::new(generate_state_id(), "未確認".into()),
            pending: None,
        }
    }
}
impl Setup {
    pub fn start(&mut self) {
        self.open.set(true);
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("myrica-widevine-setup".into())
            .spawn(move || {
                let _ = tx.send(check());
            }) {
            Ok(_) => {
                self.pending = Some(rx);
                self.status
                    .set("CDMとLinuxの依存ライブラリを確認しています…".into());
            }
            Err(error) => self
                .status
                .set(format!("確認スレッドの起動に失敗: {error}")),
        }
    }
    pub fn poll(&mut self) {
        let Some(receiver) = self.pending.as_ref() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                Err("確認処理が終了しました。再確認してください。".into())
            }
        };
        self.pending = None;
        let text = match result {
            Ok(text) => text,
            Err(error) => format!("設定が必要です\n{error}"),
        };
        eprintln!("[myrica:widevine] {text}");
        self.status.set(text);
    }
}

fn check() -> Result<String, String> {
    #[cfg(all(target_os = "scarlet", feature = "backend-webkit-vulkan"))]
    {
        myrica_webkit_sys::widevine_setup::check()
    }
    #[cfg(not(all(target_os = "scarlet", feature = "backend-webkit-vulkan")))]
    {
        Err("WidevineのセットアップにはScarlet用WebKitバンドルが必要です。".into())
    }
}

pub fn panel(setup: std::rc::Rc<std::cell::RefCell<Setup>>) -> impl View + Clone {
    let status = setup.borrow().status.clone();
    let close = setup.borrow().open.clone();
    vstack! {
        hstack! {
            Text::new("Widevine セットアップ").font_size(21.0).frame_width(f32::INFINITY),
            Button::new("再確認").on_click(move || setup.borrow_mut().start()),
            Button::new("閉じる").on_click(move || close.set(false)),
        }.spacing(12.0),
        ScrollView::new(vstack! {
            Text::from_state(status).font_size(14.0),
            Text::new(INSTRUCTIONS).font_size(13.0),
        }.spacing(18.0).alignment(Alignment::Leading).padding(12.0))
            .both_axes().content_size(760.0, 850.0)
            .frame(f32::INFINITY, f32::INFINITY),
    }
    .spacing(12.0)
    .padding(18.0)
    .background(Color::rgb(247, 245, 242))
    .frame(f32::INFINITY, f32::INFINITY)
    .on_event(|_| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closing_during_check_and_rechecking_consumes_fresh_results_on_ui_thread() {
        let mut setup = Setup::default();
        let (tx, rx) = mpsc::channel();
        setup.pending = Some(rx);
        setup.open.set(false);
        tx.send(Err("libnspr4.so".into())).unwrap();
        setup.poll();
        assert!(!setup.open.get());
        assert!(setup.status.get().contains("libnspr4.so"));
        assert!(setup.pending.is_none());
        let (tx, rx) = mpsc::channel();
        setup.pending = Some(rx);
        tx.send(Ok("CDM initialized".into())).unwrap();
        setup.poll();
        assert_eq!(setup.status.get(), "CDM initialized");
    }
}
