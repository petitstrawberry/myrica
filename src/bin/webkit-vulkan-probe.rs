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
    #[cfg(feature = "backend-webkit-vulkan")]
    if let Err(error) = gpu_page_contract() {
        eprintln!("WebCore GPU page contract failed: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(feature = "backend-webkit-vulkan")]
fn gpu_page_contract() -> Result<(), String> {
    use myrica_webkit_sys as ffi;
    fn check(status: i32, operation: &str) -> Result<(), String> {
        if status == ffi::OK {
            Ok(())
        } else {
            Err(format!("{operation}: status {status}"))
        }
    }
    // Every call runs on this owner. The fixture uses the ordinary HTTP response
    // bridge, parser, JavaScriptCore, CSS layout and frame-view GPU painting.
    unsafe {
        let font = std::env::var("MYRICA_WEBKIT_FONT_CONFIG")
            .unwrap_or_else(|_| "/etc/fonts/fonts.conf".into());
        check(
            ffi::scarlet_webview_initialize(ffi::Bytes::from_str(&font)),
            "initialize",
        )?;
        let mut view = std::ptr::null_mut();
        let configuration = ffi::Configuration {
            abi_version: ffi::ABI_VERSION,
            width: 320,
            height: 224,
            script_enabled: 1,
            device_scale_factor: 2.0,
        };
        check(
            ffi::scarlet_webview_create(&configuration, &mut view),
            "create",
        )?;
        let result = (|| {
            let location = "https://myrica.test/gpu-contract";
            check(
                ffi::scarlet_webview_navigate(view, ffi::Bytes::from_str(location)),
                "navigate",
            )?;
            let html = r#"<!doctype html><meta charset="utf-8"><title>GPU page contract</title>
<style>html,body{margin:0;background:white;width:160px;height:112px}
#r,#b{position:absolute;width:32px;height:32px}#r{left:16px;top:16px;background:red}
#b{left:64px;top:32px;background:blue;border-radius:8px}#tap,#red-tap{position:absolute;left:112px;width:40px;height:32px}
#tap{top:72px}#red-tap{top:8px}
#marker{position:absolute;left:2px;top:88px;font:12px sans-serif}</style>
<div id="r"></div><div id="b"></div><div id="tap" onclick="flip()">tap</div><div id="red-tap" onclick="yellow()">red</div><div id="marker">Loading</div>
<script>let green=false;document.getElementById('marker').textContent='JS executed';
function flip(){green=!green;document.getElementById('b').style.background=green?'lime':'blue';}
function yellow(){document.getElementById('r').style.background='yellow';}</script>"#;
            let header = ffi::Header {
                name: ffi::Bytes::from_str("Content-Type"),
                value: ffi::Bytes::from_str("text/html; charset=utf-8"),
            };
            let mut completed = false;
            for _ in 0..16 {
                check(ffi::scarlet_webview_tick(view), "initial tick")?;
                let mut request = ffi::Request::default();
                if ffi::scarlet_webview_poll_request(view, &mut request) == ffi::OK
                    && request.is_cancel == 0
                {
                    let response = ffi::Response {
                        request_id: request.request_id,
                        status_code: 200,
                        final_url: ffi::Bytes::from_str(location),
                        status_text: ffi::Bytes::from_str("OK"),
                        headers: &header,
                        header_count: 1,
                        body: ffi::Bytes::from_str(html),
                        ..Default::default()
                    };
                    check(
                        ffi::scarlet_webview_complete_request(view, &response),
                        "complete HTML",
                    )?;
                    completed = true;
                    break;
                }
            }
            if !completed {
                return Err("fixture navigation produced no request".into());
            }
            for _ in 0..4 {
                check(ffi::scarlet_webview_tick(view), "layout tick")?;
            }
            check(ffi::scarlet_webview_focus(view, 1), "focus")?;
            let first = acquire_gpu(view)?;
            let first_id = gpu_image_id(view, first.generation)?;
            verify_gpu_pixels(view, first.generation, [0, 0, 255, 255], [255, 0, 0, 255])?;
            let mut marker = ffi::Bytes::default();
            check(
                ffi::scarlet_webview_text_content(
                    view,
                    ffi::Bytes::from_str("marker"),
                    &mut marker,
                ),
                "inspect JS marker",
            )?;
            if std::slice::from_raw_parts(marker.data, marker.length) != b"JS executed" {
                return Err("JavaScript marker did not execute".into());
            }
            click_fixture(view, 240.0, 160.0)?;
            let second = acquire_gpu(view)?;
            let second_id = gpu_image_id(view, second.generation)?;
            if first_id == second_id {
                return Err("concurrently leased GPU pages share an allocation identity".into());
            }
            if second.generation <= first.generation {
                return Err("page generation did not advance".into());
            }
            verify_gpu_pixels(view, second.generation, [0, 0, 255, 255], [0, 255, 0, 255])?;
            // Changing DOM/painting a second image must not mutate a leased first image.
            verify_gpu_pixels(view, first.generation, [0, 0, 255, 255], [255, 0, 0, 255])?;
            let mut capacity = 99;
            check(
                ffi::scarlet_webview_gpu_frame_capacity(view, &mut capacity),
                "capacity",
            )?;
            if capacity != 0 {
                return Err("two live frames did not apply backpressure".into());
            }
            // A second, distant damage rect arrives while both images remain
            // leased. Reusing A must accumulate B's green update as well.
            click_fixture(view, 240.0, 40.0)?;
            let mut blocked = ffi::GpuFrame::default();
            if ffi::scarlet_webview_gpu_frame(view, &mut blocked) != ffi::EMPTY
                || blocked.handle != -1
                || blocked.generation != 0
            {
                return Err("dirty page bypassed two-image lease backpressure".into());
            }
            verify_gpu_pixels(view, first.generation, [0, 0, 255, 255], [255, 0, 0, 255])?;
            verify_gpu_pixels(view, second.generation, [0, 0, 255, 255], [0, 255, 0, 255])?;
            close_frame(first.handle);
            check(
                ffi::scarlet_webview_release_gpu_frame(view, first.generation),
                "release first",
            )?;
            verify_stale_image_id(view, first.generation)?;
            let mut before = ffi::PaintStatistics::default();
            check(
                ffi::scarlet_webview_paint_statistics(view, &mut before),
                "paint statistics before reuse",
            )?;
            let third = acquire_gpu(view)?;
            if gpu_image_id(view, third.generation)? != first_id
                || third.generation <= second.generation
            {
                return Err("third page did not reuse A with a fresh generation".into());
            }
            verify_stale_image_id(view, first.generation)?;
            verify_gpu_pixels(view, third.generation, [0, 255, 255, 255], [0, 255, 0, 255])?;
            verify_gpu_pixels(view, second.generation, [0, 0, 255, 255], [0, 255, 0, 255])?;
            let mut after = ffi::PaintStatistics::default();
            check(
                ffi::scarlet_webview_paint_statistics(view, &mut after),
                "paint statistics after reuse",
            )?;
            let painted = after
                .painted_pixels
                .checked_sub(before.painted_pixels)
                .ok_or("paint statistics regressed")?;
            if after.paints != before.paints + 1 || painted == 0 || painted >= 320 * 224 {
                return Err(format!(
                    "GPU reuse failed bounded DPR2 damage: paints {} -> {}, painted_pixels={painted}",
                    before.paints, after.paints
                ));
            }
            println!(
                "PASS: GPU allocation IDs A={first_id} B={second_id}, A reuse accumulates distant damage, painted_pixels={painted}"
            );
            close_frame(third.handle);
            check(
                ffi::scarlet_webview_release_gpu_frame(view, third.generation),
                "release third",
            )?;
            verify_stale_image_id(view, third.generation)?;
            check(ffi::scarlet_webview_resize(view, 384, 256, 2.0), "resize")?;
            check(ffi::scarlet_webview_tick(view), "resize tick")?;
            let resized = acquire_gpu(view)?;
            let resized_id = gpu_image_id(view, resized.generation)?;
            if resized.width != 384
                || resized.height != 256
                || resized_id == first_id
                || resized_id == second_id
            {
                return Err("resized GPU page reused an old allocation identity or extent".into());
            }
            verify_gpu_pixels(
                view,
                resized.generation,
                [0, 255, 255, 255],
                [0, 255, 0, 255],
            )?;
            verify_gpu_pixels(view, second.generation, [0, 0, 255, 255], [0, 255, 0, 255])?;
            if gpu_image_id(view, second.generation)? != second_id {
                return Err("resize changed the identity of a still-leased page".into());
            }
            close_frame(resized.handle);
            check(
                ffi::scarlet_webview_release_gpu_frame(view, resized.generation),
                "release resized",
            )?;
            verify_stale_image_id(view, resized.generation)?;
            close_frame(second.handle);
            check(
                ffi::scarlet_webview_release_gpu_frame(view, second.generation),
                "release second",
            )?;
            verify_stale_image_id(view, second.generation)?;
            println!(
                "PASS: WebCore DOM/CSS/JS/input Retina GPU pages, two-image backpressure and immutable leased readback"
            );
            Ok(())
        })();
        let destroyed = check(ffi::scarlet_webview_destroy(view), "destroy");
        let shutdown = check(ffi::scarlet_webview_shutdown(), "shutdown");
        result.and(destroyed).and(shutdown)
    }
}
#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn acquire_gpu(
    view: *mut myrica_webkit_sys::ScarletWebView,
) -> Result<myrica_webkit_sys::GpuFrame, String> {
    let mut frame = myrica_webkit_sys::GpuFrame::default();
    let status = unsafe { myrica_webkit_sys::scarlet_webview_gpu_frame(view, &mut frame) };
    if status != 0 || frame.handle < 0 {
        Err(format!("acquire GPU page: {status}"))
    } else {
        Ok(frame)
    }
}
#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn gpu_image_id(
    view: *mut myrica_webkit_sys::ScarletWebView,
    generation: u64,
) -> Result<u64, String> {
    let mut image_id = 0;
    let status = unsafe {
        myrica_webkit_sys::scarlet_webview_gpu_frame_image_id(view, generation, &mut image_id)
    };
    if status != myrica_webkit_sys::OK || image_id == 0 {
        Err(format!(
            "leased GPU image identity: status={status} id={image_id}"
        ))
    } else {
        Ok(image_id)
    }
}
#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn verify_stale_image_id(
    view: *mut myrica_webkit_sys::ScarletWebView,
    generation: u64,
) -> Result<(), String> {
    let mut image_id = u64::MAX;
    let status = unsafe {
        myrica_webkit_sys::scarlet_webview_gpu_frame_image_id(view, generation, &mut image_id)
    };
    if status != myrica_webkit_sys::STALE_REQUEST || image_id != 0 {
        Err(format!(
            "released GPU image identity was not cleared: status={status} id={image_id}"
        ))
    } else {
        Ok(())
    }
}
#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn verify_gpu_pixels(
    view: *mut myrica_webkit_sys::ScarletWebView,
    generation: u64,
    red: [u8; 4],
    blue: [u8; 4],
) -> Result<(), String> {
    let mut pixels = myrica_webkit_sys::Pixels::default();
    let status = unsafe {
        myrica_webkit_sys::scarlet_webview_gpu_frame_readback(view, generation, &mut pixels)
    };
    if status != 0 {
        return Err(format!("GPU page readback: {status}"));
    }
    if pixels.data.is_null()
        || pixels.width < 320
        || pixels.height < 224
        || pixels.stride < pixels.width * 4
        || (pixels.stride as usize)
            .checked_mul(pixels.height as usize)
            .is_none_or(|length| length > pixels.length)
        || pixels.format != myrica_webkit_sys::PIXEL_FORMAT_BGRA_PREMULTIPLIED
        || pixels.generation != generation
    {
        return Err("GPU page readback returned invalid metadata".into());
    }
    let bytes = unsafe { std::slice::from_raw_parts(pixels.data, pixels.length) };
    for (x, y, expected) in [
        (0, 0, [255; 4]),
        (24, 24, red),
        (64, 32, [255; 4]),
        (72, 40, blue),
    ] {
        let offset = (y * 2) * pixels.stride as usize + (x * 2) * 4;
        if bytes.get(offset..offset + 4) != Some(expected.as_slice()) {
            return Err(format!(
                "GPU DOM pixel mismatch at {x},{y}: {:?}",
                bytes.get(offset..offset + 4)
            ));
        }
    }
    let mut ink = 0;
    for y in 88..106 {
        for x in 0..104 {
            let offset = (y * 2) * pixels.stride as usize + (x * 2) * 4;
            if bytes[offset] < 96 && bytes[offset + 1] < 96 && bytes[offset + 2] < 96 {
                ink += 1;
            }
        }
    }
    if ink < 8 {
        return Err("GPU page text pixels are missing".into());
    }
    Ok(())
}
#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn click_fixture(
    view: *mut myrica_webkit_sys::ScarletWebView,
    x: f64,
    y: f64,
) -> Result<(), String> {
    for kind in [1, 2] {
        let event = myrica_webkit_sys::MouseEvent {
            kind,
            button: 1,
            click_count: 1,
            x,
            y,
            global_x: x,
            global_y: y,
            ..Default::default()
        };
        let mut handled = 0;
        let status =
            unsafe { myrica_webkit_sys::scarlet_webview_mouse(view, &event, &mut handled) };
        if status != 0 {
            return Err(format!("fixture click: {status}"));
        }
    }
    let status = unsafe { myrica_webkit_sys::scarlet_webview_tick(view) };
    if status != 0 {
        Err(format!("click tick: {status}"))
    } else {
        Ok(())
    }
}
#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn close_frame(handle: i32) {
    unsafe {
        scarlet_sys::syscall1(scarlet_sys::Syscall::HandleClose, handle as usize);
    }
}
