//! Shared configuration for playback and the native setup screen.
use crate::CdmHostOptions;
use scarlet_os::environment::{Environment, HandleMapping};
use scarlet_os::socket::Socket;
use std::io::Read;
use std::time::{Duration, Instant};

pub const BUNDLED_HOST: &str = "/usr/libexec/myrica/widevine-host";
const LEGACY_HOST: &str = "/shared/widevine/widevine-host";
const LEGACY_CDM: &str = "/shared/widevine/libwidevinecdm.so";

/// Explicit overrides are authoritative, including paths which fail to open.
/// The Linux helper owns CDM discovery, using its actual filesystem view.
pub fn configure(host: &str, cdm: &str, ld: &str) -> Result<CdmHostOptions, String> {
    let environment = Environment::current().map_err(|e| format!("実行環境: {e:?}"))?;
    let view = environment.root("linux-aarch64").map_err(|_| {
        "Debian (linux-aarch64) ビューがありません。Debianバンドルを導入してください。".to_owned()
    })?;
    let host = match std::env::var("MYRICA_WIDEVINE_HOST") {
        Ok(path) => path,
        Err(_) if !host.is_empty() && host != LEGACY_HOST => host.to_owned(),
        Err(_) if view.open(BUNDLED_HOST, 0).is_ok() => BUNDLED_HOST.to_owned(),
        Err(_) => LEGACY_HOST.to_owned(),
    };
    if host.is_empty() || view.open(&host, 0).is_err() {
        return Err(format!(
            "Widevineホストがありません: {host}\nホストを含むMyricaバンドルを導入してください。MYRICA_WIDEVINE_HOST の指定も確認してください。"
        ));
    }
    if view.open("/lib/ld-linux-aarch64.so.1", 0).is_err() {
        return Err("Debianのローダー /lib/ld-linux-aarch64.so.1 がありません。Debianバンドルを確認してください。".to_owned());
    }
    let cdm = std::env::var("MYRICA_WIDEVINE_CDM").unwrap_or_else(|_| {
        if cdm.is_empty() || cdm == LEGACY_CDM {
            "auto".into()
        } else {
            cdm.into()
        }
    });
    if cdm.is_empty() {
        return Err(
            "MYRICA_WIDEVINE_CDM が空です。指定を解除するかCDMのパスを設定してください。".into(),
        );
    }
    let mut options = CdmHostOptions::new(host, cdm);
    let ld = std::env::var("MYRICA_WIDEVINE_LD_PATH").unwrap_or_else(|_| ld.to_owned());
    if !ld.is_empty() {
        options.host_ld_path = Some(ld);
    }
    Ok(options)
}

/// Bounded load/initialization check. The caller runs this on a worker thread.
/// fd 3 carries a short reply; no temporary files or license sessions are used.
pub fn check() -> Result<String, String> {
    let options = configure("", "", "")?;
    let environment = Environment::current().map_err(|e| format!("{e:?}"))?;
    let view = environment
        .root("linux-aarch64")
        .map_err(|e| format!("{e:?}"))?;
    let executable = view
        .open(&options.host_path, 0)
        .map_err(|e| format!("ホスト: {e:?}"))?;
    let (mut reader, writer) = Socket::pair().map_err(|e| format!("確認用ソケット: {e:?}"))?;
    reader.set_nonblocking(true).map_err(|e| format!("{e:?}"))?;
    let stdin = crate::duplicate_stdio(0).map_err(|e| format!("{e:?}"))?;
    let stdout = crate::duplicate_stdio(1).map_err(|e| format!("{e:?}"))?;
    let stderr = crate::duplicate_stdio(2).map_err(|e| format!("{e:?}"))?;
    let maps = [
        HandleMapping {
            source: &stdin,
            target: 0,
        },
        HandleMapping {
            source: &stdout,
            target: 1,
        },
        HandleMapping {
            source: &stderr,
            target: 2,
        },
        HandleMapping {
            source: writer.as_handle(),
            target: 3,
        },
    ];
    let env: Vec<String> = options
        .host_ld_path
        .as_ref()
        .map(|p| vec![format!("LD_LIBRARY_PATH={p}")])
        .unwrap_or_default();
    let env: Vec<&str> = env.iter().map(String::as_str).collect();
    let pid = environment
        .spawn_with_abi(
            "linux-aarch64",
            &executable,
            &[
                "widevine-host",
                "--check",
                "--fd",
                "3",
                "--cdm",
                &options.cdm_path,
            ],
            &env,
            "/",
            &maps,
        )
        .map_err(|e| format!("Linuxホストの起動に失敗: {e:?}"))?;
    drop(writer);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    let result = loop {
        let mut buffer = [0u8; 1024];
        match reader.read(&mut buffer) {
            Ok(0) => break Err("ホストが確認結果を返さず終了しました。Debianのローダー、libc6、libnspr4とホストのバージョンを確認してください。".into()),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() > 4096 { break Err("ホストの応答が長すぎます。".into()); }
                if bytes.contains(&b'\n') { break parse_reply(&bytes); }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break Err(format!("ホストの確認に失敗: {e}")),
        }
        if Instant::now() >= deadline {
            break Err(
                "CDMの確認がタイムアウトしました。ホストと依存ライブラリを確認してください。"
                    .into(),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Reap the check process without blocking the UI or waiting indefinitely.
    let deadline = Instant::now() + Duration::from_secs(1);
    while scarlet_os::process::waitpid(pid as i32, scarlet_os::process::WAIT_NOHANG).0 == 0 {
        if Instant::now() >= deadline {
            let _ = scarlet_os::ipc::send_process_control_to_group(
                Some(pid),
                scarlet_os::ipc::ProcessControl::Kill,
            );
            let _ = scarlet_os::process::waitpid(pid as i32, scarlet_os::process::WAIT_NOHANG);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    result.map(|detail| format!("CDMの読み込み・初期化に成功しました。\n{detail}\nホスト: {}\n再生ページを再読み込みしてお試しください。", options.host_path))
}

fn parse_reply(bytes: &[u8]) -> Result<String, String> {
    let line = std::str::from_utf8(bytes)
        .map_err(|_| "ホストの応答形式が不正です。")?
        .lines()
        .next()
        .unwrap_or("");
    if let Some(detail) = line.strip_prefix("WVH_CHECK\tOK\t") {
        return Ok(detail.to_owned());
    }
    if let Some(detail) = line.strip_prefix("WVH_CHECK\tERROR\t") {
        return Err(if detail.contains("CDM_NOT_FOUND") {
            "Widevine CDMがありません。下の手順でwidevine-installerを実行してください。".into()
        } else if detail.contains("libnspr4.so") {
            "libnspr4.so がありません。Debianビューで apt-get install libnspr4 を実行してください。"
                .into()
        } else {
            format!("CDMの読み込み・初期化に失敗: {detail}")
        });
    }
    Err("ホストがセットアップ確認に対応していません。Myricaバンドルを更新してください。".into())
}
