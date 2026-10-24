//! Adopt native GPU page images, or convert the optional Cairo framebuffer.

use std::sync::Arc;

use scarlet_ui::{SgfxCanvasVertex, SgfxMesh, SgfxMeshHandle, SgfxTextureHandle};

use super::super::{BackendError, BrowserDraw, BrowserFrame, BrowserTexture, BrowserTextureSource};

pub(super) struct FrameRenderer {
    mesh: SgfxMeshHandle,
    texture: SgfxTextureHandle,
    texture_size: (u32, u32),
    #[cfg(all(feature = "backend-webkit-vulkan", target_os = "scarlet"))]
    gpu_mesh: Option<Arc<SgfxMesh>>,
    cached: Option<((u64, u32, u32, usize), BrowserFrame)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_pixels_reuse_the_texture_and_mesh() {
        let mut renderer = FrameRenderer::new();
        let first = renderer.frame(7, 2, 1, 8, &[0; 8]).unwrap();
        for _ in 0..100 {
            let cached = renderer.cached_frame(7, 2, 1, 8).unwrap();
            assert!(Arc::ptr_eq(&first.draws[0].mesh, &cached.draws[0].mesh));
            let first_texture = first.draws[0].texture.as_ref().unwrap();
            let cached_texture = cached.draws[0].texture.as_ref().unwrap();
            assert!(Arc::ptr_eq(first_texture, cached_texture));
            assert!(Arc::ptr_eq(
                first_texture.rgba_pixels(),
                cached_texture.rgba_pixels()
            ));
        }
        assert!(renderer.cached_frame(8, 2, 1, 8).is_none());
        assert!(renderer.cached_frame(7, 1, 1, 8).is_none());
        assert!(renderer.cached_frame(7, 2, 1, 12).is_none());
    }

    #[test]
    fn a_new_generation_replaces_pixels_and_resize_invalidates_the_cache() {
        let mut renderer = FrameRenderer::new();
        let first = renderer.frame(1, 1, 1, 4, &[0, 0, 0, 255]).unwrap();
        let changed = renderer.frame(2, 1, 1, 4, &[0, 0, 255, 255]).unwrap();
        assert!(!Arc::ptr_eq(
            first.draws[0].texture.as_ref().unwrap().rgba_pixels(),
            changed.draws[0].texture.as_ref().unwrap().rgba_pixels(),
        ));
        assert_eq!(
            &**changed.draws[0].texture.as_ref().unwrap().rgba_pixels(),
            &[255, 0, 0, 255]
        );
        assert!(renderer.cached_frame(1, 1, 1, 4).is_none());
        renderer.frame(2, 2, 1, 8, &[0; 8]).unwrap();
        assert!(renderer.cached_frame(2, 1, 1, 4).is_none());
        assert!(renderer.cached_frame(2, 2, 1, 8).is_some());
    }
}

impl FrameRenderer {
    pub fn new() -> Self {
        Self {
            mesh: SgfxMeshHandle::new(),
            texture: SgfxTextureHandle::new(),
            texture_size: (0, 0),
            cached: None,
            #[cfg(all(feature = "backend-webkit-vulkan", target_os = "scarlet"))]
            gpu_mesh: None,
        }
    }

    #[cfg(all(feature = "backend-webkit-vulkan", target_os = "scarlet"))]
    pub fn gpu_frame(
        &mut self,
        native: myrica_webkit_sys::GpuFrame,
        image: Arc<super::super::frame::OwnedGpuImage>,
        lease: Arc<super::super::FrameLease>,
    ) -> Result<BrowserFrame, BackendError> {
        if native.handle < 0
            || native.width == 0
            || native.height == 0
            || native.generation == 0
            || native.format != myrica_webkit_sys::PIXEL_FORMAT_BGRA_PREMULTIPLIED
        {
            return Err(BackendError::new(
                "WebCore returned an invalid shared GPU image",
            ));
        }
        let mesh = self.gpu_mesh.get_or_insert_with(|| {
            let vertex = |x, y, u, v| {
                SgfxCanvasVertex::new([x, y, 0.0, 1.0], [1.0; 4]).with_tex_coord([u, v])
            };
            SgfxMesh::with_handle(
                self.mesh,
                0,
                vec![
                    vertex(-1.0, 1.0, 0.0, 0.0),
                    vertex(1.0, 1.0, 1.0, 0.0),
                    vertex(-1.0, -1.0, 0.0, 1.0),
                    vertex(-1.0, -1.0, 0.0, 1.0),
                    vertex(1.0, 1.0, 1.0, 0.0),
                    vertex(1.0, -1.0, 1.0, 1.0),
                ],
            )
        });
        Ok(BrowserFrame {
            revision: native.generation,
            width: native.width,
            height: native.height,
            damage: None,
            lease: Some(lease),
            draws: vec![BrowserDraw {
                mesh: Arc::clone(mesh),
                transform: [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
                ],
                texture: Some(Arc::new(BrowserTexture {
                    handle: self.texture,
                    revision: native.generation,
                    width: native.width,
                    height: native.height,
                    source: BrowserTextureSource::SharedGpu(image),
                })),
            }],
        })
    }

    pub fn cached_frame(
        &self,
        revision: u64,
        width: u32,
        height: u32,
        stride: usize,
    ) -> Option<BrowserFrame> {
        self.cached.as_ref().and_then(|(key, frame)| {
            (*key == (revision, width, height, stride)).then(|| frame.clone())
        })
    }

    pub fn frame(
        &mut self,
        revision: u64,
        width: u32,
        height: u32,
        stride: usize,
        bgra: &[u8],
    ) -> Result<BrowserFrame, BackendError> {
        let rgba =
            super::pixels::convert_bgra(width, height, stride, bgra).map_err(BackendError::new)?;
        let capacity = (
            width
                .checked_next_power_of_two()
                .unwrap_or(u32::MAX)
                .max(self.texture_size.0),
            height
                .checked_next_power_of_two()
                .unwrap_or(u32::MAX)
                .max(self.texture_size.1),
        );
        let capacity_bytes = (capacity.0 as usize)
            .checked_mul(capacity.1 as usize)
            .and_then(|size| size.checked_mul(4))
            .filter(|size| *size <= super::pixels::MAX_FRAME_BYTES)
            .ok_or_else(|| BackendError::new("WebCore viewport exceeds SGFX texture capacity"))?;
        if self.texture_size != (0, 0) && self.texture_size != capacity {
            // SGFX retains a texture's allocation by handle; its dimensions
            // cannot change with a revision update. Grow geometrically rather
            // than consuming an allocation for every interactive resize.
            self.texture = SgfxTextureHandle::new();
        }
        self.texture_size = capacity;
        let mut padded = vec![0; capacity_bytes];
        let row_bytes = width as usize * 4;
        for (source, destination) in rgba
            .chunks_exact(row_bytes)
            .zip(padded.chunks_exact_mut(capacity.0 as usize * 4))
        {
            destination[..row_bytes].copy_from_slice(source);
        }
        let u = width as f32 / capacity.0 as f32;
        let v = height as f32 / capacity.1 as f32;
        let vertex =
            |x, y, u, v| SgfxCanvasVertex::new([x, y, 0.0, 1.0], [1.0; 4]).with_tex_coord([u, v]);
        let vertices = vec![
            vertex(-1.0, 1.0, 0.0, 0.0),
            vertex(1.0, 1.0, u, 0.0),
            vertex(-1.0, -1.0, 0.0, v),
            vertex(-1.0, -1.0, 0.0, v),
            vertex(1.0, 1.0, u, 0.0),
            vertex(1.0, -1.0, u, v),
        ];
        let frame = BrowserFrame {
            revision,
            width,
            height,
            damage: None,
            lease: None,
            draws: vec![BrowserDraw {
                mesh: SgfxMesh::with_handle(self.mesh, revision, vertices),
                transform: [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
                ],
                texture: Some(Arc::new(BrowserTexture {
                    handle: self.texture,
                    revision,
                    width: capacity.0,
                    height: capacity.1,
                    source: BrowserTextureSource::Rgba8(Arc::from(padded)),
                })),
            }],
        };
        self.cached = Some(((revision, width, height, stride), frame.clone()));
        Ok(frame)
    }
}
