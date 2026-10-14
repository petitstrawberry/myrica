//! Exercise WebKit's actual Skia Vulkan backend through the installed loader/ICD.
use std::process::ExitCode;

fn main() -> ExitCode {
    std::thread_local! { static RUST_TLS: std::cell::Cell<u64> = const { std::cell::Cell::new(123) }; }
    unsafe {
        assert_eq!(myrica_webkit_sys::scarlet_webview_tls_value(), 17);
        assert_eq!(myrica_webkit_sys::scarlet_webview_tls_increment(), 18);
    }
    RUST_TLS.set(456);
    let threads: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                assert_eq!(RUST_TLS.get(), 123);
                RUST_TLS.set(789);
                unsafe {
                    assert_eq!(myrica_webkit_sys::scarlet_webview_tls_value(), 17);
                    assert_eq!(myrica_webkit_sys::scarlet_webview_tls_increment(), 18);
                    assert_eq!(myrica_webkit_sys::scarlet_webview_tls_increment(), 19);
                }
                assert_eq!(RUST_TLS.get(), 789);
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("TLS isolation probe");
    }
    assert_eq!(RUST_TLS.get(), 456);
    assert_eq!(
        unsafe { myrica_webkit_sys::scarlet_webview_tls_value() },
        18
    );
    println!("PASS: initialized C++ ELF TLS and Rust TLS coexist across 4 threads");
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
