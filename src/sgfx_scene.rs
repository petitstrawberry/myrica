//! AnyRender scene sink backed by ScarletUI's retained SGFX canvas.

use std::collections::HashMap;
use std::sync::Arc;

use anyrender::{Filter, Glyph, NormalizedCoord, Paint, PaintRef, PaintScene, RenderContext};
use lyon_path::Path;
use lyon_path::math::point;
use lyon_tessellation::{
    BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, VertexBuffers,
};
use peniko::color::Srgb;
use peniko::kurbo::{
    Affine, BezPath, PathEl, Point, Rect, RoundedRect, Shape, Stroke, StrokeOpts, Vec2, stroke,
};
use peniko::{
    BlendMode, Color as PaintColor, Extend, Fill, FontData, Gradient, GradientKind, ImageAlphaType,
    ImageData, ImageFormat, StyleRef,
};
use scarlet_ui::{
    Color as UiColor, SgfxCanvasDraw, SgfxCanvasFrame, SgfxCanvasVertex, SgfxMesh, SgfxMeshHandle,
    SgfxTexture,
};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider, instance::Size};

const PATH_TOLERANCE: f64 = 0.15;
const MAX_CANVAS_DRAWS: usize = 240;
const MAX_GLYPH_CACHE_ENTRIES: usize = 4096;

/// Persistent AnyRender sink that turns Blitz paint commands into SGFX meshes.
pub struct SgfxSceneRenderer {
    width: u32,
    height: u32,
    revision: u64,
    batches: Vec<Batch>,
    layers: Vec<LayerState>,
    layer: LayerState,
    mesh_handles: Vec<SgfxMeshHandle>,
    textures: HashMap<u64, Arc<SgfxTexture>>,
    glyphs: HashMap<GlyphCacheKey, Arc<[[f32; 2]]>>,
}

impl SgfxSceneRenderer {
    pub fn new() -> Self {
        Self {
            width: 1,
            height: 1,
            revision: 0,
            batches: Vec::new(),
            layers: Vec::new(),
            layer: LayerState::new(1, 1),
            mesh_handles: Vec::new(),
            textures: HashMap::new(),
            glyphs: HashMap::new(),
        }
    }

    /// Start recording a physical-pixel frame.
    pub fn begin_frame(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.revision = self.revision.wrapping_add(1);
        self.batches.clear();
        self.layers.clear();
        self.layer = LayerState::new(self.width, self.height);
    }

    /// Return an empty, valid frame while no document is available.
    pub fn empty_frame(&mut self, width: u32, height: u32) -> Arc<SgfxCanvasFrame> {
        self.begin_frame(width, height);
        self.finish_frame()
    }

    #[cfg(test)]
    pub(crate) fn texture_count(&self) -> usize {
        self.textures.len()
    }

    /// Finish recording and build the retained ScarletUI frame.
    pub fn finish_frame(&mut self) -> Arc<SgfxCanvasFrame> {
        let aspect = self.width as f32 / self.height.max(1) as f32;
        let transform = pixel_to_clip(self.width, self.height);
        let mut frame = SgfxCanvasFrame::new(self.revision, UiColor::rgb(255_u8, 255_u8, 255_u8))
            .reference_aspect(aspect);

        for (index, batch) in self
            .batches
            .drain(..)
            .filter(|batch| !batch.vertices.is_empty())
            .take(MAX_CANVAS_DRAWS)
            .enumerate()
        {
            if index == self.mesh_handles.len() {
                self.mesh_handles.push(SgfxMeshHandle::new());
            }
            let mesh =
                SgfxMesh::with_handle(self.mesh_handles[index], self.revision, batch.vertices);
            let mut draw = SgfxCanvasDraw::new(mesh, transform);
            if let Some(texture) = batch.texture {
                draw = draw.texture(texture);
            }
            frame = frame.draw(draw);
        }

        Arc::new(frame)
    }

    fn push_clip(&mut self, transform: Affine, clip: &impl Shape, alpha: f32) {
        self.layers.push(self.layer);
        let bounds = transform.transform_rect_bbox(clip.bounding_box());
        self.layer.clip = self.layer.clip.intersect(ClipRect::from_kurbo(bounds));
        self.layer.alpha *= alpha.clamp(0.0, 1.0);
    }

    fn fill_path(
        &mut self,
        path: &BezPath,
        fill: Fill,
        transform: Affine,
        paint: PaintRef<'_>,
        brush_transform: Option<Affine>,
        extra_alpha: f32,
    ) {
        let Some(resolved) = self.resolve_paint(
            paint,
            transform * brush_transform.unwrap_or(Affine::IDENTITY),
            self.layer.alpha * extra_alpha,
        ) else {
            return;
        };
        let geometry = tessellate(path, fill, transform);
        self.append_geometry(&geometry, &resolved);
    }

    fn append_geometry(&mut self, geometry: &TriangleGeometry, paint: &ResolvedPaint) {
        for triangle in geometry.indices.chunks_exact(3) {
            let a = geometry.vertices[triangle[0] as usize];
            let b = geometry.vertices[triangle[1] as usize];
            let c = geometry.vertices[triangle[2] as usize];
            self.append_triangle(a, b, c, paint);
        }
    }

    fn append_triangle(&mut self, a: [f32; 2], b: [f32; 2], c: [f32; 2], paint: &ResolvedPaint) {
        let mut polygon = vec![paint.vertex(a), paint.vertex(b), paint.vertex(c)];
        polygon = clip_polygon(polygon, self.layer.clip);
        if polygon.len() < 3 {
            return;
        }

        let batch = self.batch_for(paint.texture_key(), paint.texture());
        for index in 1..polygon.len() - 1 {
            let triangle = [polygon[0], polygon[index], polygon[index + 1]];
            if triangle.iter().all(Vertex::is_finite) {
                batch
                    .vertices
                    .extend(triangle.into_iter().map(Vertex::to_sgfx));
            }
        }
    }

    fn batch_for(
        &mut self,
        texture_key: Option<u64>,
        texture: Option<Arc<SgfxTexture>>,
    ) -> &mut Batch {
        if self
            .batches
            .last()
            .is_none_or(|batch| batch.texture_key != texture_key)
        {
            self.batches.push(Batch {
                texture_key,
                texture,
                vertices: Vec::new(),
            });
        }
        self.batches.last_mut().expect("a batch was just created")
    }

    fn resolve_paint(
        &mut self,
        paint: PaintRef<'_>,
        paint_to_world: Affine,
        alpha: f32,
    ) -> Option<ResolvedPaint> {
        let inverse = finite_inverse(paint_to_world);
        match paint {
            Paint::Solid(color) => Some(ResolvedPaint::Solid(multiply_alpha(color, alpha))),
            Paint::Gradient(gradient) => Some(ResolvedPaint::Gradient {
                gradient: gradient.clone(),
                inverse,
                alpha,
            }),
            Paint::Image(image) => {
                let key = image.image.data.id();
                let texture = self.texture(image.image)?;
                Some(ResolvedPaint::Image {
                    key,
                    texture,
                    inverse,
                    width: image.image.width.max(1) as f32,
                    height: image.image.height.max(1) as f32,
                    alpha: alpha * image.sampler.alpha,
                })
            }
            Paint::Resource(_) | Paint::Custom(_) => None,
        }
    }

    fn texture(&mut self, image: &ImageData) -> Option<Arc<SgfxTexture>> {
        let key = image.data.id();
        if let Some(texture) = self.textures.get(&key) {
            return Some(Arc::clone(texture));
        }
        let expected = image.width as usize * image.height as usize * 4;
        if image.width == 0 || image.height == 0 || image.data.len() != expected {
            return None;
        }

        let mut rgba = image.data.data().to_vec();
        match image.format {
            ImageFormat::Rgba8 => {}
            ImageFormat::Bgra8 => {
                for pixel in rgba.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
            }
            _ => return None,
        }
        if image.alpha_type == ImageAlphaType::AlphaPremultiplied {
            for pixel in rgba.chunks_exact_mut(4) {
                let alpha = u32::from(pixel[3]);
                if alpha == 0 {
                    pixel[0] = 0;
                    pixel[1] = 0;
                    pixel[2] = 0;
                } else if alpha < 255 {
                    for channel in &mut pixel[..3] {
                        *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                    }
                }
            }
        }

        let texture = SgfxTexture::rgba8(image.width, image.height, rgba);
        self.textures.insert(key, Arc::clone(&texture));
        Some(texture)
    }

    fn glyph_geometry(
        &mut self,
        font: &FontRef<'_>,
        font_data: &FontData,
        font_size: f32,
        normalized_coords: &[NormalizedCoord],
        glyph_id: u32,
        fill: Fill,
    ) -> Option<Arc<[[f32; 2]]>> {
        let key = GlyphCacheKey {
            font_id: font_data.data.id(),
            font_index: font_data.index,
            glyph_id,
            font_size: font_size.to_bits(),
            fill: fill as u8,
            coords: normalized_coords.into(),
        };
        if let Some(geometry) = self.glyphs.get(&key) {
            return Some(Arc::clone(geometry));
        }

        let outline = font.outline_glyphs().get(GlyphId::new(glyph_id))?;
        let coords: Vec<_> = normalized_coords
            .iter()
            .map(|coord| skrifa::instance::NormalizedCoord::from_f32(f32::from(*coord) / 16384.0))
            .collect();
        let mut pen = BezPen::default();
        outline
            .draw(
                DrawSettings::unhinted(Size::new(font_size), coords.as_slice()),
                &mut pen,
            )
            .ok()?;
        let geometry = tessellate(&pen.path, fill, Affine::IDENTITY);
        let triangles: Arc<[[f32; 2]]> = geometry
            .indices
            .iter()
            .map(|index| geometry.vertices[*index as usize])
            .collect::<Vec<_>>()
            .into();
        if self.glyphs.len() >= MAX_GLYPH_CACHE_ENTRIES {
            self.glyphs.clear();
        }
        self.glyphs.insert(key, Arc::clone(&triangles));
        Some(triangles)
    }
}

impl RenderContext for SgfxSceneRenderer {}

impl PaintScene for SgfxSceneRenderer {
    fn reset(&mut self) {
        self.begin_frame(self.width, self.height);
    }

    fn push_layer(
        &mut self,
        _blend: impl Into<BlendMode>,
        alpha: f32,
        transform: Affine,
        clip: &impl Shape,
        _filter: Option<Arc<Filter>>,
        _backdrop_filter: Option<Arc<Filter>>,
    ) {
        self.push_clip(transform, clip, alpha);
    }

    fn push_clip_layer(&mut self, transform: Affine, clip: &impl Shape) {
        self.push_clip(transform, clip, 1.0);
    }

    fn pop_layer(&mut self) {
        if let Some(layer) = self.layers.pop() {
            self.layer = layer;
        }
    }

    fn stroke<'a>(
        &mut self,
        style: &Stroke,
        transform: Affine,
        paint: impl Into<PaintRef<'a>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        let path = shape.into_path(PATH_TOLERANCE);
        let outline = stroke(
            path.elements().iter().copied(),
            style,
            &StrokeOpts::default(),
            PATH_TOLERANCE,
        );
        self.fill_path(
            &outline,
            Fill::NonZero,
            transform,
            paint.into(),
            brush_transform,
            1.0,
        );
    }

    fn fill<'a>(
        &mut self,
        fill: Fill,
        transform: Affine,
        paint: impl Into<PaintRef<'a>>,
        brush_transform: Option<Affine>,
        shape: &impl Shape,
    ) {
        self.fill_path(
            &shape.into_path(PATH_TOLERANCE),
            fill,
            transform,
            paint.into(),
            brush_transform,
            1.0,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_glyphs<'a, 's: 'a>(
        &'s mut self,
        font_data: &'a FontData,
        font_size: f32,
        _hint: bool,
        normalized_coords: &'a [NormalizedCoord],
        _embolden: Vec2,
        style: impl Into<StyleRef<'a>>,
        paint: impl Into<PaintRef<'a>>,
        brush_alpha: f32,
        transform: Affine,
        glyph_transform: Option<Affine>,
        glyphs: impl Iterator<Item = Glyph> + Clone,
    ) {
        let Ok(font) = FontRef::from_index(font_data.data.data(), font_data.index) else {
            return;
        };
        let Some(resolved) =
            self.resolve_paint(paint.into(), transform, self.layer.alpha * brush_alpha)
        else {
            return;
        };
        let style = style.into();

        for glyph in glyphs {
            let glyph_transform =
                Affine::new([1.0, 0.0, 0.0, -1.0, f64::from(glyph.x), f64::from(glyph.y)])
                    * glyph_transform.unwrap_or(Affine::IDENTITY);
            let world_transform = transform * glyph_transform;
            match style {
                StyleRef::Fill(fill) => {
                    let Some(geometry) = self.glyph_geometry(
                        &font,
                        font_data,
                        font_size,
                        normalized_coords,
                        glyph.id,
                        fill,
                    ) else {
                        continue;
                    };
                    for triangle in geometry.chunks_exact(3) {
                        self.append_triangle(
                            transform_point(world_transform, triangle[0]),
                            transform_point(world_transform, triangle[1]),
                            transform_point(world_transform, triangle[2]),
                            &resolved,
                        );
                    }
                }
                StyleRef::Stroke(stroke_style) => {
                    let Some(outline) =
                        glyph_outline(&font, font_size, normalized_coords, glyph.id)
                    else {
                        continue;
                    };
                    let outline = stroke(
                        outline.elements().iter().copied(),
                        stroke_style,
                        &StrokeOpts::default(),
                        PATH_TOLERANCE,
                    );
                    let geometry = tessellate(&outline, Fill::NonZero, world_transform);
                    self.append_geometry(&geometry, &resolved);
                }
            }
        }
    }

    fn draw_box_shadow(
        &mut self,
        transform: Affine,
        rect: Rect,
        color: PaintColor,
        radius: f64,
        std_dev: f64,
    ) {
        let steps = 8;
        let spread = (std_dev * 2.5).max(0.5);
        for step in (0..steps).rev() {
            let factor = (step + 1) as f64 / steps as f64;
            let inset = spread * factor;
            let shadow_rect = Rect::new(
                rect.x0 - inset,
                rect.y0 - inset,
                rect.x1 + inset,
                rect.y1 + inset,
            );
            let rounded = RoundedRect::from_rect(shadow_rect, radius + inset);
            self.fill_path(
                &rounded.into_path(PATH_TOLERANCE),
                Fill::NonZero,
                transform,
                Paint::Solid(color),
                None,
                1.0 / steps as f32,
            );
        }
    }
}

#[derive(Clone, Copy)]
struct LayerState {
    clip: ClipRect,
    alpha: f32,
}

impl LayerState {
    fn new(width: u32, height: u32) -> Self {
        Self {
            clip: ClipRect {
                x0: 0.0,
                y0: 0.0,
                x1: width as f32,
                y1: height as f32,
            },
            alpha: 1.0,
        }
    }
}

struct Batch {
    texture_key: Option<u64>,
    texture: Option<Arc<SgfxTexture>>,
    vertices: Vec<SgfxCanvasVertex>,
}

#[derive(Clone)]
enum ResolvedPaint {
    Solid(PaintColor),
    Gradient {
        gradient: Gradient,
        inverse: Affine,
        alpha: f32,
    },
    Image {
        key: u64,
        texture: Arc<SgfxTexture>,
        inverse: Affine,
        width: f32,
        height: f32,
        alpha: f32,
    },
}

impl ResolvedPaint {
    fn texture_key(&self) -> Option<u64> {
        match self {
            Self::Image { key, .. } => Some(*key),
            Self::Solid(_) | Self::Gradient { .. } => None,
        }
    }

    fn texture(&self) -> Option<Arc<SgfxTexture>> {
        match self {
            Self::Image { texture, .. } => Some(Arc::clone(texture)),
            Self::Solid(_) | Self::Gradient { .. } => None,
        }
    }

    fn vertex(&self, position: [f32; 2]) -> Vertex {
        match self {
            Self::Solid(color) => Vertex {
                position,
                color: color.components,
                uv: [0.0, 0.0],
            },
            Self::Gradient {
                gradient,
                inverse,
                alpha,
            } => {
                let local = *inverse * Point::new(f64::from(position[0]), f64::from(position[1]));
                Vertex {
                    position,
                    color: gradient_color(gradient, local, *alpha),
                    uv: [0.0, 0.0],
                }
            }
            Self::Image {
                inverse,
                width,
                height,
                alpha,
                ..
            } => {
                let local = *inverse * Point::new(f64::from(position[0]), f64::from(position[1]));
                Vertex {
                    position,
                    color: [1.0, 1.0, 1.0, alpha.clamp(0.0, 1.0)],
                    uv: [local.x as f32 / *width, local.y as f32 / *height],
                }
            }
        }
    }
}

struct TriangleGeometry {
    vertices: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

fn tessellate(path: &BezPath, fill: Fill, transform: Affine) -> TriangleGeometry {
    let path = to_lyon_path(path, transform);
    let mut geometry: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let options = FillOptions::default()
        .with_tolerance(PATH_TOLERANCE as f32)
        .with_fill_rule(match fill {
            Fill::NonZero => FillRule::NonZero,
            Fill::EvenOdd => FillRule::EvenOdd,
        });
    let _ = FillTessellator::new().tessellate_path(
        &path,
        &options,
        &mut BuffersBuilder::new(&mut geometry, |vertex: FillVertex<'_>| {
            let position = vertex.position();
            [position.x, position.y]
        }),
    );
    TriangleGeometry {
        vertices: geometry.vertices,
        indices: geometry.indices,
    }
}

fn to_lyon_path(path: &BezPath, transform: Affine) -> Path {
    let mut builder = Path::builder().with_svg();
    for element in path.elements() {
        match *element {
            PathEl::MoveTo(p) => {
                let p = transform * p;
                builder.move_to(point(p.x as f32, p.y as f32));
            }
            PathEl::LineTo(p) => {
                let p = transform * p;
                builder.line_to(point(p.x as f32, p.y as f32));
            }
            PathEl::QuadTo(p1, p2) => {
                let p1 = transform * p1;
                let p2 = transform * p2;
                builder.quadratic_bezier_to(
                    point(p1.x as f32, p1.y as f32),
                    point(p2.x as f32, p2.y as f32),
                );
            }
            PathEl::CurveTo(p1, p2, p3) => {
                let p1 = transform * p1;
                let p2 = transform * p2;
                let p3 = transform * p3;
                builder.cubic_bezier_to(
                    point(p1.x as f32, p1.y as f32),
                    point(p2.x as f32, p2.y as f32),
                    point(p3.x as f32, p3.y as f32),
                );
            }
            PathEl::ClosePath => builder.close(),
        }
    }
    builder.build()
}

#[derive(Default)]
struct BezPen {
    path: BezPath,
}

impl OutlinePen for BezPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to((f64::from(x), f64::from(y)));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to((f64::from(x), f64::from(y)));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.path.quad_to(
            (f64::from(cx0), f64::from(cy0)),
            (f64::from(x), f64::from(y)),
        );
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.path.curve_to(
            (f64::from(cx0), f64::from(cy0)),
            (f64::from(cx1), f64::from(cy1)),
            (f64::from(x), f64::from(y)),
        );
    }

    fn close(&mut self) {
        self.path.close_path();
    }
}

fn glyph_outline(
    font: &FontRef<'_>,
    font_size: f32,
    normalized_coords: &[NormalizedCoord],
    glyph_id: u32,
) -> Option<BezPath> {
    let outline = font.outline_glyphs().get(GlyphId::new(glyph_id))?;
    let coords: Vec<_> = normalized_coords
        .iter()
        .map(|coord| skrifa::instance::NormalizedCoord::from_f32(f32::from(*coord) / 16384.0))
        .collect();
    let mut pen = BezPen::default();
    outline
        .draw(
            DrawSettings::unhinted(Size::new(font_size), coords.as_slice()),
            &mut pen,
        )
        .ok()?;
    Some(pen.path)
}

#[derive(Hash, PartialEq, Eq)]
struct GlyphCacheKey {
    font_id: u64,
    font_index: u32,
    glyph_id: u32,
    font_size: u32,
    fill: u8,
    coords: Box<[i16]>,
}

#[derive(Clone, Copy)]
struct ClipRect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl ClipRect {
    fn from_kurbo(rect: Rect) -> Self {
        Self {
            x0: rect.x0 as f32,
            y0: rect.y0 as f32,
            x1: rect.x1 as f32,
            y1: rect.y1 as f32,
        }
    }

    fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }
}

#[derive(Clone, Copy)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
    uv: [f32; 2],
}

impl Vertex {
    fn is_finite(&self) -> bool {
        self.position.iter().all(|value| value.is_finite())
            && self.color.iter().all(|value| value.is_finite())
            && self.uv.iter().all(|value| value.is_finite())
    }

    fn lerp(self, other: Self, t: f32) -> Self {
        Self {
            position: lerp2(self.position, other.position, t),
            color: lerp4(self.color, other.color, t),
            uv: lerp2(self.uv, other.uv, t),
        }
    }

    fn to_sgfx(self) -> SgfxCanvasVertex {
        SgfxCanvasVertex::new([self.position[0], self.position[1], 0.0, 1.0], self.color)
            .with_tex_coord(self.uv)
    }
}

#[derive(Clone, Copy)]
enum ClipEdge {
    Left(f32),
    Right(f32),
    Top(f32),
    Bottom(f32),
}

fn clip_polygon(mut polygon: Vec<Vertex>, clip: ClipRect) -> Vec<Vertex> {
    if clip.x0 >= clip.x1 || clip.y0 >= clip.y1 {
        return Vec::new();
    }
    for edge in [
        ClipEdge::Left(clip.x0),
        ClipEdge::Right(clip.x1),
        ClipEdge::Top(clip.y0),
        ClipEdge::Bottom(clip.y1),
    ] {
        polygon = clip_edge(&polygon, edge);
        if polygon.is_empty() {
            break;
        }
    }
    polygon
}

fn clip_edge(input: &[Vertex], edge: ClipEdge) -> Vec<Vertex> {
    let mut output = Vec::with_capacity(input.len() + 1);
    let Some(mut previous) = input.last().copied() else {
        return output;
    };
    let mut previous_inside = inside(previous, edge);
    for &current in input {
        let current_inside = inside(current, edge);
        if current_inside != previous_inside {
            output.push(intersection(previous, current, edge));
        }
        if current_inside {
            output.push(current);
        }
        previous = current;
        previous_inside = current_inside;
    }
    output
}

fn inside(vertex: Vertex, edge: ClipEdge) -> bool {
    match edge {
        ClipEdge::Left(x) => vertex.position[0] >= x,
        ClipEdge::Right(x) => vertex.position[0] <= x,
        ClipEdge::Top(y) => vertex.position[1] >= y,
        ClipEdge::Bottom(y) => vertex.position[1] <= y,
    }
}

fn intersection(a: Vertex, b: Vertex, edge: ClipEdge) -> Vertex {
    let (start, end, boundary) = match edge {
        ClipEdge::Left(x) | ClipEdge::Right(x) => (a.position[0], b.position[0], x),
        ClipEdge::Top(y) | ClipEdge::Bottom(y) => (a.position[1], b.position[1], y),
    };
    let denominator = end - start;
    let t = if denominator.abs() <= f32::EPSILON {
        0.0
    } else {
        ((boundary - start) / denominator).clamp(0.0, 1.0)
    };
    a.lerp(b, t)
}

fn gradient_color(gradient: &Gradient, point: Point, alpha: f32) -> [f32; 4] {
    let raw_t = match gradient.kind {
        GradientKind::Linear(linear) => {
            let delta = linear.end - linear.start;
            let length_squared = delta.hypot2();
            if length_squared <= f64::EPSILON {
                0.0
            } else {
                ((point - linear.start).dot(delta) / length_squared) as f32
            }
        }
        GradientKind::Radial(radial) => {
            let radius = radial.end_radius - radial.start_radius;
            if radius.abs() <= f32::EPSILON {
                0.0
            } else {
                ((point - radial.end_center).hypot() as f32 - radial.start_radius) / radius
            }
        }
        GradientKind::Sweep(sweep) => {
            let vector = point - sweep.center;
            let angle = vector.y.atan2(vector.x) as f32;
            let span = sweep.end_angle - sweep.start_angle;
            if span.abs() <= f32::EPSILON {
                0.0
            } else {
                (angle - sweep.start_angle) / span
            }
        }
    };
    let t = apply_extend(raw_t, gradient.extend);
    let stops = gradient.stops.as_slice();
    if stops.is_empty() {
        return [0.0; 4];
    }
    let (before, after) = stops
        .windows(2)
        .find(|pair| t <= pair[1].offset)
        .map_or((&stops[stops.len() - 1], &stops[stops.len() - 1]), |pair| {
            (&pair[0], &pair[1])
        });
    let span = after.offset - before.offset;
    let local_t = if span.abs() <= f32::EPSILON {
        0.0
    } else {
        ((t - before.offset) / span).clamp(0.0, 1.0)
    };
    let from = before.color.to_alpha_color::<Srgb>().components;
    let to = after.color.to_alpha_color::<Srgb>().components;
    let mut color = lerp4(from, to, local_t);
    color[3] *= alpha;
    color
}

fn apply_extend(value: f32, extend: Extend) -> f32 {
    match extend {
        Extend::Pad => value.clamp(0.0, 1.0),
        Extend::Repeat => value.rem_euclid(1.0),
        Extend::Reflect => {
            let value = value.rem_euclid(2.0);
            if value <= 1.0 { value } else { 2.0 - value }
        }
    }
}

fn multiply_alpha(color: PaintColor, alpha: f32) -> PaintColor {
    let mut components = color.components;
    components[3] *= alpha.clamp(0.0, 1.0);
    PaintColor::new(components)
}

fn finite_inverse(transform: Affine) -> Affine {
    let inverse = transform.inverse();
    if inverse.as_coeffs().iter().all(|value| value.is_finite()) {
        inverse
    } else {
        Affine::IDENTITY
    }
}

fn transform_point(transform: Affine, point: [f32; 2]) -> [f32; 2] {
    let point = transform * Point::new(f64::from(point[0]), f64::from(point[1]));
    [point.x as f32, point.y as f32]
}

fn lerp2(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

fn pixel_to_clip(width: u32, height: u32) -> [f32; 16] {
    let width = width.max(1) as f32;
    let height = height.max(1) as f32;
    [
        2.0 / width,
        0.0,
        0.0,
        0.0,
        0.0,
        -2.0 / height,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
        -1.0,
        1.0,
        0.0,
        1.0,
    ]
}
