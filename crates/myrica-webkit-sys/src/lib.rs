//! Raw bindings to `ScarletWebView.h`; all calls belong to the creating thread.

#[cfg(target_os = "scarlet")]
mod native_memory;

use std::ffi::c_int;

pub const ABI_VERSION: u32 = 1;

#[cfg(feature = "vulkan-probe")]
unsafe extern "C" {
    /// Executes real Ganesh Vulkan draws and validates GPU-read pixels.
    /// Must be called on a single native Scarlet thread before starting a GUI.
    pub fn scarlet_webview_vulkan_probe() -> c_int;
    pub fn scarlet_webview_tls_value() -> u64;
    pub fn scarlet_webview_tls_increment() -> u64;
}
pub const OK: i32 = 0;
pub const EMPTY: i32 = 1;
pub const STALE_REQUEST: i32 = -6;
pub const NOT_INITIALIZED: i32 = -3;
pub const SHIFT: u32 = 1;
pub const CONTROL: u32 = 2;
pub const ALT: u32 = 4;
pub const META: u32 = 8;
pub const PIXEL_FORMAT_BGRA_PREMULTIPLIED: u32 = 1;

#[repr(C)]
pub struct ScarletWebView {
    _opaque: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Bytes {
    pub data: *const u8,
    pub length: usize,
}

impl Bytes {
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self {
            data: bytes.as_ptr(),
            length: bytes.len(),
        }
    }
    pub fn from_str(text: &str) -> Self {
        Self::from_slice(text.as_bytes())
    }
    pub const fn empty() -> Self {
        Self {
            data: std::ptr::null(),
            length: 0,
        }
    }
}

impl Default for Bytes {
    fn default() -> Self {
        Self::empty()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PaintStatistics {
    pub paints: u64,
    pub painted_pixels: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Header {
    pub name: Bytes,
    pub value: Bytes,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Configuration {
    pub abi_version: u32,
    pub width: u32,
    pub height: u32,
    pub script_enabled: u32,
    pub device_scale_factor: f64,
}

/// Owned native GPU capability and owner-thread frame lease.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GpuFrame {
    pub handle: i32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub generation: u64,
}
impl Default for GpuFrame {
    fn default() -> Self {
        Self {
            handle: -1,
            width: 0,
            height: 0,
            format: 0,
            generation: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Pixels {
    pub data: *const u8,
    pub length: usize,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    pub generation: u64,
}

impl Default for Pixels {
    fn default() -> Self {
        Self {
            data: std::ptr::null(),
            length: 0,
            width: 0,
            height: 0,
            stride: 0,
            format: 0,
            generation: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct State {
    pub url: Bytes,
    pub title: Bytes,
    pub last_error: Bytes,
    pub frame_generation: u64,
    pub script_execution_entries: u64,
    pub loading: u32,
    pub pending_requests: u32,
    pub scroll_x: i32,
    pub scroll_y: i32,
    pub can_go_back: u32,
    pub can_go_forward: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MouseEvent {
    pub kind: u32,
    pub button: u32,
    pub modifiers: u32,
    pub click_count: u32,
    pub x: f64,
    pub y: f64,
    pub global_x: f64,
    pub global_y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WheelEvent {
    pub modifiers: u32,
    pub x: f64,
    pub y: f64,
    pub delta_x: f64,
    pub delta_y: f64,
    pub global_x: f64,
    pub global_y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KeyEvent {
    pub kind: u32,
    pub modifiers: u32,
    pub windows_virtual_key: u32,
    pub auto_repeat: u32,
    pub text: Bytes,
    pub unmodified_text: Bytes,
    pub key: Bytes,
    pub code: Bytes,
    pub key_identifier: Bytes,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Request {
    pub request_id: u64,
    pub url: Bytes,
    pub method: Bytes,
    pub headers: *const Header,
    pub header_count: usize,
    pub body: Bytes,
    pub credentials_mode: u32,
    pub is_cancel: u32,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            request_id: 0,
            url: Bytes::empty(),
            method: Bytes::empty(),
            headers: std::ptr::null(),
            header_count: 0,
            body: Bytes::empty(),
            credentials_mode: 0,
            is_cancel: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Response {
    pub request_id: u64,
    pub status_code: u32,
    pub final_url: Bytes,
    pub status_text: Bytes,
    pub headers: *const Header,
    pub header_count: usize,
    pub body: Bytes,
    pub transport_error: i32,
    pub error_message: Bytes,
}

impl Default for Response {
    fn default() -> Self {
        Self {
            request_id: 0,
            status_code: 0,
            final_url: Bytes::empty(),
            status_text: Bytes::empty(),
            headers: std::ptr::null(),
            header_count: 0,
            body: Bytes::empty(),
            transport_error: 0,
            error_message: Bytes::empty(),
        }
    }
}

unsafe extern "C" {
    pub fn scarlet_webview_initialize(font_configuration_path: Bytes) -> c_int;
    pub fn scarlet_webview_shutdown() -> c_int;
    /// The sole thread-safe exception to the owner-thread-only view API.
    pub fn scarlet_webview_request_termination() -> c_int;
    pub fn scarlet_webview_create(
        configuration: *const Configuration,
        out_view: *mut *mut ScarletWebView,
    ) -> c_int;
    pub fn scarlet_webview_destroy(view: *mut ScarletWebView) -> c_int;
    pub fn scarlet_webview_navigate(view: *mut ScarletWebView, absolute_url: Bytes) -> c_int;
    pub fn scarlet_webview_reload(view: *mut ScarletWebView) -> c_int;
    pub fn scarlet_webview_back(view: *mut ScarletWebView) -> c_int;
    pub fn scarlet_webview_forward(view: *mut ScarletWebView) -> c_int;
    pub fn scarlet_webview_resize(
        view: *mut ScarletWebView,
        width: u32,
        height: u32,
        device_scale_factor: f64,
    ) -> c_int;
    pub fn scarlet_webview_focus(view: *mut ScarletWebView, focused: u32) -> c_int;
    pub fn scarlet_webview_tick(view: *mut ScarletWebView) -> c_int;
    pub fn scarlet_webview_needs_paint(view: *mut ScarletWebView, out: *mut u32) -> c_int;
    pub fn scarlet_webview_pixels(view: *mut ScarletWebView, out_pixels: *mut Pixels) -> c_int;
    pub fn scarlet_webview_gpu_frame_capacity(view: *mut ScarletWebView, out: *mut u32) -> c_int;
    pub fn scarlet_webview_gpu_frame(view: *mut ScarletWebView, out: *mut GpuFrame) -> c_int;
    pub fn scarlet_webview_release_gpu_frame(view: *mut ScarletWebView, generation: u64) -> c_int;
    /// Renderer diagnostic only; browser presentation never reads page pixels.
    pub fn scarlet_webview_gpu_frame_readback(
        view: *mut ScarletWebView,
        generation: u64,
        out: *mut Pixels,
    ) -> c_int;
    pub fn scarlet_webview_state(view: *mut ScarletWebView, out_state: *mut State) -> c_int;
    pub fn scarlet_webview_mouse(
        view: *mut ScarletWebView,
        event: *const MouseEvent,
        handled: *mut u32,
    ) -> c_int;
    pub fn scarlet_webview_wheel(
        view: *mut ScarletWebView,
        event: *const WheelEvent,
        handled: *mut u32,
    ) -> c_int;
    pub fn scarlet_webview_key(
        view: *mut ScarletWebView,
        event: *const KeyEvent,
        handled: *mut u32,
    ) -> c_int;
    pub fn scarlet_webview_poll_request(
        view: *mut ScarletWebView,
        out_request: *mut Request,
    ) -> c_int;
    pub fn scarlet_webview_complete_request(
        view: *mut ScarletWebView,
        response: *const Response,
    ) -> c_int;
    pub fn scarlet_webview_paint_statistics(
        view: *mut ScarletWebView,
        out: *mut PaintStatistics,
    ) -> c_int;
    pub fn scarlet_webview_text_content(
        view: *mut ScarletWebView,
        element_id: Bytes,
        out_text: *mut Bytes,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abi_v1_layout_matches_native_64_bit_header() {
        assert_eq!(std::mem::size_of::<Bytes>(), 16);
        assert_eq!(std::mem::size_of::<Configuration>(), 24);
        assert_eq!(std::mem::size_of::<Pixels>(), 40);
        assert_eq!(std::mem::size_of::<State>(), 88);
        assert_eq!(std::mem::size_of::<MouseEvent>(), 48);
        assert_eq!(std::mem::size_of::<WheelEvent>(), 56);
        assert_eq!(std::mem::size_of::<KeyEvent>(), 96);
        assert_eq!(std::mem::size_of::<Request>(), 80);
        assert_eq!(std::mem::size_of::<Response>(), 104);
    }
}
