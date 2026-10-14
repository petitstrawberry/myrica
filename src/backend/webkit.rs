//! Persistent native WebCore backend. WebCore owns the document and resource policy;
//! Myrica supplies HTTP transport and presents native page images through SGFX.

use std::collections::HashSet;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use myrica_webkit_sys as ffi;

use super::{
    BackendError, BrowserBackend, BrowserFrame, BrowserInput, BrowserKey, BrowserModifiers,
    BrowserMouseButton, BrowserSnapshot, LoadState, WakeCallback,
};

mod frame;
mod network;
mod pixels;

pub struct WebKitBackend {
    view: NonNull<ffi::ScarletWebView>,
    network: network::NetworkService,
    renderer: frame::FrameRenderer,
    wake: WakeCallback,
    snapshot: BrowserSnapshot,
    in_flight: HashSet<u64>,
    width: u32,
    height: u32,
    scale: f64,
    modifiers: u32,
    last_frame: u64,
    #[cfg(feature = "backend-webkit-vulkan")]
    frame_releaser: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    #[cfg(feature = "backend-webkit-vulkan")]
    gpu_logged: bool,
    last_click: Option<(BrowserMouseButton, f32, f32, Instant, u32)>,
    pressed_keys: Vec<BrowserKey>,
    suppress_next_text: bool,
}

impl WebKitBackend {
    pub fn new(wake: WakeCallback) -> Result<Self, BackendError> {
        let network = network::NetworkService::new(Arc::clone(&wake))?;
        let font_config = std::env::var("MYRICA_WEBKIT_FONT_CONFIG")
            .unwrap_or_else(|_| String::from("/etc/fonts/fonts.conf"));
        // SAFETY: Backend construction takes place on its dedicated owner thread;
        // the path is borrowed only for the initialization call.
        check(
            unsafe { ffi::scarlet_webview_initialize(ffi::Bytes::from_str(&font_config)) },
            "initialize WebCore",
        )?;
        let configuration = ffi::Configuration {
            abi_version: ffi::ABI_VERSION,
            width: 960,
            height: 640,
            script_enabled: 1,
            device_scale_factor: 1.0,
        };
        let mut view = std::ptr::null_mut();
        // SAFETY: Both input and output POD structs remain valid during the call.
        let status = unsafe { ffi::scarlet_webview_create(&configuration, &mut view) };
        if let Err(error) = check(status, "create WebCore view") {
            // SAFETY: Initialization succeeded and no live view was returned.
            let status = unsafe { ffi::scarlet_webview_shutdown() };
            if status != ffi::OK {
                eprintln!(
                    "[myrica:webkit] failed initialization could not shut down (status {status})"
                );
                std::process::abort();
            }
            return Err(error);
        }
        let Some(view) = NonNull::new(view) else {
            // A successful create must return a valid native owner. Continuing
            // without one would leave initialized C++ globals unaccounted for.
            eprintln!("[myrica:webkit] successful create returned a null native view");
            std::process::abort();
        };
        Ok(Self {
            view,
            network,
            renderer: frame::FrameRenderer::new(),
            wake,
            snapshot: BrowserSnapshot {
                backend_name: "WebKit",
                url: String::new(),
                title: String::from("Myrica"),
                load_state: LoadState::Idle,
                can_go_back: false,
                can_go_forward: false,
                text_input: None,
            },
            in_flight: HashSet::new(),
            width: 960,
            height: 640,
            scale: 1.0,
            modifiers: 0,
            last_frame: 0,
            #[cfg(feature = "backend-webkit-vulkan")]
            frame_releaser: None,
            #[cfg(feature = "backend-webkit-vulkan")]
            gpu_logged: false,
            last_click: None,
            pressed_keys: Vec::new(),
            suppress_next_text: false,
        })
    }

    /// Inspect the engine's actual raster work for native integration checks.
    pub fn paint_statistics(&mut self) -> Result<ffi::PaintStatistics, BackendError> {
        let mut statistics = ffi::PaintStatistics::default();
        // SAFETY: This native view belongs to the current owner thread.
        check(
            unsafe { ffi::scarlet_webview_paint_statistics(self.view.as_ptr(), &mut statistics) },
            "inspect WebCore paint statistics",
        )?;
        Ok(statistics)
    }

    /// Diagnostic-only GPU readback for native integration probes. Browser
    /// presentation continues to transfer shared images and never calls this.
    /// The caller must keep this frame's owner lease alive until this returns.
    #[cfg(feature = "backend-webkit-vulkan")]
    pub fn diagnostic_gpu_frame_rgba(
        &mut self,
        frame: &BrowserFrame,
    ) -> Result<Vec<u8>, BackendError> {
        let lease = frame
            .lease
            .as_ref()
            .ok_or_else(|| BackendError::new("GPU diagnostic frame has no lease"))?;
        let mut pixels = ffi::Pixels::default();
        // SAFETY: this is the native owner; frame keeps its lease alive. Copy
        // the borrowed result before another native readback/return can occur.
        check(
            unsafe {
                ffi::scarlet_webview_gpu_frame_readback(
                    self.view.as_ptr(),
                    lease.token,
                    &mut pixels,
                )
            },
            "diagnostic WebCore GPU readback",
        )?;
        if pixels.generation != frame.revision
            || pixels.width != frame.width
            || pixels.height != frame.height
            || pixels.format != ffi::PIXEL_FORMAT_BGRA_PREMULTIPLIED
            || pixels.data.is_null()
            || pixels.length > pixels::MAX_FRAME_BYTES
            || pixels.length > isize::MAX as usize
        {
            return Err(BackendError::new(
                "GPU diagnostic readback has invalid metadata",
            ));
        }
        // SAFETY: validated non-null owner-borrowed C ABI buffer and bounded length.
        let bytes = unsafe { std::slice::from_raw_parts(pixels.data, pixels.length) };
        pixels::convert_bgra(pixels.width, pixels.height, pixels.stride as usize, bytes)
            .map_err(BackendError::new)
    }

    /// Read actual DOM text without running host-supplied JavaScript.
    pub fn text_content(&mut self, id: &str) -> Result<Option<String>, BackendError> {
        let mut text = ffi::Bytes::empty();
        // SAFETY: The view belongs to this owner; output is cloned before another call.
        let status = unsafe {
            ffi::scarlet_webview_text_content(
                self.view.as_ptr(),
                ffi::Bytes::from_str(id),
                &mut text,
            )
        };
        if status == ffi::EMPTY {
            return Ok(None);
        }
        check(status, "inspect WebCore DOM text")?;
        Ok(Some(read_string(text)?))
    }

    fn start_navigation(&mut self, location: &str) -> Result<(), BackendError> {
        let url = normalize_location(location)?;
        // SAFETY: This live view and borrowed URL are used only on their owner.
        check(
            unsafe {
                ffi::scarlet_webview_navigate(
                    self.view.as_ptr(),
                    ffi::Bytes::from_str(url.as_str()),
                )
            },
            "navigate WebCore",
        )?;
        self.snapshot.url = url.to_string();
        self.snapshot.load_state = LoadState::Loading;
        self.snapshot.text_input = None;
        (self.wake)();
        Ok(())
    }

    fn fail(&mut self, error: impl std::fmt::Display) {
        let message = error.to_string();
        eprintln!("[myrica:webkit] {message}");
        self.snapshot.load_state = LoadState::Failed(message);
    }

    fn complete_error(&mut self, request_id: u64, message: &str) -> Result<(), BackendError> {
        let response = ffi::Response {
            request_id,
            transport_error: 1,
            error_message: ffi::Bytes::from_str(message),
            ..Default::default()
        };
        // SAFETY: Response spans are valid until this synchronous call returns.
        let result =
            unsafe { ffi::scarlet_webview_complete_request(self.view.as_ptr(), &response) };
        if result == ffi::STALE_REQUEST {
            return Ok(());
        }
        check(result, "fail WebCore resource")
    }

    fn poll_requests(&mut self) -> Result<(), BackendError> {
        // Bound each turn so a resource-heavy page cannot starve input or paint.
        for _ in 0..128 {
            let mut request = ffi::Request::default();
            // SAFETY: Output POD is writable; returned spans are copied before
            // the next poll or before leaving this owner thread.
            let result =
                unsafe { ffi::scarlet_webview_poll_request(self.view.as_ptr(), &mut request) };
            if result == ffi::EMPTY {
                break;
            }
            check(result, "poll WebCore resource")?;
            if request.is_cancel != 0 {
                self.in_flight.remove(&request.request_id);
                self.network.cancel(request.request_id);
                continue;
            }
            match clone_request(&request) {
                Ok(transport) => {
                    self.in_flight.insert(request.request_id);
                    self.network.submit(request.request_id, transport);
                }
                Err(error) => self.complete_error(request.request_id, &error.to_string())?,
            }
        }
        Ok(())
    }

    fn drain_completions(&mut self) -> Result<bool, BackendError> {
        let mut changed = false;
        for _ in 0..128 {
            let Some(completion) = self.network.try_recv() else {
                break;
            };
            if !self.in_flight.remove(&completion.id) {
                continue;
            }
            changed = true;
            match completion.result {
                Err(error) => self.complete_error(completion.id, &error)?,
                Ok(response) => {
                    let headers: Vec<_> = response
                        .headers
                        .iter()
                        .map(|(name, value)| ffi::Header {
                            name: ffi::Bytes::from_str(name),
                            value: ffi::Bytes::from_str(value),
                        })
                        .collect();
                    let native = ffi::Response {
                        request_id: completion.id,
                        status_code: u32::from(response.status),
                        final_url: ffi::Bytes::from_str(&response.final_url),
                        status_text: ffi::Bytes::from_str(&response.status_text),
                        headers: headers.as_ptr(),
                        header_count: headers.len(),
                        body: ffi::Bytes::from_slice(&response.body),
                        ..Default::default()
                    };
                    // SAFETY: All spans refer to owned response data kept alive
                    // through this call. No network thread enters WebCore.
                    let result = unsafe {
                        ffi::scarlet_webview_complete_request(self.view.as_ptr(), &native)
                    };
                    if result != ffi::STALE_REQUEST {
                        check(result, "complete WebCore resource")?;
                    }
                }
            }
        }
        Ok(changed)
    }

    fn refresh_state(&mut self) -> Result<bool, BackendError> {
        let mut state = ffi::State::default();
        // SAFETY: State output belongs to this view and is copied before mutation.
        check(
            unsafe { ffi::scarlet_webview_state(self.view.as_ptr(), &mut state) },
            "read WebCore state",
        )?;
        let url = read_string(state.url)?;
        let title = read_string(state.title)?;
        let error = read_string(state.last_error)?;
        let before = self.snapshot.clone();
        let changed = state.frame_generation != self.last_frame;
        self.last_frame = state.frame_generation;
        if !url.is_empty() {
            self.snapshot.url = url;
        }
        self.snapshot.title = title;
        self.snapshot.load_state = if !error.is_empty() {
            LoadState::Failed(error)
        } else if state.loading != 0 {
            LoadState::Loading
        } else {
            LoadState::Ready
        };
        self.snapshot.can_go_back = state.can_go_back != 0;
        self.snapshot.can_go_forward = state.can_go_forward != 0;
        Ok(changed || self.snapshot != before)
    }

    fn native_navigation(
        &mut self,
        name: &str,
        function: unsafe extern "C" fn(*mut ffi::ScarletWebView) -> i32,
    ) {
        // SAFETY: Navigation uses genuine WebCore HistoryItems and runs only on
        // this persistent view's owner thread.
        let result = unsafe { function(self.view.as_ptr()) };
        if result == ffi::EMPTY {
            return;
        }
        match check(result, name) {
            Ok(()) => {
                self.snapshot.load_state = LoadState::Loading;
                (self.wake)();
            }
            Err(error) => self.fail(error),
        }
    }

    fn mouse(
        &mut self,
        kind: u32,
        button: u32,
        x: f32,
        y: f32,
        clicks: u32,
    ) -> Result<bool, BackendError> {
        let x = f64::from(x) * self.scale;
        let y = f64::from(y) * self.scale;
        let event = ffi::MouseEvent {
            kind,
            button,
            modifiers: self.modifiers,
            click_count: clicks,
            x,
            y,
            global_x: x,
            global_y: y,
        };
        let mut handled = 0;
        // SAFETY: Event POD is borrowed for this owner-thread call only.
        check(
            unsafe { ffi::scarlet_webview_mouse(self.view.as_ptr(), &event, &mut handled) },
            "WebCore mouse input",
        )?;
        Ok(handled != 0)
    }

    fn keyboard(
        &mut self,
        key: BrowserKey,
        kind: u32,
        modifiers: BrowserModifiers,
        text: &str,
    ) -> Result<bool, BackendError> {
        let mapped = map_key(key);
        self.modifiers = map_modifiers(modifiers);
        let auto_repeat = kind == 0 && self.pressed_keys.contains(&key);
        if kind == 0 && !auto_repeat {
            self.pressed_keys.push(key);
        } else if kind == 1 {
            self.pressed_keys.retain(|pressed| *pressed != key);
        }
        let event = ffi::KeyEvent {
            kind,
            modifiers: self.modifiers,
            windows_virtual_key: mapped.virtual_key,
            auto_repeat: u32::from(auto_repeat),
            text: ffi::Bytes::from_str(text),
            unmodified_text: ffi::Bytes::from_str(text),
            key: ffi::Bytes::from_str(&mapped.key),
            code: ffi::Bytes::from_str(&mapped.code),
            key_identifier: ffi::Bytes::from_str(&mapped.identifier),
        };
        let mut handled = 0;
        // SAFETY: Strings and event POD remain alive for the synchronous call.
        check(
            unsafe { ffi::scarlet_webview_key(self.view.as_ptr(), &event, &mut handled) },
            "WebCore keyboard input",
        )?;
        Ok(handled != 0)
    }
}

impl BrowserBackend for WebKitBackend {
    fn shutdown_callback(&self) -> Option<WakeCallback> {
        Some(Arc::new(|| {
            // SAFETY: This API alone is explicitly concurrent-safe. It raises
            // JSC's real termination trap without entering the owner-only Page.
            let status = unsafe { ffi::scarlet_webview_request_termination() };
            if status != ffi::OK || std::env::var_os("MYRICA_WEBKIT_TRACE_SHUTDOWN").is_some() {
                eprintln!("[myrica:webkit] native JavaScript termination status={status}");
            }
        }))
    }

    fn navigate(&mut self, location: &str) -> Result<(), BackendError> {
        let result = self.start_navigation(location);
        if let Err(error) = &result {
            self.fail(error);
        }
        result
    }

    fn reload(&mut self) {
        self.native_navigation("reload WebCore", ffi::scarlet_webview_reload);
    }

    fn go_back(&mut self) {
        self.native_navigation("go back in WebCore", ffi::scarlet_webview_back);
    }

    fn go_forward(&mut self) {
        self.native_navigation("go forward in WebCore", ffi::scarlet_webview_forward);
    }

    fn tick(&mut self) -> bool {
        let result = (|| {
            self.poll_requests()?;
            let completed = self.drain_completions()?;
            // SAFETY: Native timers/layout are advanced only on the view owner.
            check(
                unsafe { ffi::scarlet_webview_tick(self.view.as_ptr()) },
                "tick WebCore",
            )?;
            self.poll_requests()?;
            let mut needs_paint = 0;
            // SAFETY: Queries pending damage without rasterizing on a network wake.
            check(
                unsafe { ffi::scarlet_webview_needs_paint(self.view.as_ptr(), &mut needs_paint) },
                "query pending WebCore paint",
            )?;
            Ok::<_, BackendError>(completed | self.refresh_state()? | (needs_paint != 0))
        })();
        match result {
            Ok(changed) => changed,
            Err(error) => {
                self.fail(error);
                true
            }
        }
    }

    #[cfg(feature = "backend-webkit-vulkan")]
    fn set_frame_releaser(&mut self, release: Arc<dyn Fn(u64) + Send + Sync>) {
        self.frame_releaser = Some(release);
    }

    #[cfg(feature = "backend-webkit-vulkan")]
    fn release_frame(&mut self, token: u64) {
        // SAFETY: returns a lease on this view's owner, across navigation/resize.
        let status = unsafe { ffi::scarlet_webview_release_gpu_frame(self.view.as_ptr(), token) };
        if let Err(error) = check(status, "return WebCore GPU image") {
            self.fail(error);
        }
    }

    #[cfg(feature = "backend-webkit-vulkan")]
    fn can_render(&mut self) -> bool {
        let mut capacity = 0;
        // SAFETY: queries this live owner-thread view without painting.
        let status =
            unsafe { ffi::scarlet_webview_gpu_frame_capacity(self.view.as_ptr(), &mut capacity) };
        match check(status, "query WebCore GPU capacity") {
            Ok(()) => capacity > 0,
            Err(error) => {
                self.fail(error);
                false
            }
        }
    }

    #[cfg(feature = "backend-webkit-vulkan")]
    fn render(&mut self, width: u32, height: u32, scale: f32) -> BrowserFrame {
        self.render_if_ready(width, height, scale)
            .unwrap_or_else(|| BrowserFrame::empty(width, height))
    }

    #[cfg(feature = "backend-webkit-vulkan")]
    fn render_if_ready(&mut self, width: u32, height: u32, scale: f32) -> Option<BrowserFrame> {
        let result = (|| {
            let scale = f64::from(scale);
            if width != self.width || height != self.height || scale != self.scale {
                // SAFETY: This view is live and confined to its owner thread.
                check(
                    unsafe {
                        ffi::scarlet_webview_resize(self.view.as_ptr(), width, height, scale)
                    },
                    "resize WebCore",
                )?;
                self.width = width;
                self.height = height;
                self.scale = scale;
                check(
                    unsafe { ffi::scarlet_webview_tick(self.view.as_ptr()) },
                    "paint resized WebCore",
                )?;
            }
            let mut image = ffi::GpuFrame::default();
            // SAFETY: the returned handle is newly owned; the lease remains in
            // WebCore until UI replacement/discard queues its owner-thread return.
            let status = unsafe { ffi::scarlet_webview_gpu_frame(self.view.as_ptr(), &mut image) };
            if status == ffi::EMPTY {
                return Ok(None);
            }
            check(status, "paint WebCore GPU page")?;
            let Some(release) = self.frame_releaser.as_ref() else {
                drop(super::frame::OwnedGpuImage::new(image.handle));
                // SAFETY: a just-acquired frame has never reached a consumer.
                unsafe {
                    ffi::scarlet_webview_release_gpu_frame(self.view.as_ptr(), image.generation);
                }
                return Err(BackendError::new(
                    "GPU frame return queue was not installed",
                ));
            };
            let frame = self.renderer.gpu_frame(image, Arc::clone(release))?;
            if frame.width != width || frame.height != height {
                return Err(BackendError::new(
                    "WebCore returned a mismatched GPU page extent",
                ));
            }
            self.last_frame = frame.revision;
            if !self.gpu_logged {
                eprintln!(
                    "[myrica:webkit] page renderer=skia-vulkan presentation=shared-bgra8 cpu-framebuffer=none"
                );
                self.gpu_logged = true;
            }
            Ok::<_, BackendError>(Some(frame))
        })();
        match result {
            Ok(frame) => frame,
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    #[cfg(not(feature = "backend-webkit-vulkan"))]
    fn render(&mut self, width: u32, height: u32, scale: f32) -> BrowserFrame {
        let result = (|| {
            let scale = f64::from(scale);
            if width != self.width || height != self.height || scale != self.scale {
                // SAFETY: This view is live and confined to its owner thread.
                check(
                    unsafe {
                        ffi::scarlet_webview_resize(self.view.as_ptr(), width, height, scale)
                    },
                    "resize WebCore",
                )?;
                self.width = width;
                self.height = height;
                self.scale = scale;
                check(
                    unsafe { ffi::scarlet_webview_tick(self.view.as_ptr()) },
                    "paint resized WebCore",
                )?;
            }
            let mut pixels = ffi::Pixels::default();
            // SAFETY: Output is copied into a Rust-owned texture before mutation.
            check(
                unsafe { ffi::scarlet_webview_pixels(self.view.as_ptr(), &mut pixels) },
                "read WebCore pixels",
            )?;
            if pixels.format != ffi::PIXEL_FORMAT_BGRA_PREMULTIPLIED
                || pixels.width != width
                || pixels.height != height
            {
                return Err(BackendError::new(
                    "WebCore returned a mismatched pixel surface",
                ));
            }
            self.last_frame = pixels.generation;
            if let Some(frame) =
                self.renderer
                    .cached_frame(pixels.generation, width, height, pixels.stride as usize)
            {
                return Ok(frame);
            }
            let bytes = copy_span(ffi::Bytes {
                data: pixels.data,
                length: pixels.length,
            })?;
            self.renderer.frame(
                pixels.generation,
                width,
                height,
                pixels.stride as usize,
                &bytes,
            )
        })();
        match result {
            Ok(frame) => frame,
            Err(error) => {
                self.fail(error);
                BrowserFrame::empty(width, height)
            }
        }
    }

    fn handle_input(&mut self, input: BrowserInput) -> bool {
        let result = (|| match input {
            BrowserInput::PointerMoved { x, y } => self.mouse(0, 0, x, y, 0),
            BrowserInput::PointerExited => self.mouse(0, 0, -1.0, -1.0, 0),
            BrowserInput::PointerButton {
                button,
                pressed,
                x,
                y,
            } => {
                let mut clicks = self
                    .last_click
                    .filter(|(previous, ..)| *previous == button)
                    .map_or(1, |(_, _, _, _, count)| count);
                if pressed {
                    clicks = 1;
                    // SAFETY: Focus belongs to the same persistent native view.
                    check(
                        unsafe { ffi::scarlet_webview_focus(self.view.as_ptr(), 1) },
                        "focus WebCore",
                    )?;
                    let now = Instant::now();
                    if let Some((previous, px, py, at, count)) = self.last_click {
                        if previous == button
                            && now.duration_since(at) < Duration::from_millis(500)
                            && (x - px).abs() < 4.0
                            && (y - py).abs() < 4.0
                        {
                            clicks = count % 3 + 1;
                        }
                    }
                    self.last_click = Some((button, x, y, now, clicks));
                }
                self.mouse(
                    if pressed { 1 } else { 2 },
                    match button {
                        BrowserMouseButton::Primary => 1,
                        BrowserMouseButton::Auxiliary => 2,
                        BrowserMouseButton::Secondary => 3,
                    },
                    x,
                    y,
                    clicks,
                )
            }
            BrowserInput::Wheel {
                delta_x,
                delta_y,
                x,
                y,
            } => {
                let event = ffi::WheelEvent {
                    modifiers: self.modifiers,
                    x: f64::from(x) * self.scale,
                    y: f64::from(y) * self.scale,
                    // ScarletUI and BrowserInput use positive right/down;
                    // WebCore's platform wheel event uses positive left/up.
                    delta_x: -delta_x * self.scale,
                    delta_y: -delta_y * self.scale,
                    global_x: f64::from(x) * self.scale,
                    global_y: f64::from(y) * self.scale,
                };
                let mut handled = 0;
                // SAFETY: Event POD is borrowed by the native owner-thread call.
                check(
                    unsafe { ffi::scarlet_webview_wheel(self.view.as_ptr(), &event, &mut handled) },
                    "WebCore wheel input",
                )?;
                Ok(handled != 0)
            }
            BrowserInput::Key {
                key,
                pressed,
                modifiers,
            } => {
                let handled = self.keyboard(key, if pressed { 0 } else { 1 }, modifiers, "")?;
                if pressed {
                    self.suppress_next_text = handled;
                    // ScarletUI sends printable Char events separately, but
                    // excludes control characters. WebCore's native editor
                    // defers Enter's newline/form behavior to keypress.
                    if key == BrowserKey::Enter && !handled {
                        return self.keyboard(key, 2, modifiers, "\r");
                    }
                }
                Ok(handled)
            }
            BrowserInput::Text(character) => {
                if std::mem::take(&mut self.suppress_next_text) {
                    // A consumed/prevented keydown must suppress its following
                    // keypress and default text insertion, as EventHandler does
                    // for platform events that combine both phases.
                    return Ok(true);
                }
                self.keyboard(
                    BrowserKey::Character(character),
                    2,
                    unmap_modifiers(self.modifiers),
                    &character.to_string(),
                )
            }
            BrowserInput::ImeCommit(text) => {
                let mut consumed = false;
                for character in text.chars() {
                    consumed |= self.keyboard(
                        BrowserKey::Character(character),
                        2,
                        unmap_modifiers(self.modifiers),
                        &character.to_string(),
                    )?;
                }
                Ok(consumed)
            }
            BrowserInput::ImePreedit { .. } => Ok(false),
            BrowserInput::FocusLost => {
                self.modifiers = 0;
                self.pressed_keys.clear();
                self.suppress_next_text = false;
                // SAFETY: Focus is cleared on the live native view's owner.
                check(
                    unsafe { ffi::scarlet_webview_focus(self.view.as_ptr(), 0) },
                    "blur WebCore",
                )?;
                Ok(true)
            }
        })();
        match result {
            Ok(handled) => handled,
            Err(error) => {
                self.fail(error);
                false
            }
        }
    }

    fn snapshot(&self) -> BrowserSnapshot {
        self.snapshot.clone()
    }
}

impl Drop for WebKitBackend {
    fn drop(&mut self) {
        for id in self.in_flight.drain() {
            self.network.cancel(id);
        }
        // SAFETY: The worker drops this backend on its creating thread, after
        // all callbacks have returned. Native destruction performs real teardown.
        let result = unsafe { ffi::scarlet_webview_destroy(self.view.as_ptr()) };
        if result != ffi::OK {
            eprintln!("[myrica:webkit] destroy failed: {result}");
            std::process::abort();
        }
        // SAFETY: Myrica owns one view on this engine worker, now destroyed.
        let result = unsafe { ffi::scarlet_webview_shutdown() };
        if result != ffi::OK {
            eprintln!("[myrica:webkit] shutdown failed: {result}");
            std::process::abort();
        }
    }
}

fn check(code: i32, operation: &str) -> Result<(), BackendError> {
    if code == ffi::OK {
        Ok(())
    } else {
        Err(BackendError::new(format!(
            "{operation} failed (WebCore status {code})"
        )))
    }
}

fn copy_span(span: ffi::Bytes) -> Result<Vec<u8>, BackendError> {
    if span.length == 0 {
        return Ok(Vec::new());
    }
    if span.data.is_null() || span.length > 128 * 1024 * 1024 {
        return Err(BackendError::new("WebCore returned an invalid byte span"));
    }
    // SAFETY: The trusted C ABI promises readable bytes until the next mutation.
    // Copy them immediately; no borrowed view data escapes this helper.
    Ok(unsafe { std::slice::from_raw_parts(span.data, span.length) }.to_vec())
}

fn read_string(span: ffi::Bytes) -> Result<String, BackendError> {
    String::from_utf8(copy_span(span)?)
        .map_err(|_| BackendError::new("WebCore returned invalid UTF-8"))
}

fn clone_request(request: &ffi::Request) -> Result<network::Request, BackendError> {
    if request.header_count > 4096 || (request.header_count != 0 && request.headers.is_null()) {
        return Err(BackendError::new(
            "WebCore returned invalid request headers",
        ));
    }
    let headers = if request.header_count == 0 {
        &[][..]
    } else {
        // SAFETY: Header array is view-owned until the next poll_request call.
        unsafe { std::slice::from_raw_parts(request.headers, request.header_count) }
    };
    Ok(network::Request {
        method: read_string(request.method)?,
        url: read_string(request.url)?,
        headers: headers
            .iter()
            .map(|header| Ok((read_string(header.name)?, read_string(header.value)?)))
            .collect::<Result<_, BackendError>>()?,
        body: copy_span(request.body)?,
    })
}

fn normalize_location(location: &str) -> Result<reqwest::Url, BackendError> {
    let location = location.trim();
    if location.is_empty() {
        return Err(BackendError::new("the address is empty"));
    }
    let candidate = if location.contains("://") {
        location.to_owned()
    } else if location.starts_with("localhost")
        || location.starts_with("127.")
        || location.starts_with("0.0.0.0")
        || location.starts_with('[')
    {
        format!("http://{location}")
    } else {
        format!("https://{location}")
    };
    let url = reqwest::Url::parse(&candidate)
        .map_err(|error| BackendError::new(format!("invalid address: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BackendError::new(format!(
            "unsupported URL scheme: {}",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BackendError::new(
            "URL-embedded credentials are unsupported by the WebKit transport",
        ));
    }
    Ok(url)
}

fn map_modifiers(modifiers: BrowserModifiers) -> u32 {
    u32::from(modifiers.shift) * ffi::SHIFT
        | u32::from(modifiers.control) * ffi::CONTROL
        | u32::from(modifiers.alt) * ffi::ALT
        | u32::from(modifiers.super_key) * ffi::META
}

fn unmap_modifiers(modifiers: u32) -> BrowserModifiers {
    BrowserModifiers {
        shift: modifiers & ffi::SHIFT != 0,
        control: modifiers & ffi::CONTROL != 0,
        alt: modifiers & ffi::ALT != 0,
        super_key: modifiers & ffi::META != 0,
    }
}

struct KeyMapping {
    key: String,
    code: String,
    identifier: String,
    virtual_key: u32,
}

fn map_key(key: BrowserKey) -> KeyMapping {
    let (name, code, identifier, virtual_key) = match key {
        BrowserKey::Escape => ("Escape", "Escape", "U+001B", 27),
        BrowserKey::Enter => ("Enter", "Enter", "Enter", 13),
        BrowserKey::Tab => ("Tab", "Tab", "U+0009", 9),
        BrowserKey::Backspace => ("Backspace", "Backspace", "U+0008", 8),
        BrowserKey::Space => (" ", "Space", "U+0020", 32),
        BrowserKey::Left => ("ArrowLeft", "ArrowLeft", "Left", 37),
        BrowserKey::Right => ("ArrowRight", "ArrowRight", "Right", 39),
        BrowserKey::Up => ("ArrowUp", "ArrowUp", "Up", 38),
        BrowserKey::Down => ("ArrowDown", "ArrowDown", "Down", 40),
        BrowserKey::Home => ("Home", "Home", "Home", 36),
        BrowserKey::End => ("End", "End", "End", 35),
        BrowserKey::PageUp => ("PageUp", "PageUp", "PageUp", 33),
        BrowserKey::PageDown => ("PageDown", "PageDown", "PageDown", 34),
        BrowserKey::Insert => ("Insert", "Insert", "U+002D", 45),
        BrowserKey::Delete => ("Delete", "Delete", "U+007F", 46),
        BrowserKey::Unknown => ("Unidentified", "Unidentified", "", 0),
        BrowserKey::Character(character) => {
            let upper = character.to_ascii_uppercase();
            let code = if upper.is_ascii_alphabetic() {
                format!("Key{upper}")
            } else if upper.is_ascii_digit() {
                format!("Digit{upper}")
            } else {
                String::from("Unidentified")
            };
            return KeyMapping {
                key: character.to_string(),
                code,
                identifier: format!("U+{:04X}", character as u32),
                virtual_key: if upper.is_ascii_alphanumeric() {
                    upper as u32
                } else {
                    0
                },
            };
        }
    };
    KeyMapping {
        key: name.into(),
        code: code.into(),
        identifier: identifier.into(),
        virtual_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_http_and_https_only() {
        assert_eq!(
            normalize_location("ichigo.dev").unwrap().as_str(),
            "https://ichigo.dev/"
        );
        assert_eq!(
            normalize_location("127.0.0.1:8080/test").unwrap().as_str(),
            "http://127.0.0.1:8080/test"
        );
        assert!(normalize_location("file:///etc/passwd").is_err());
        assert!(normalize_location("javascript://example.com/alert(1)").is_err());
        assert!(normalize_location("https://user:password@example.com/").is_err());
        assert!(normalize_location("").is_err());
    }

    #[test]
    fn unicode_and_navigation_keys_have_native_event_values() {
        let japanese = map_key(BrowserKey::Character('苺'));
        assert_eq!(japanese.key, "苺");
        assert_eq!(japanese.identifier, "U+82FA");
        assert_eq!(japanese.virtual_key, 0);
        assert_eq!(map_key(BrowserKey::Left).virtual_key, 37);
        let modifiers = BrowserModifiers {
            shift: true,
            control: true,
            alt: false,
            super_key: true,
        };
        assert_eq!(unmap_modifiers(map_modifiers(modifiers)), modifiers);
    }
}
