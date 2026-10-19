//! Myrica browser entry point.

#[cfg(not(any(feature = "backend-blitz", feature = "backend-webkit")))]
compile_error!("Myrica requires `backend-blitz` or `backend-webkit`");

mod app;
mod backend;
#[cfg(feature = "backend-blitz")]
mod network;
mod platform;
#[cfg(feature = "backend-blitz")]
mod sgfx_scene;
mod widevine;

use std::process::ExitCode;

use app::MyricaApp;
use backend::BackendKind;
use scarlet_ui::ApplicationRunExt;

const DEFAULT_LOCATION: &str = "https://example.com/";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("myrica: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let Some((kind, initial_location, setup)) = parse_arguments(std::env::args().skip(1))? else {
        return Ok(());
    };
    let mut app = if kind == BackendKind::default() {
        MyricaApp::new(&initial_location)?
    } else {
        MyricaApp::with_backend(&initial_location, kind)?
    };
    if setup {
        app.show_widevine_setup();
    }
    let result = app.run().map_err(|error| error.to_string());
    app.shutdown();
    result
}

fn parse_arguments(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Option<(BackendKind, String, bool)>, String> {
    let mut kind = BackendKind::default();
    let mut location = None;
    let mut setup = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                println!("usage: myrica [--backend blitz|webkit] [--widevine-setup] [URL]");
                println!("       URL defaults to {DEFAULT_LOCATION}");
                // Return through the native CRT so C/C++ finalizers also run.
                return Ok(None);
            }
            "--widevine-setup" => setup = true,
            "--backend" => {
                kind = BackendKind::parse(
                    &arguments
                        .next()
                        .ok_or("--backend requires an engine name")?,
                )
                .map_err(|error| error.to_string())?
            }
            _ if argument.starts_with("--backend=") => {
                kind = BackendKind::parse(&argument[10..]).map_err(|error| error.to_string())?
            }
            _ if argument.starts_with('-') => return Err(format!("unknown option: {argument}")),
            _ if location.is_none() => location = Some(argument),
            _ => return Err(format!("unexpected argument: {argument}")),
        }
    }
    Ok(Some((
        kind,
        location.unwrap_or_else(|| {
            String::from(if setup {
                "about:blank"
            } else {
                DEFAULT_LOCATION
            })
        }),
        setup,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_returns_without_starting_the_application_or_exiting_the_process() {
        for flag in ["-h", "--help"] {
            assert!(
                parse_arguments([flag.to_owned()].into_iter())
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn setup_opens_without_an_unrelated_network_load() {
        let (_, url, setup) = parse_arguments(["--widevine-setup".into()].into_iter())
            .unwrap()
            .unwrap();
        assert!(setup);
        assert_eq!(url, "about:blank");
    }

    #[test]
    fn cli_rejects_missing_backend_and_extra_locations() {
        assert!(parse_arguments(["--backend".to_owned()].into_iter()).is_err());
        assert!(parse_arguments(["a".to_owned(), "b".to_owned()].into_iter()).is_err());
        let (kind, location, setup) =
            parse_arguments(["https://ichigo.dev/".to_owned()].into_iter())
                .unwrap()
                .unwrap();
        assert!(!setup);
        assert_eq!(kind, BackendKind::default());
        assert_eq!(location, "https://ichigo.dev/");
    }
}
