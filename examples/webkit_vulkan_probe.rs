//! Exercise WebKit's actual Skia Vulkan backend through the installed loader/ICD.
use std::process::ExitCode;

fn main() -> ExitCode {
    for cycle in 1..=3 {
        // Each call owns and destroys its loader, device, Ganesh and surface.
        let result = unsafe { myrica_webkit_sys::scarlet_webview_vulkan_probe() };
        if result != 0 {
            eprintln!("WebKit Vulkan probe cycle {cycle} failed: {result}");
            return ExitCode::FAILURE;
        }
    }
    println!("PASS: WebKit Skia Vulkan create/draw/readback/destroy, 3 cycles");
    ExitCode::SUCCESS
}
