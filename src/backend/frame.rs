//! Owned, thread-safe scene data sent from the engine to the UI.

use std::sync::Arc;

use scarlet_ui::{
    Color, SgfxCanvasDraw, SgfxCanvasFrame, SgfxMesh, SgfxTexture, SgfxTextureHandle,
};

/// CPU-prepared scene. Platform GPU resources are created only on the UI thread.
pub struct BrowserFrame {
    pub revision: u64,
    pub width: u32,
    pub height: u32,
    pub draws: Vec<BrowserDraw>,
}

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
    pub pixels: Arc<[u8]>,
}

impl BrowserFrame {
    pub fn empty(width: u32, height: u32) -> Self {
        Self {
            revision: 0,
            width: width.max(1),
            height: height.max(1),
            draws: Vec::new(),
        }
    }

    /// Wrap the prepared meshes and shared pixels without copying their contents.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn into_canvas_frame(self) -> Arc<SgfxCanvasFrame> {
        let mut frame = SgfxCanvasFrame::new(self.revision, Color::WHITE)
            .reference_aspect(self.width as f32 / self.height.max(1) as f32);
        for draw in self.draws {
            let mut canvas_draw = SgfxCanvasDraw::new(draw.mesh, draw.transform);
            if let Some(texture) = draw.texture {
                canvas_draw = canvas_draw.texture(SgfxTexture::rgba8_with_handle(
                    texture.handle,
                    texture.revision,
                    texture.width,
                    texture.height,
                    Arc::clone(&texture.pixels),
                ));
            }
            frame = frame.draw(canvas_draw);
        }
        Arc::new(frame)
    }

    #[cfg(test)]
    pub fn draw_count(&self) -> usize {
        self.draws.len()
    }
}
