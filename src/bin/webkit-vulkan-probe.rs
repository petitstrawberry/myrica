//! Exercise WebKit's actual Skia Vulkan backend through the installed loader/ICD.
use std::process::ExitCode;

unsafe extern "C" {
    fn __errno_location() -> *mut i32;
}

fn main() -> ExitCode {
    std::thread_local! { static RUST_TLS: std::cell::Cell<u64> = const { std::cell::Cell::new(123) }; }
    unsafe {
        *__errno_location() = 61;
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
                    assert_eq!(*__errno_location(), 0);
                    *__errno_location() = 62;
                    assert_eq!(myrica_webkit_sys::scarlet_webview_tls_value(), 17);
                    assert_eq!(myrica_webkit_sys::scarlet_webview_tls_increment(), 18);
                    assert_eq!(myrica_webkit_sys::scarlet_webview_tls_increment(), 19);
                    assert_eq!(*__errno_location(), 62);
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
    assert_eq!(unsafe { *__errno_location() }, 61);
    println!("PASS: initialized C++ ELF TLS, Rust TLS and errno coexist across 4 threads");
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
