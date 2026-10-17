//! UI-side endpoints for completed compositor images and asynchronous wheel
//! routing. These operations never execute JS, paint, or wait for a GPU fence.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

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

pub(super) struct InputSink {
    output: Arc<Output>,
    modifiers: Mutex<u32>,
}

impl super::super::BrowserInputSink for InputSink {
    fn dispatch(
        &self,
        input: &super::super::BrowserInput,
        generation: u64,
        viewport: BrowserViewport,
    ) -> bool {
        use super::super::BrowserInput;
        let mut modifiers = self.modifiers.lock().unwrap();
        match input {
            BrowserInput::Key {
                modifiers: current, ..
            } => {
                *modifiers = super::map_modifiers(*current);
                false
            }
            BrowserInput::FocusLost => {
                *modifiers = 0;
                false
            }
            BrowserInput::Wheel {
                delta_x,
                delta_y,
                x,
                y,
            } => {
                let event = ffi::WheelEvent {
                    modifiers: *modifiers,
                    x: f64::from(*x),
                    y: f64::from(*y),
                    delta_x: *delta_x,
                    delta_y: *delta_y,
                    global_x: f64::from(*x),
                    global_y: f64::from(*y),
                };
                // SAFETY: The native endpoint accepts foreign-thread events,
                // retains its scrolling state and rejects obsolete viewports.
                unsafe {
                    ffi::scarlet_webview_output_wheel(
                        self.output.0.as_ptr(),
                        &event,
                        generation,
                        viewport.width,
                        viewport.height,
                        f64::from(viewport.scale),
                    ) == ffi::OK
                }
            }
            _ => false,
        }
    }
}

pub(super) struct FrameSource {
    output: Arc<Output>,
    renderer: FrameRenderer,
    logged: bool,
    maximum_dimension: u32,
}

impl FrameSource {
    pub(super) fn input_sink(&self) -> Arc<dyn super::super::BrowserInputSink> {
        Arc::new(InputSink {
            output: Arc::clone(&self.output),
            modifiers: Mutex::new(0),
        })
    }
    pub(super) fn new(view: NonNull<ffi::ScarletWebView>) -> Result<Self, BackendError> {
        let mut maximum_dimension = 0;
        // SAFETY: The immutable embedding/GPU limit is read on the Page owner.
        check(
            unsafe {
                ffi::scarlet_webview_maximum_surface_dimension(
                    view.as_ptr(),
                    &mut maximum_dimension,
                )
            },
            "query WebCore surface limit",
        )?;
        if maximum_dimension == 0 {
            return Err(BackendError::new("WebCore returned an empty surface limit"));
        }
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
            maximum_dimension,
        })
    }
}

impl BrowserFrameSource for FrameSource {
    fn maximum_surface_dimension(&self) -> Option<u32> {
        Some(self.maximum_dimension)
    }
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
        let mut damage = [0; 4];
        // SAFETY: The leased generation's immutable damage belongs to this
        // independently owned endpoint, including after Page destruction.
        check(
            unsafe {
                ffi::scarlet_webview_output_damage(
                    self.output.0.as_ptr(),
                    frame.generation,
                    damage.as_mut_ptr(),
                )
            },
            "query compositor damage",
        )?;
        let mut frame = self.renderer.gpu_frame(frame, image, lease)?;
        frame.damage = Some(damage);
        if !self.logged {
            eprintln!(
                "[myrica:webkit] page renderer=skia-vulkan presentation=shared-bgra8 cpu-framebuffer=none compositor=threaded scrolling=async"
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
