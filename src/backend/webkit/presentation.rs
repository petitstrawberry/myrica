//! UI-side endpoint for completed native compositor images. No Page pointer is
//! retained, and none of these operations execute JS, paint, or wait for a fence.

use std::ptr::NonNull;
use std::sync::Arc;

use myrica_webkit_sys as ffi;

use super::super::{BackendError, BrowserFrameSource, BrowserViewport, FrameLease, PresentedFrame};
use super::{check, frame::FrameRenderer};

struct Output(NonNull<ffi::CompositorOutput>);

// SAFETY: This opaque handle owns only the native mutex-protected output
// mailbox. The native API explicitly permits acquire/release/destruction on
// any thread and after the Page has gone away. The owner-only view is separate.
unsafe impl Send for Output {}
unsafe impl Sync for Output {}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: Arc's final reference destroys this independently owned handle.
        unsafe { ffi::scarlet_webview_output_destroy(self.0.as_ptr()) };
    }
}

pub(super) struct FrameSource {
    output: Arc<Output>,
    renderer: FrameRenderer,
    logged: bool,
}

impl FrameSource {
    pub(super) fn new(view: NonNull<ffi::ScarletWebView>) -> Result<Self, BackendError> {
        let mut output = std::ptr::null_mut();
        // SAFETY: Called once while constructing the endpoint on the Page owner.
        check(
            unsafe { ffi::scarlet_webview_compositor_output(view.as_ptr(), &mut output) },
            "open WebCore compositor output",
        )?;
        let output = NonNull::new(output)
            .ok_or_else(|| BackendError::new("WebCore returned a null compositor output"))?;
        Ok(Self {
            output: Arc::new(Output(output)),
            renderer: FrameRenderer::new(),
            logged: false,
        })
    }
}

impl BrowserFrameSource for FrameSource {
    fn poll(&mut self) -> Result<Option<PresentedFrame>, BackendError> {
        let mut frame = ffi::GpuFrame::default();
        let (mut allocation_id, mut epoch, mut scale) = (0, 0, 1.0);
        // SAFETY: Reads only completed mailbox metadata and duplicates a native
        // image capability under a short lock; there is no GPU/owner wait.
        let status = unsafe {
            ffi::scarlet_webview_output_acquire(
                self.output.0.as_ptr(),
                &mut frame,
                &mut allocation_id,
                &mut epoch,
                &mut scale,
            )
        };
        if status == ffi::EMPTY {
            return Ok(None);
        }
        check(status, "acquire compositor image")?;
        let capability = super::super::frame::OwnedGpuCapability::new(frame.handle);
        let output = Arc::clone(&self.output);
        let lease = FrameLease::new(
            frame.generation,
            Arc::new(move |generation| {
                // SAFETY: This mailbox outlives the Page and is safe on UI drop.
                unsafe { ffi::scarlet_webview_output_release(output.0.as_ptr(), generation) };
            }),
        );
        if allocation_id == 0 || !scale.is_finite() || scale <= 0.0 {
            return Err(BackendError::new("invalid compositor image metadata"));
        }
        let image = super::super::frame::OwnedGpuImage::new(capability.into_raw(), allocation_id);
        let viewport = BrowserViewport {
            width: frame.width,
            height: frame.height,
            scale: scale as f32,
        };
        let frame = self.renderer.gpu_frame(frame, image, lease)?;
        if !self.logged {
            eprintln!(
                "[myrica:webkit] page renderer=skia-vulkan presentation=shared-bgra8 cpu-framebuffer=none compositor=threaded"
            );
            self.logged = true;
        }
        Ok(Some(PresentedFrame {
            generation: epoch,
            viewport,
            frame,
        }))
    }
}
