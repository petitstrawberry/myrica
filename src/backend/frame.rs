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
    /// Changed physical pixels since the last acquired producer frame. None
    /// means the complete image; consumers union updates skipped before paint.
    pub damage: Option<[u32; 4]>,
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
    allocation_id: u64,
}
#[cfg(target_os = "scarlet")]
impl OwnedGpuImage {
    /// Takes ownership of one exported native capability.
    pub(crate) fn new(raw: i32, allocation_id: u64) -> Arc<Self> {
        Arc::new(Self {
            raw: std::sync::atomic::AtomicI32::new(raw),
            allocation_id,
        })
    }
    fn take(&self) -> Result<OwnedGpuCapability, super::BackendError> {
        let raw = self.raw.swap(-1, std::sync::atomic::Ordering::AcqRel);
        if raw < 0 {
            Err(super::BackendError::new(
                "GPU page capability was already consumed",
            ))
        } else {
            Ok(OwnedGpuCapability::new(raw))
        }
    }
}
/// One exported capability, closed unless explicitly transferred to ScarletUI.
#[cfg(target_os = "scarlet")]
pub(crate) struct OwnedGpuCapability(i32);
#[cfg(target_os = "scarlet")]
impl OwnedGpuCapability {
    pub(crate) fn new(raw: i32) -> Self {
        Self(raw)
    }
    pub(crate) fn into_raw(mut self) -> i32 {
        std::mem::replace(&mut self.0, -1)
    }
}
#[cfg(target_os = "scarlet")]
impl Drop for OwnedGpuCapability {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: this guard exclusively owns the exported capability.
            unsafe {
                scarlet_sys::syscall1(scarlet_sys::Syscall::HandleClose, self.0 as usize);
            }
        }
    }
}
#[cfg(target_os = "scarlet")]
impl Drop for OwnedGpuImage {
    fn drop(&mut self) {
        drop(OwnedGpuCapability::new(*self.raw.get_mut()));
    }
}

/// UI-thread image ownership only. Frame leases must never enter this cache.
pub(crate) struct GpuImageCache<T> {
    entries: Vec<(u64, u32, u32, Arc<T>)>,
}
impl<T> Default for GpuImageCache<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}
impl<T> GpuImageCache<T> {
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
    // Creation consumes an export only on a miss. A hit still owns the same
    // allocation, but each newly presented frame retains its independent lease.
    #[cfg(any(target_os = "scarlet", test))]
    fn get_or_try_insert(
        &mut self,
        allocation_id: u64,
        width: u32,
        height: u32,
        create: impl FnOnce() -> Result<Arc<T>, super::BackendError>,
    ) -> Result<Arc<T>, super::BackendError> {
        if allocation_id == 0 || width == 0 || height == 0 {
            return Err(super::BackendError::new(
                "invalid GPU image allocation identity or extent",
            ));
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.0 == allocation_id)
        {
            if (self.entries[index].1, self.entries[index].2) != (width, height) {
                return Err(super::BackendError::new(
                    "GPU image allocation extent changed",
                ));
            }
            let entry = self.entries.remove(index);
            let image = Arc::clone(&entry.3);
            self.entries.push(entry);
            return Ok(image);
        }
        let image = create()?;
        if self.entries.len() == 2 {
            self.entries.remove(0);
        }
        self.entries
            .push((allocation_id, width, height, Arc::clone(&image)));
        Ok(image)
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
        cache: &mut GpuImageCache<SgfxTexture>,
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
            let capability = image.take()?;
            return cache
                .get_or_try_insert(image.allocation_id, self.width, self.height, || {
                    // SAFETY: the guard transfers the exclusively owned export;
                    // ScarletUI consumes it on both success and failure.
                    unsafe {
                        scarlet_ui::shared_bgra8_texture_from_raw(
                            capability.into_raw(),
                            self.width,
                            self.height,
                        )
                    }
                    .map_err(|error| {
                        super::BackendError::new(format!("adopt GPU page image: {error}"))
                    })
                })
                .map(Some);
        }
        let _ = cache;
        Ok(None)
    }

    pub fn empty(width: u32, height: u32) -> Self {
        Self {
            revision: 0,
            width: width.max(1),
            height: height.max(1),
            damage: None,
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
                        let capability = image.take()?;
                        // SAFETY: take() transfers this exclusively owned capability
                        // once. ScarletUI consumes it on both success and failure.
                        unsafe {
                            scarlet_ui::shared_bgra8_texture_from_raw(
                                capability.into_raw(),
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

#[cfg(test)]
mod gpu_image_cache_tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct Resource(Rc<Cell<usize>>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn allocation_identity_reuses_ownership_and_drops_unused_export() {
        let drops = Rc::new(Cell::new(0));
        let mut cache = GpuImageCache::default();
        let first = cache
            .get_or_try_insert(17, 640, 480, || Ok(Arc::new(Resource(Rc::clone(&drops)))))
            .unwrap();
        let unused_export = Resource(Rc::clone(&drops));
        let same = cache
            .get_or_try_insert(17, 640, 480, move || {
                drop(unused_export);
                panic!("same allocation must not be imported twice")
            })
            .unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        assert_eq!(drops.get(), 1, "unused export is released even on a hit");
        drop(same);
        cache.clear();
        assert_eq!(Arc::strong_count(&first), 1);
        assert_eq!(drops.get(), 1, "current UI image outlives cache clearing");
        drop(first);
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn two_entry_lru_retires_only_cache_ownership() {
        let drops = Rc::new(Cell::new(0));
        let mut cache = GpuImageCache::default();
        let first = cache
            .get_or_try_insert(1, 2, 2, || Ok(Arc::new(Resource(Rc::clone(&drops)))))
            .unwrap();
        let second = cache
            .get_or_try_insert(2, 2, 2, || Ok(Arc::new(Resource(Rc::clone(&drops)))))
            .unwrap();
        drop(
            cache
                .get_or_try_insert(1, 2, 2, || panic!("cache hit"))
                .unwrap(),
        );
        drop(
            cache
                .get_or_try_insert(3, 4, 4, || Ok(Arc::new(Resource(Rc::clone(&drops)))))
                .unwrap(),
        );
        assert_eq!(
            cache
                .entries
                .iter()
                .map(|entry| entry.0)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(Arc::strong_count(&first), 2);
        assert_eq!(
            Arc::strong_count(&second),
            1,
            "evicted allocation can still be in flight"
        );
        assert_eq!(drops.get(), 0);
        drop(second);
        assert_eq!(drops.get(), 1);
        cache.clear();
        assert_eq!(drops.get(), 2);
        drop(first);
        assert_eq!(drops.get(), 3);
    }

    #[test]
    fn invalid_extent_and_failed_adoption_preserve_cached_allocations() {
        let drops = Rc::new(Cell::new(0));
        let mut cache = GpuImageCache::default();
        drop(
            cache
                .get_or_try_insert(1, 2, 2, || Ok(Arc::new(Resource(Rc::clone(&drops)))))
                .unwrap(),
        );
        drop(
            cache
                .get_or_try_insert(2, 2, 2, || Ok(Arc::new(Resource(Rc::clone(&drops)))))
                .unwrap(),
        );
        let unused_export = Resource(Rc::clone(&drops));
        assert!(
            cache
                .get_or_try_insert(1, 3, 2, move || {
                    drop(unused_export);
                    panic!("extent mismatch must not import")
                })
                .is_err()
        );
        assert_eq!(drops.get(), 1);
        assert!(
            cache
                .get_or_try_insert(3, 4, 4, || Err(super::super::BackendError::new(
                    "adoption failed"
                )))
                .is_err()
        );
        assert!(
            cache
                .get_or_try_insert(0, 4, 4, || panic!("zero identity"))
                .is_err()
        );
        assert_eq!(
            cache
                .entries
                .iter()
                .map(|entry| entry.0)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        cache.clear();
        assert_eq!(drops.get(), 3);
    }
}
