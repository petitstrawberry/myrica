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
fn check(status: i32, operation: &str) -> Result<(), String> {
    if status == myrica_webkit_sys::OK {
        Ok(())
    } else {
        Err(format!("{operation}: status {status}"))
    }
}

#[cfg(feature = "backend-webkit-vulkan")]
fn gpu_page_contract() -> Result<(), String> {
    use myrica_webkit_sys as ffi;
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
        let mut pending_teardown = false;
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
            async_gpu_lifetime_contract(view, &second, second_id, resized.generation)?;
            close_frame(second.handle);
            check(
                ffi::scarlet_webview_release_gpu_frame(view, second.generation),
                "release second",
            )?;
            verify_stale_image_id(view, second.generation)?;
            println!(
                "PASS: WebCore DOM/CSS/JS/input Retina GPU pages, two-image backpressure and immutable leased readback"
            );
            compositor_page_contract(view)?;
            filter_bounds_contract(view)?;
            pending_cover_boundary_contract(view)?;
            let mut compositor = ffi::CompositingStatistics::default();
            if ffi::scarlet_webview_compositing_statistics(view, &mut compositor) == ffi::OK {
                // Start an actual GPU update and immediately destroy its owner.
                // No completion poll or readback may drain this on our behalf.
                check(
                    ffi::scarlet_webview_resize(view, 706, 514, 1.0),
                    "pending teardown resize",
                )?;
                check(ffi::scarlet_webview_tick(view), "pending teardown layout")?;
                let mut unfinished = ffi::GpuFrame::default();
                if ffi::scarlet_webview_gpu_frame(view, &mut unfinished) != ffi::EMPTY
                    || unfinished.handle != -1
                    || unfinished.generation != 0
                {
                    return Err("pending teardown exposed unfinished GPU output".into());
                }
                pending_teardown = true;
            }
            Ok(())
        })();
        let destroyed = check(ffi::scarlet_webview_destroy(view), "destroy");
        let shutdown = check(ffi::scarlet_webview_shutdown(), "shutdown");
        let result = result.and(destroyed).and(shutdown);
        if result.is_ok() && pending_teardown {
            println!(
                "PASS: pending GPU submission drains during actual owner destruction and engine shutdown"
            );
        }
        result
    }
}
#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn async_gpu_lifetime_contract(
    view: *mut myrica_webkit_sys::ScarletWebView,
    leased: &myrica_webkit_sys::GpuFrame,
    leased_id: u64,
    last_generation: u64,
) -> Result<(), String> {
    use myrica_webkit_sys as ffi;
    let mut compositor = ffi::CompositingStatistics::default();
    let status = unsafe { ffi::scarlet_webview_compositing_statistics(view, &mut compositor) };
    if status == ffi::UNSUPPORTED {
        return Ok(());
    }
    check(status, "async compositor profile")?;
    let mut before = ffi::PaintStatistics::default();
    check(
        unsafe { ffi::scarlet_webview_paint_statistics(view, &mut before) },
        "before pending submit",
    )?;
    check(
        unsafe { ffi::scarlet_webview_resize(view, 448, 288, 2.0) },
        "pending resize",
    )?;
    check(
        unsafe { ffi::scarlet_webview_tick(view) },
        "pending resize layout",
    )?;
    let mut pending = ffi::GpuFrame::default();
    let status = unsafe { ffi::scarlet_webview_gpu_frame(view, &mut pending) };
    if status != ffi::EMPTY
        || pending.handle != -1
        || pending.generation != 0
        || pending.width != 0
        || pending.height != 0
    {
        return Err(format!(
            "async submit published before completion observation: status={status} handle={} generation={} size={}x{}",
            pending.handle, pending.generation, pending.width, pending.height
        ));
    }
    let mut submitted = ffi::PaintStatistics::default();
    check(
        unsafe { ffi::scarlet_webview_paint_statistics(view, &mut submitted) },
        "pending paint statistics",
    )?;
    if submitted.paints != before.paints || submitted.painted_pixels != before.painted_pixels {
        return Err("in-flight GPU page advanced completed paint statistics".into());
    }
    unsafe { verify_stale_image_id(view, last_generation + 1) }?;
    // Cancel while the target is reserved, before any tick/capacity poll can
    // observe completion. Resize must drain it without publishing or reusing
    // the old storage early, while the second output remains externally leased.
    check(
        unsafe { ffi::scarlet_webview_resize(view, 480, 320, 2.0) },
        "resize during pending submit",
    )?;
    unsafe { verify_stale_image_id(view, last_generation + 1) }?;
    let native = unsafe { acquire_gpu(view) }?;
    let completed = ContractGpuFrame { view, native };
    if completed.native.generation != last_generation + 1
        || completed.native.width != 480
        || completed.native.height != 320
    {
        return Err("canceled GPU page consumed a generation or leaked an old extent".into());
    }
    if unsafe { gpu_image_id(view, completed.native.generation) }? == leased_id {
        return Err("pending resize reused the still-leased allocation".into());
    }
    unsafe {
        verify_gpu_pixels(
            view,
            completed.native.generation,
            [0, 255, 255, 255],
            [0, 255, 0, 255],
        )
    }?;
    unsafe { verify_gpu_pixels(view, leased.generation, [0, 0, 255, 255], [0, 255, 0, 255]) }?;
    if unsafe { gpu_image_id(view, leased.generation) }? != leased_id {
        return Err("async cancellation changed a still-leased allocation identity".into());
    }
    let mut after = ffi::PaintStatistics::default();
    check(
        unsafe { ffi::scarlet_webview_paint_statistics(view, &mut after) },
        "completed paint statistics",
    )?;
    if after.paints != before.paints + 1 {
        return Err("canceled submit was counted as a completed GPU page".into());
    }
    println!(
        "PASS: async completion gates capability/generation/statistics, pending resize drains and preserves old GPU lease"
    );
    Ok(())
}

#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn acquire_gpu(
    view: *mut myrica_webkit_sys::ScarletWebView,
) -> Result<myrica_webkit_sys::GpuFrame, String> {
    use myrica_webkit_sys as ffi;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let mut frame = ffi::GpuFrame::default();
        let status = unsafe { ffi::scarlet_webview_gpu_frame(view, &mut frame) };
        if status == ffi::OK {
            if frame.handle < 0 || frame.generation == 0 {
                return Err("completed GPU page has no valid capability/generation".into());
            }
            return Ok(frame);
        }
        if status != ffi::EMPTY {
            return Err(format!("acquire GPU page: {status}"));
        }
        // An in-flight target is never published as a usable native capability.
        if frame.handle != -1 || frame.generation != 0 || frame.width != 0 || frame.height != 0 {
            return Err("pending GPU page exposed an unfinished capability".into());
        }
        check(
            unsafe { ffi::scarlet_webview_tick(view) },
            "GPU completion tick",
        )?;
        if std::time::Instant::now() >= deadline {
            return Err("actual GPU page completion timed out".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
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

// This page exercises native composited CSS, never pre-rendered fixture pixels.
#[cfg(feature = "backend-webkit-vulkan")]
const COMPOSITOR_HTML: &str = r#"<!doctype html><meta charset="utf-8"><title>compositor-ready</title>
<style>html,body{margin:0;width:640px;height:480px;background:white;overflow:hidden}
#moving{position:absolute;left:24px;top:24px;width:64px;height:64px;background:red;will-change:transform,opacity;transform:translateZ(0)}
#group{position:absolute;left:240px;top:24px;width:128px;height:80px;opacity:.5;will-change:opacity}
#group div{position:absolute;width:64px;height:64px;background:red}#group div+div{left:32px;background:blue}
#clip{position:absolute;left:24px;top:144px;width:96px;height:96px;border-radius:24px;overflow:hidden;background:lime;transform:translateZ(0)}
#clip div{position:absolute;left:20px;top:20px;width:140px;height:140px;background:blue}
#mask{position:absolute;left:160px;top:144px;width:96px;height:96px;background:red;transform:translateZ(0);mask-image:linear-gradient(to right,black 50%,transparent 50%);-webkit-mask-image:linear-gradient(to right,black 50%,transparent 50%)}
#blur{position:absolute;left:24px;top:304px;width:64px;height:64px;background:red;filter:blur(4px);transform:translateZ(0)}
#shadow{position:absolute;left:160px;top:304px;width:48px;height:48px;background:blue;filter:drop-shadow(64px 0 4px lime);transform:translateZ(0)}
#scroll{position:absolute;left:384px;top:24px;width:128px;height:240px;overflow:auto;will-change:scroll-position;transform:translateZ(0)}
#stripes div{height:128px}#change,#small,#large{position:absolute;left:560px;width:64px;height:40px;background:#888;top:320px}#small{top:368px}#large{top:416px}
</style><div id="moving"></div><div id="group"><div></div><div></div></div>
<div id="clip"><div></div></div><div id="mask"></div><div id="blur"></div><div id="shadow"></div><div id="scroll"><div id="stripes"></div></div>
<div id="change" onclick="document.getElementById('moving').style.transform='translate3d(96px,0,0)';document.getElementById('moving').style.opacity='.5'"></div>
<div id="small" onclick="document.getElementById('scroll').scrollTop=16"></div>
<div id="large" onclick="document.getElementById('scroll').scrollTop=10316"></div>
<script>const colours=['red','blue','lime','yellow','magenta','cyan'];for(let i=0;i<100;i++){const stripe=document.createElement('div');stripe.style.background=colours[i%6];document.getElementById('stripes').appendChild(stripe)}</script>"#;

// At DPR2 the first filter is wider than the old 2048px clamp but smaller
// than the device's 4096px limit. The last filter needs the offscreen source at
// x=5200 CSS px to cast its shadow back into the viewport. Reverse mapping the
// visible output therefore cannot reduce its input to a small visible texture:
// source plus reverse-shadow bounds exceed 10000 physical pixels.
#[cfg(feature = "backend-webkit-vulkan")]
const FILTER_BOUNDS_HTML: &str = r#"<!doctype html><meta charset="utf-8"><title>filter-bounds-ready</title>
<style>html,body{margin:0;width:1280px;height:720px;overflow:hidden;background:white}
#wide{position:absolute;left:16px;top:24px;width:1248px;height:96px;background:linear-gradient(to right,red 0%,red 40%,blue 40%,blue 90%,lime 90%,lime 100%);filter:drop-shadow(0 24px 2px black);transform:translateZ(0)}
#clip{position:absolute;left:32px;top:164px;width:520px;height:220px;overflow:hidden;transform:translateZ(0)}
#multiple{position:absolute;left:16px;top:16px;width:160px;height:96px;background:red;filter:drop-shadow(240px 0 0 lime) drop-shadow(0 120px 0 blue) blur(2px);transform:translateZ(0)}
#far{position:absolute;left:0;top:400px;width:5400px;height:160px;filter:drop-shadow(-5000px 0 0 lime);transform:translateZ(0)}
#far div{position:absolute;top:16px}#near-source{left:16px;width:64px;height:64px;background:red}#far-source{left:5200px;width:96px;height:96px;background:blue}
</style><div id="wide"></div><div id="clip"><div id="multiple"></div></div>
<div id="far"><div id="near-source"></div><div id="far-source"></div></div>"#;

#[cfg(feature = "backend-webkit-vulkan")]
const PENDING_COVER_HTML: &str = r#"<!doctype html><meta charset="utf-8"><title>cover-ready</title>
<style>html,body{margin:0;width:640px;height:480px;overflow:hidden;background:white}
#huge{position:absolute;left:-1000px;top:-1000px;width:4096px;height:4096px;will-change:transform;transform:translateZ(0);background:linear-gradient(to right,#20c040 0%,#20c040 60%,#3050d0 60%,#3050d0 100%)}
#hide,#show{position:fixed;left:520px;top:360px;width:90px;height:40px;z-index:2;background:#ddd}#show{top:420px}
</style><div id="huge"></div>
<div id="hide" onclick="document.getElementById('huge').style.transform='translate3d(100000px,0,0)';document.title='cover-hidden'">Hide</div>
<div id="show" onclick="document.getElementById('huge').style.transform='translateZ(0)';document.title='cover-returned'">Show</div>"#;

#[cfg(feature = "backend-webkit-vulkan")]
struct ContractGpuFrame {
    view: *mut myrica_webkit_sys::ScarletWebView,
    native: myrica_webkit_sys::GpuFrame,
}
#[cfg(feature = "backend-webkit-vulkan")]
impl Drop for ContractGpuFrame {
    fn drop(&mut self) {
        // These guards never outlive their creating view or leave its owner.
        unsafe {
            close_frame(self.native.handle);
            myrica_webkit_sys::scarlet_webview_release_gpu_frame(self.view, self.native.generation);
        }
    }
}

#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn compositor_snapshot(
    view: *mut myrica_webkit_sys::ScarletWebView,
) -> Result<myrica_webkit_sys::CompositingStatistics, String> {
    let mut statistics = myrica_webkit_sys::CompositingStatistics::default();
    let status =
        unsafe { myrica_webkit_sys::scarlet_webview_compositing_statistics(view, &mut statistics) };
    if status != myrica_webkit_sys::OK || statistics.enabled != 1 || statistics.reserved != 0 {
        return Err(format!(
            "GPU compositor unavailable: status={status} statistics={statistics:?}"
        ));
    }
    Ok(statistics)
}

#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn contract_navigation(
    view: *mut myrica_webkit_sys::ScarletWebView,
    location: &str,
    html: &str,
) -> Result<(), String> {
    use myrica_webkit_sys as ffi;
    let status = unsafe { ffi::scarlet_webview_navigate(view, ffi::Bytes::from_str(location)) };
    if status != ffi::OK {
        return Err(format!("navigate compositor fixture: {status}"));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = unsafe { ffi::scarlet_webview_tick(view) };
        if status != ffi::OK {
            return Err(format!("tick compositor navigation: {status}"));
        }
        let mut request = ffi::Request::default();
        let status = unsafe { ffi::scarlet_webview_poll_request(view, &mut request) };
        if status == ffi::OK && request.is_cancel == 0 {
            let header = ffi::Header {
                name: ffi::Bytes::from_str("Content-Type"),
                value: ffi::Bytes::from_str("text/html; charset=utf-8"),
            };
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
            let status = unsafe { ffi::scarlet_webview_complete_request(view, &response) };
            return if status == ffi::OK {
                Ok(())
            } else {
                Err(format!("complete compositor HTML: {status}"))
            };
        }
        if status != ffi::EMPTY && status != ffi::OK {
            return Err(format!("poll compositor request: {status}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err("compositor fixture produced no request".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn copy_contract_pixels(frame: &ContractGpuFrame) -> Result<Vec<u8>, String> {
    use myrica_webkit_sys as ffi;
    let mut pixels = ffi::Pixels::default();
    let status = unsafe {
        ffi::scarlet_webview_gpu_frame_readback(frame.view, frame.native.generation, &mut pixels)
    };
    let row = frame.native.width as usize * 4;
    if status != ffi::OK
        || pixels.data.is_null()
        || pixels.width != frame.native.width
        || pixels.height != frame.native.height
        || (pixels.stride as usize) < row
        || (pixels.stride as usize)
            .checked_mul(pixels.height as usize)
            .is_none_or(|size| size > pixels.length)
        || pixels.format != ffi::PIXEL_FORMAT_BGRA_PREMULTIPLIED
        || pixels.generation != frame.native.generation
    {
        return Err(format!("invalid compositor GPU readback: status={status}"));
    }
    let bytes = unsafe { std::slice::from_raw_parts(pixels.data, pixels.length) };
    let mut copied = Vec::with_capacity(row * pixels.height as usize);
    for source in bytes
        .chunks(pixels.stride as usize)
        .take(pixels.height as usize)
    {
        copied.extend_from_slice(&source[..row]);
    }
    Ok(copied)
}

#[cfg(feature = "backend-webkit-vulkan")]
fn contract_pixel(
    bytes: &[u8],
    width: u32,
    x: usize,
    y: usize,
    rgba: [u8; 4],
) -> Result<(), String> {
    let offset = (y * width as usize + x) * 4;
    let expected = [rgba[2], rgba[1], rgba[0], rgba[3]];
    if bytes.get(offset..offset + 4).is_some_and(|actual| {
        actual
            .iter()
            .zip(expected)
            .all(|(&a, b)| a.abs_diff(b) <= 1)
    }) {
        Ok(())
    } else {
        Err(format!(
            "compositor pixel ({x},{y}) expected RGBA {rgba:?}, got BGRA {:?}",
            bytes.get(offset..offset + 4)
        ))
    }
}

// Test the filtered output, including pixels outside the source rectangles.
// Deep interiors have exact colours; the gaussian fringe allows differences
// between Skia's supported blur kernels while rejecting white or unblurred edges.
#[cfg(feature = "backend-webkit-vulkan")]
fn verify_filter_pixels(bytes: &[u8], width: u32) -> Result<(), String> {
    contract_pixel(bytes, width, 56, 336, [255, 0, 0, 255])?;
    contract_pixel(bytes, width, 184, 328, [0, 0, 255, 255])?;
    contract_pixel(bytes, width, 248, 328, [0, 255, 0, 255])?;
    contract_pixel(bytes, width, 304, 328, [255; 4])?;
    for (x, y, rgba) in [
        (22, 336, [255, 166, 166, 255]),
        (222, 328, [166, 255, 166, 255]),
    ] {
        let offset = (y * width as usize + x) * 4;
        let expected = [rgba[2], rgba[1], rgba[0], rgba[3]];
        if !bytes.get(offset..offset + 4).is_some_and(|actual| {
            actual[3] == 255
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(&a, b)| a.abs_diff(b) <= 40)
        }) {
            return Err(format!(
                "CSS filter fringe ({x},{y}) expected RGBA {rgba:?} (+/-40), got BGRA {:?}",
                bytes.get(offset..offset + 4)
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn await_contract_frame(
    view: *mut myrica_webkit_sys::ScarletWebView,
    inspect: impl Fn(&[u8], u32) -> Result<(), String>,
) -> Result<(ContractGpuFrame, Vec<u8>), String> {
    use myrica_webkit_sys as ffi;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut last_error = String::from("no completed GPU frame");
    loop {
        let status = unsafe { ffi::scarlet_webview_tick(view) };
        if status != ffi::OK {
            return Err(format!("compositor tick: {status}"));
        }
        let mut native = ffi::GpuFrame::default();
        let status = unsafe { ffi::scarlet_webview_gpu_frame(view, &mut native) };
        if status == ffi::OK {
            let frame = ContractGpuFrame { view, native };
            let pixels = unsafe { copy_contract_pixels(&frame) }?;
            match inspect(&pixels, frame.native.width) {
                Ok(()) => return Ok((frame, pixels)),
                Err(error) => last_error = error,
            }
        } else if status != ffi::EMPTY {
            return Err(format!("acquire compositor GPU frame: {status}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "compositor output did not reach expected native state: {last_error}"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

// Drain only genuine pending rendering updates. Static fixtures have no timers
// or animations; an EMPTY acquire plus needs_paint=0 establishes quiescence.
#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn settle_contract_composition(
    view: *mut myrica_webkit_sys::ScarletWebView,
) -> Result<(), String> {
    use myrica_webkit_sys as ffi;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if unsafe { ffi::scarlet_webview_tick(view) } != ffi::OK {
            return Err("settle compositor tick failed".into());
        }
        let mut dirty = 1;
        if unsafe { ffi::scarlet_webview_needs_paint(view, &mut dirty) } != ffi::OK {
            return Err("query compositor pending work failed".into());
        }
        let mut native = ffi::GpuFrame::default();
        let status = unsafe { ffi::scarlet_webview_gpu_frame(view, &mut native) };
        if status == ffi::OK {
            drop(ContractGpuFrame { view, native });
        } else if status == ffi::EMPTY && dirty == 0 {
            return Ok(());
        } else if status != ffi::EMPTY {
            return Err(format!("settle compositor acquisition failed: {status}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err("static compositor did not drain actual pending work".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn filter_bounds_contract(
    view: *mut myrica_webkit_sys::ScarletWebView,
) -> Result<(), String> {
    use myrica_webkit_sys as ffi;
    check(
        unsafe { ffi::scarlet_webview_resize(view, 2560, 1440, 2.0) },
        "resize DPR2 filter bounds fixture",
    )?;
    unsafe {
        contract_navigation(
            view,
            "https://myrica.test/filter-bounds",
            FILTER_BOUNDS_HTML,
        )
    }?;
    let (frame, _) = unsafe {
        await_contract_frame(view, |pixels, width| {
            if width != 2560 || pixels.len() != 2560 * 1440 * 4 {
                return Err(format!(
                    "DPR2 filter frame extent mismatch: width={width} length={}",
                    pixels.len()
                ));
            }
            // All positions are physical pixels. The blue samples on both
            // sides of x=2048 must survive the old per-surface clamp boundary.
            for (x, y, rgba) in [
                (80, 120, [255, 0, 0, 255]),
                (1400, 120, [0, 0, 255, 255]),
                (2032, 120, [0, 0, 255, 255]),
                (2040, 120, [0, 0, 255, 255]),
                (2048, 120, [0, 0, 255, 255]),
                (2056, 120, [0, 0, 255, 255]),
                (2440, 120, [0, 255, 0, 255]),
                (1400, 264, [0, 0, 0, 255]), // Shadow below the source.
                (16, 120, [255; 4]),
                (256, 456, [255, 0, 0, 255]),
                (736, 456, [0, 255, 0, 255]),
                (256, 688, [0, 0, 255, 255]),
                (736, 688, [0, 0, 255, 255]), // Shadow of the first shadow.
                (256, 752, [0, 0, 255, 255]), // Inside the ancestor clip.
                (256, 784, [255; 4]),         // Outside the ancestor clip.
                (1000, 456, [255; 4]),
                (96, 896, [255, 0, 0, 255]),  // Visible near source.
                (496, 928, [0, 255, 0, 255]), // Shadow from x=5200 CSS source.
                (720, 896, [255; 4]),
            ] {
                contract_pixel(pixels, width, x, y, rgba)?;
            }
            // Blur must extend beyond the source and first shadow. These
            // one-sigma fringe pixels reject both white (missing outset) and
            // an opaque/unblurred rectangle; supported Skia kernels may differ.
            for (x, y, rgba) in [
                (92, 456, [255u8, 214, 214, 255]),
                (572, 456, [214, 255, 214, 255]),
            ] {
                let offset = (y * width as usize + x) * 4;
                let expected = [rgba[2], rgba[1], rgba[0], rgba[3]];
                if !pixels.get(offset..offset + 4).is_some_and(|actual| {
                    actual[3] == 255
                        && actual
                            .iter()
                            .zip(expected)
                            .all(|(&a, b)| a.abs_diff(b) <= 32)
                }) {
                    return Err(format!(
                        "multiple-filter fringe ({x},{y}) expected RGBA {rgba:?} (+/-32), got BGRA {:?}",
                        pixels.get(offset..offset + 4)
                    ));
                }
            }
            Ok(())
        })
    }?;
    drop(frame);
    unsafe { settle_contract_composition(view) }?;
    println!(
        "PASS: DPR2 filter 2496px source across 2048px boundary, multiple-shadow/blur outsets and ancestor clip, >10000px reverse-input downsample preserves far-source shadow"
    );
    Ok(())
}

#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn pending_cover_boundary_contract(
    view: *mut myrica_webkit_sys::ScarletWebView,
) -> Result<(), String> {
    use myrica_webkit_sys as ffi;
    if unsafe { ffi::scarlet_webview_resize(view, 640, 480, 1.0) } != ffi::OK {
        return Err("resize pending-cover fixture failed".into());
    }
    unsafe {
        contract_navigation(
            view,
            "https://myrica.test/pending-cover",
            PENDING_COVER_HTML,
        )
    }?;
    let before = unsafe { compositor_snapshot(view) }?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    // Observe the first real tile raster pass, without acquiring a final image:
    // acquire also flushes layers and could finish the incremental outer cover.
    // The 4096px gradient cannot use the solid-colour layer shortcut. Its
    // interior visible rect has prefetch cover on every side of the viewport.
    let first_pass = loop {
        if unsafe { ffi::scarlet_webview_tick(view) } != ffi::OK {
            return Err("tick pending-cover initial raster failed".into());
        }
        let statistics = unsafe { compositor_snapshot(view) }?;
        if statistics.tile_paints > before.tile_paints
            && statistics.tile_pixels.saturating_sub(before.tile_pixels) > 640 * 480
        {
            break statistics;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "large gradient did not create real backing tiles: {before:?} -> {statistics:?}"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    // Move it out immediately, while cover expansion has not been drained.
    // Empty visible geometry must clear old pending creation rather than
    // keeping needs_paint true forever.
    unsafe { click_fixture(view, 560.0, 380.0) }?;
    let (hidden, hidden_pixels) = unsafe {
        await_contract_frame(view, |pixels, width| {
            contract_pixel(pixels, width, 40, 40, [255; 4])
        })
    }?;
    unsafe { settle_contract_composition(view) }?;
    let empty_cover = unsafe { compositor_snapshot(view) }?;
    unsafe { click_fixture(view, 560.0, 440.0) }?;
    let (returned, _) = unsafe {
        await_contract_frame(view, |pixels, width| {
            contract_pixel(pixels, width, 40, 40, [32, 192, 64, 255])?;
            contract_pixel(pixels, width, 320, 240, [32, 192, 64, 255])
        })
    }?;
    if unsafe { copy_contract_pixels(&hidden) }? != hidden_pixels {
        return Err("returning visible cover mutated an old leased output".into());
    }
    drop(hidden);
    drop(returned);
    unsafe { settle_contract_composition(view) }?;
    let restored = unsafe { compositor_snapshot(view) }?;
    if restored.tile_paints <= empty_cover.tile_paints
        || restored.tile_pixels <= empty_cover.tile_pixels
    {
        return Err(format!(
            "returning visible cover omitted new tile work: {empty_cover:?} -> {restored:?}"
        ));
    }
    println!(
        "PASS: pending cover moved outside viewport drains to needs_paint=0/EMPTY, return paints new visible tiles; initial={first_pass:?} hidden={empty_cover:?} restored={restored:?}"
    );
    Ok(())
}

#[cfg(feature = "backend-webkit-vulkan")]
unsafe fn compositor_page_contract(
    view: *mut myrica_webkit_sys::ScarletWebView,
) -> Result<(), String> {
    use myrica_webkit_sys as ffi;
    if unsafe { ffi::scarlet_webview_resize(view, 640, 480, 1.0) } != ffi::OK {
        return Err("resize compositor fixture failed".into());
    }
    let before_fixture = unsafe { compositor_snapshot(view) }?;
    unsafe { contract_navigation(view, "https://myrica.test/compositor", COMPOSITOR_HTML) }?;
    let (first, first_pixels) = unsafe {
        await_contract_frame(view, |pixels, width| {
            for (x, y, colour) in [
                (40, 40, [255, 0, 0, 255]),
                (248, 40, [255, 128, 128, 255]),
                (280, 40, [128, 128, 255, 255]),
                (24, 144, [255; 4]),
                (60, 180, [0, 0, 255, 255]),
                (128, 190, [255; 4]),
                (176, 176, [255, 0, 0, 255]),
                (240, 176, [255; 4]),
                (400, 144, [255, 0, 0, 255]),
            ] {
                contract_pixel(pixels, width, x, y, colour)?;
            }
            verify_filter_pixels(pixels, width)
        })
    }?;
    println!("PASS: actual GPU CSS blur and drop-shadow source, shadow and gaussian fringe pixels");
    let first_id = unsafe { gpu_image_id(view, first.native.generation) }?;
    unsafe { settle_contract_composition(view) }?;
    let initial = unsafe { compositor_snapshot(view) }?;
    if initial.tile_paints <= before_fixture.tile_paints
        || initial.tile_pixels <= before_fixture.tile_pixels
    {
        return Err(format!(
            "initial GPU output omitted actual tile raster work: {initial:?}"
        ));
    }
    unsafe { click_fixture(view, 590.0, 336.0) }?;
    let (second, _) = unsafe {
        await_contract_frame(view, |pixels, width| {
            contract_pixel(pixels, width, 40, 40, [255; 4])?;
            contract_pixel(pixels, width, 136, 40, [255, 128, 128, 255])
        })
    }?;
    unsafe { settle_contract_composition(view) }?;
    let transformed = unsafe { compositor_snapshot(view) }?;
    if transformed != initial {
        return Err(format!(
            "transform/opacity rerasterized tiles: {initial:?} -> {transformed:?}"
        ));
    }
    if unsafe { copy_contract_pixels(&first) }? != first_pixels
        || unsafe { gpu_image_id(view, second.native.generation) }? == first_id
    {
        return Err("composition mutated a leased GPU image or reused its live allocation".into());
    }
    drop(first);
    drop(second);
    println!(
        "PASS: actual GPU compositor initial tiles, overlap group opacity, rounded clipping/mask, transform/opacity tile delta zero, immutable two leases"
    );
    unsafe { click_fixture(view, 590.0, 384.0) }?;
    let (small, _) = unsafe {
        await_contract_frame(view, |pixels, width| {
            contract_pixel(pixels, width, 400, 144, [0, 0, 255, 255])
        })
    }?;
    unsafe { settle_contract_composition(view) }?;
    let small_stats = unsafe { compositor_snapshot(view) }?;
    if small_stats != transformed {
        return Err(format!(
            "scroll within existing coverage rerasterized tiles: {transformed:?} -> {small_stats:?}"
        ));
    }
    drop(small);
    unsafe { click_fixture(view, 590.0, 432.0) }?;
    let (exposed, _) = unsafe {
        await_contract_frame(view, |pixels, width| {
            contract_pixel(pixels, width, 400, 64, [0, 255, 0, 255])
        })
    }?;
    unsafe { settle_contract_composition(view) }?;
    let exposed_stats = unsafe { compositor_snapshot(view) }?;
    if exposed_stats.tile_paints <= small_stats.tile_paints
        || exposed_stats.tile_pixels <= small_stats.tile_pixels
    {
        return Err(format!(
            "new scroll coverage did not rasterize new tiles: {small_stats:?} -> {exposed_stats:?}"
        ));
    }
    drop(exposed);
    // An idle page has no animation/timer. Owner ticks and snapshot queries must
    // not trigger either tile raster work or final-page GPU submissions.
    let idle_tiles = unsafe { compositor_snapshot(view) }?;
    let mut idle_paints = ffi::PaintStatistics::default();
    if unsafe { ffi::scarlet_webview_paint_statistics(view, &mut idle_paints) } != ffi::OK {
        return Err("query idle page paints failed".into());
    }
    for _ in 0..32 {
        if unsafe { ffi::scarlet_webview_tick(view) } != ffi::OK {
            return Err("idle compositor tick failed".into());
        }
        let mut idle = ffi::GpuFrame::default();
        let status = unsafe { ffi::scarlet_webview_gpu_frame(view, &mut idle) };
        if status != ffi::EMPTY {
            if status == ffi::OK {
                drop(ContractGpuFrame { view, native: idle });
            }
            return Err(format!(
                "idle compositor unexpectedly produced a GPU frame: {status}"
            ));
        }
    }
    let mut after_idle_paints = ffi::PaintStatistics::default();
    if unsafe { ffi::scarlet_webview_paint_statistics(view, &mut after_idle_paints) } != ffi::OK
        || after_idle_paints.paints != idle_paints.paints
        || after_idle_paints.painted_pixels != idle_paints.painted_pixels
        || unsafe { compositor_snapshot(view) }? != idle_tiles
    {
        return Err("idle owner polling produced raster or composition work".into());
    }
    println!(
        "PASS: scroll reuses cached tiles, new exposure paints new tiles, idle tile/page paint deltas zero"
    );
    // Keep the scroll page leased across native document teardown and resize.
    unsafe { click_fixture(view, 590.0, 384.0) }?;
    let (retired, retired_pixels) = unsafe {
        await_contract_frame(view, |pixels, width| {
            contract_pixel(pixels, width, 400, 144, [0, 0, 255, 255])
        })
    }?;
    let retired_id = unsafe { gpu_image_id(view, retired.native.generation) }?;
    unsafe {
        contract_navigation(
            view,
            "https://myrica.test/compositor-detached",
            "<!doctype html><style>html,body{margin:0;background:#2468ac}</style>",
        )
    }?;
    let (new_page, _) = unsafe {
        await_contract_frame(view, |pixels, width| {
            contract_pixel(pixels, width, 300, 300, [36, 104, 172, 255])
        })
    }?;
    if unsafe { copy_contract_pixels(&retired) }? != retired_pixels {
        return Err("navigation mutated an old leased compositor frame".into());
    }
    drop(new_page);
    if unsafe { ffi::scarlet_webview_resize(view, 704, 512, 1.0) } != ffi::OK {
        return Err("resize detached compositor failed".into());
    }
    let (resized, _) = unsafe {
        await_contract_frame(view, |pixels, width| {
            contract_pixel(pixels, width, 300, 300, [36, 104, 172, 255])
        })
    }?;
    if resized.native.width != 704
        || resized.native.height != 512
        || unsafe { gpu_image_id(view, resized.native.generation) }? == retired_id
        || unsafe { copy_contract_pixels(&retired) }? != retired_pixels
    {
        return Err("resize reused or mutated a retired compositor allocation".into());
    }
    let final_stats = unsafe { compositor_snapshot(view) }?;
    if final_stats.tile_paints < exposed_stats.tile_paints
        || final_stats.tile_pixels < exposed_stats.tile_pixels
    {
        return Err("view-lifetime tile counters reset on navigation/resize".into());
    }
    drop(retired);
    drop(resized);
    println!(
        "PASS: navigation detaches old compositor tree, resize preserves immutable leases and cumulative tile metrics"
    );
    Ok(())
}

#[cfg(all(test, feature = "backend-webkit-vulkan"))]
mod filter_pixel_tests {
    use super::verify_filter_pixels;

    #[test]
    fn filter_contract_rejects_missing_filter_outputs() {
        let width = 640;
        let mut pixels = vec![255; width * 480 * 4];
        for (x, y, bgra) in [
            (56, 336, [0, 0, 255, 255]),
            (184, 328, [255, 0, 0, 255]),
            (248, 328, [0, 255, 0, 255]),
            (22, 336, [166, 166, 255, 255]),
            (222, 328, [166, 255, 166, 255]),
        ] {
            let offset = (y * width + x) * 4;
            pixels[offset..offset + 4].copy_from_slice(&bgra);
        }
        assert!(verify_filter_pixels(&pixels, width as u32).is_ok());
        for (x, y) in [(248, 328), (22, 336), (222, 328)] {
            let offset = (y * width + x) * 4;
            let previous = pixels[offset..offset + 4].to_vec();
            pixels[offset..offset + 4].fill(255);
            assert!(verify_filter_pixels(&pixels, width as u32).is_err());
            pixels[offset..offset + 4].copy_from_slice(&previous);
        }
    }
}
