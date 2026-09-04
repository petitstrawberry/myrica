//! Myrica browser entry point.

#[cfg(not(feature = "backend-blitz"))]
compile_error!("Myrica currently requires the `backend-blitz` feature");

mod app;
mod backend;
#[cfg(feature = "backend-blitz")]
mod network;
mod platform;

use std::process::ExitCode;

use app::MyricaApp;
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
    let initial_location = parse_arguments()?;
    let mut app = MyricaApp::new(&initial_location)?;
    app.run().map_err(|error| error.to_string())
}

fn parse_arguments() -> Result<String, String> {
    let mut arguments = std::env::args().skip(1);
    let first = arguments.next();
    if matches!(first.as_deref(), Some("-h" | "--help")) {
        println!("usage: myrica [URL]");
        println!("       URL defaults to {DEFAULT_LOCATION}");
        std::process::exit(0);
    }
    if let Some(argument) = arguments.next() {
        return Err(format!("unexpected argument: {argument}"));
    }
    Ok(first.unwrap_or_else(|| String::from(DEFAULT_LOCATION)))
}
