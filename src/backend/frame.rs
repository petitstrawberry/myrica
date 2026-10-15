//! Owned, thread-safe scene data sent from the engine to the UI.

use super::FrameLease;
use std::sync::Arc;

use scarlet_ui::{
    Color, SgfxCanvasDraw, SgfxCanvasFrame, SgfxMesh, SgfxTexture, SgfxTextureHandle,
};

/// CPU-prepared scene. Platform GPU resources are created only on the UI thread.
#[derive(Clone)]
pub struct BrowserFrame {
    pub revision: u64,
    pub width: u32,
    pub height: u32,
    pub draws: Vec<BrowserDraw>,
    pub(crate) lease: Option<Arc<FrameLease>>,
}

#[derive(Clone)]
pub struct BrowserDraw {
    pub mesh: Arc<SgfxMesh>,
    pub transform: [f32; 16],
    pub texture: Option<Arc<BrowserTexture>>,
}

/// RGBA pixels can cross threads; ScarletUI's external platform images cannot.
pub struct BrowserTexture {
    pub handle: SgfxTextureHandle,
    pub revision: u64,
    pub width: u32,
    pub height: u32,
    pub source: BrowserTextureSource,
}

pub enum BrowserTextureSource {
    Rgba8(Arc<[u8]>),
    #[cfg(target_os = "scarlet")]
    SharedGpu(Arc<OwnedGpuImage>),
}

#[cfg(target_os = "scarlet")]
pub struct OwnedGpuImage {
    raw: std::sync::atomic::AtomicI32,
}
#[cfg(target_os = "scarlet")]
impl OwnedGpuImage {
    /// Takes ownership of one exported native capability.
    pub(crate) fn new(raw: i32) -> Arc<Self> {
        Arc::new(Self {
            raw: std::sync::atomic::AtomicI32::new(raw),
        })
    }
    fn take(&self) -> Result<i32, super::BackendError> {
        let raw = self.raw.swap(-1, std::sync::atomic::Ordering::AcqRel);
        if raw < 0 {
            Err(super::BackendError::new(
                "GPU page capability was already consumed",
            ))
        } else {
            Ok(raw)
        }
    }
}
#[cfg(target_os = "scarlet")]
impl Drop for OwnedGpuImage {
    fn drop(&mut self) {
        let raw = *self.raw.get_mut();
        if raw >= 0 {
            // SAFETY: this object owns the capability unless take() disarmed it.
            unsafe {
                scarlet_sys::syscall1(scarlet_sys::Syscall::HandleClose, raw as usize);
            }
        }
    }
}

impl BrowserTexture {
    #[cfg(test)]
    pub(crate) fn rgba_pixels(&self) -> &Arc<[u8]> {
        match &self.source {
            BrowserTextureSource::Rgba8(pixels) => pixels,
            #[cfg(target_os = "scarlet")]
            _ => panic!("expected CPU fixture pixels"),
        }
    }
}

impl BrowserFrame {
    /// Adopt a completed GPU page for direct window composition. CPU scenes
    /// retain their mesh/canvas path. The frame's lease stays owned by the UI.
    pub(crate) fn try_external_gpu_texture(
        &self,
    ) -> Result<Option<Arc<SgfxTexture>>, super::BackendError> {
        #[cfg(target_os = "scarlet")]
        if let [draw] = self.draws.as_slice()
            && let Some(texture) = &draw.texture
            && let BrowserTextureSource::SharedGpu(image) = &texture.source
        {
            if texture.width != self.width
                || texture.height != self.height
                || draw.transform
                    != [
                        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0,
                        1.0,
                    ]
            {
                return Err(super::BackendError::new(
                    "GPU page is not a full-viewport image",
                ));
            }
            let raw = image.take()?;
            // SAFETY: take() transfers the exclusively owned capability once;
            // ScarletUI consumes it on both success and failure.
            return unsafe {
                scarlet_ui::shared_bgra8_texture_from_raw(raw, self.width, self.height)
            }
            .map(Some)
            .map_err(|error| super::BackendError::new(format!("adopt GPU page image: {error}")));
        }
        Ok(None)
    }

    pub fn empty(width: u32, height: u32) -> Self {
        Self {
            revision: 0,
            width: width.max(1),
            height: height.max(1),
            draws: Vec::new(),
            lease: None,
        }
    }

    /// Wrap the prepared meshes and shared pixels without copying their contents.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn into_canvas_frame(self) -> Arc<SgfxCanvasFrame> {
        self.try_into_canvas_frame()
            .expect("CPU frame has no platform adoption failure")
    }

    #[allow(clippy::arc_with_non_send_sync)]
    pub fn try_into_canvas_frame(self) -> Result<Arc<SgfxCanvasFrame>, super::BackendError> {
        let mut frame = SgfxCanvasFrame::new(self.revision, Color::WHITE)
            .reference_aspect(self.width as f32 / self.height.max(1) as f32);
        for draw in self.draws {
            let mut canvas_draw = SgfxCanvasDraw::new(draw.mesh, draw.transform);
            if let Some(texture) = draw.texture {
                let image = match &texture.source {
                    BrowserTextureSource::Rgba8(pixels) => SgfxTexture::rgba8_with_handle(
                        texture.handle,
                        texture.revision,
                        texture.width,
                        texture.height,
                        Arc::clone(pixels),
                    ),
                    #[cfg(target_os = "scarlet")]
                    BrowserTextureSource::SharedGpu(image) => {
                        let raw = image.take()?;
                        // SAFETY: take() transfers this exclusively owned capability
                        // once. ScarletUI consumes it on both success and failure.
                        unsafe {
                            scarlet_ui::shared_bgra8_texture_from_raw(
                                raw,
                                texture.width,
                                texture.height,
                            )
                        }
                        .map_err(|error| {
                            super::BackendError::new(format!("adopt GPU page image: {error}"))
                        })?
                    }
                };
                canvas_draw = canvas_draw.texture(image);
            }
            frame = frame.draw(canvas_draw);
        }
        Ok(Arc::new(frame))
    }

    #[cfg(test)]
    pub fn draw_count(&self) -> usize {
        self.draws.len()
    }
}
