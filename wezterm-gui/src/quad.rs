// Clippy hates the implement_vertex macro and won't let me scope
// this warning to its use
#![allow(clippy::unneeded_field_pattern)]

use crate::renderstate::BorrowedLayers;
use ::window::bitmaps::TextureRect;
use ::window::color::LinearRgba;
use config::HsbTransform;

/// Each cell is composed of two triangles built from 4 vertices.
/// The buffer is organized row by row.
pub const VERTICES_PER_CELL: usize = 4;
pub const V_TOP_LEFT: usize = 0;
pub const V_TOP_RIGHT: usize = 1;
pub const V_BOT_LEFT: usize = 2;
pub const V_BOT_RIGHT: usize = 3;

/// a regular monochrome text glyph
const IS_GLYPH: f32 = 0.0;
/// a color emoji glyph
const IS_COLOR_EMOJI: f32 = 1.0;
/// a full color texture attached as the
/// background image of the window
const IS_BG_IMAGE: f32 = 2.0;
/// like 2.0, except that instead of an
/// image, we use the solid bg color
const IS_SOLID_COLOR: f32 = 3.0;
/// Grayscale poly quad for non-aa text render layers
const IS_GRAY_SCALE: f32 = 4.0;

#[repr(C)]
#[derive(Copy, Clone, Default, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    // Physical position of the corner of the character cell
    pub position: [f32; 2],
    // glyph texture
    pub tex: [f32; 2],
    pub fg_color: [f32; 4],
    pub alt_color: [f32; 4],
    pub hsv: [f32; 3],
    pub has_color: f32,
    pub mix_value: f32,
}
::window::glium::implement_vertex!(
    Vertex, position, tex, fg_color, alt_color, hsv, has_color, mix_value
);

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32x4,
    3 => Float32x4,
    4 => Float32x3,
    5 => Float32,
    6 => Float32,
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

pub trait QuadTrait {
    /// Assign the texture coordinates
    fn set_texture(&mut self, coords: TextureRect) {
        let x1 = coords.min_x();
        let x2 = coords.max_x();
        let y1 = coords.min_y();
        let y2 = coords.max_y();
        self.set_texture_discrete(x1, x2, y1, y2);
    }
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32);
    fn set_has_color_impl(&mut self, has_color: f32);

    /// Set the color glyph "flag"
    fn set_has_color(&mut self, has_color: bool) {
        self.set_has_color_impl(if has_color { IS_COLOR_EMOJI } else { IS_GLYPH });
    }

    /// Mark as a grayscale polyquad; color and alpha will be
    /// multipled with those in the texture
    fn set_grayscale(&mut self) {
        self.set_has_color_impl(IS_GRAY_SCALE);
    }

    /// Mark this quad as a background image.
    /// Mutually exclusive with set_has_color.
    fn set_is_background_image(&mut self) {
        self.set_has_color_impl(IS_BG_IMAGE);
    }

    fn set_is_background(&mut self) {
        self.set_has_color_impl(IS_SOLID_COLOR);
    }

    fn set_fg_color(&mut self, color: LinearRgba);

    /// Must be called after set_fg_color
    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32);

    fn set_hsv(&mut self, hsv: Option<HsbTransform>);
    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32);
}

pub enum QuadImpl<'a> {
    Vert(Quad<'a>),
    Boxed(&'a mut BoxedQuad),
}

impl<'a> QuadTrait for QuadImpl<'a> {
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32) {
        match self {
            Self::Vert(q) => q.set_texture_discrete(x1, x2, y1, y2),
            Self::Boxed(q) => q.set_texture_discrete(x1, x2, y1, y2),
        }
    }

    fn set_has_color_impl(&mut self, has_color: f32) {
        match self {
            Self::Vert(q) => q.set_has_color_impl(has_color),
            Self::Boxed(q) => q.set_has_color_impl(has_color),
        }
    }

    fn set_fg_color(&mut self, color: LinearRgba) {
        match self {
            Self::Vert(q) => q.set_fg_color(color),
            Self::Boxed(q) => q.set_fg_color(color),
        }
    }

    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32) {
        match self {
            Self::Vert(q) => q.set_alt_color_and_mix_value(color, mix_value),
            Self::Boxed(q) => q.set_alt_color_and_mix_value(color, mix_value),
        }
    }

    fn set_hsv(&mut self, hsv: Option<HsbTransform>) {
        match self {
            Self::Vert(q) => q.set_hsv(hsv),
            Self::Boxed(q) => q.set_hsv(hsv),
        }
    }

    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32) {
        match self {
            Self::Vert(q) => q.set_position(left, top, right, bottom),
            Self::Boxed(q) => q.set_position(left, top, right, bottom),
        }
    }
}

/// A helper for updating the 4 vertices that compose a glyph cell
pub struct Quad<'a> {
    pub(crate) vert: &'a mut [Vertex],
}

impl<'a> QuadTrait for Quad<'a> {
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32) {
        self.vert[V_TOP_LEFT].tex = [x1, y1];
        self.vert[V_TOP_RIGHT].tex = [x2, y1];
        self.vert[V_BOT_LEFT].tex = [x1, y2];
        self.vert[V_BOT_RIGHT].tex = [x2, y2];
    }

    fn set_has_color_impl(&mut self, has_color: f32) {
        for v in self.vert.iter_mut() {
            v.has_color = has_color;
        }
    }

    fn set_fg_color(&mut self, color: LinearRgba) {
        for v in self.vert.iter_mut() {
            v.fg_color = color.into();
        }
        self.set_alt_color_and_mix_value(color, 0.);
    }

    /// Must be called after set_fg_color
    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32) {
        for v in self.vert.iter_mut() {
            v.alt_color = color.into();
            v.mix_value = mix_value;
        }
    }

    fn set_hsv(&mut self, hsv: Option<HsbTransform>) {
        let (h, s, v) = hsv
            .map(|t| (t.hue, t.saturation, t.brightness))
            .unwrap_or((1., 1., 1.));
        for vert in self.vert.iter_mut() {
            vert.hsv = [h, s, v];
        }
    }

    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32) {
        self.vert[V_TOP_LEFT].position = [left, top];
        self.vert[V_TOP_RIGHT].position = [right, top];
        self.vert[V_BOT_LEFT].position = [left, bottom];
        self.vert[V_BOT_RIGHT].position = [right, bottom];
    }
}

pub trait QuadAllocator {
    fn allocate(&mut self) -> anyhow::Result<QuadImpl<'_>>;
    fn extend_with(&mut self, vertices: &[Vertex]);
}

pub trait TripleLayerQuadAllocatorTrait {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>>;
    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]);
}

/// We prefer to allocate a quad at a time for HeapQuadAllocator
/// because we tend to end up with fairly large arrays of Vertex
/// and the total amount of contiguous memory is in the MB range,
/// which is a bit gnarly to reallocate, and can waste several MB
/// in unused capacity
#[derive(Default)]
pub struct BoxedQuad {
    position: (f32, f32, f32, f32),
    fg_color: [f32; 4],
    alt_color: [f32; 4],
    tex: (f32, f32, f32, f32),
    hsv: [f32; 3],
    has_color: f32,
    mix_value: f32,
}

impl QuadTrait for BoxedQuad {
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32) {
        self.tex = (x1, x2, y1, y2);
    }

    fn set_has_color_impl(&mut self, has_color: f32) {
        self.has_color = has_color;
    }

    fn set_fg_color(&mut self, color: LinearRgba) {
        self.fg_color = color.into();
    }
    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32) {
        self.alt_color = color.into();
        self.mix_value = mix_value;
    }
    fn set_hsv(&mut self, hsv: Option<HsbTransform>) {
        let (h, s, v) = hsv
            .map(|t| (t.hue, t.saturation, t.brightness))
            .unwrap_or((1., 1., 1.));
        self.hsv = [h, s, v];
    }

    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32) {
        self.position = (left, top, right, bottom);
    }
}

impl BoxedQuad {
    fn from_vertices(verts: &[Vertex; VERTICES_PER_CELL]) -> Self {
        let [x1, y1] = verts[V_TOP_LEFT].tex;
        let [x2, y2] = verts[V_BOT_RIGHT].tex;

        let [left, top] = verts[V_TOP_LEFT].position;
        let [right, bottom] = verts[V_BOT_RIGHT].position;
        Self {
            tex: (x1, x2, y1, y2),
            position: (left, top, right, bottom),
            has_color: verts[V_TOP_LEFT].has_color,
            alt_color: verts[V_TOP_LEFT].alt_color,
            fg_color: verts[V_TOP_LEFT].fg_color,
            hsv: verts[V_TOP_LEFT].hsv,
            mix_value: verts[V_TOP_LEFT].mix_value,
        }
    }

    fn to_vertices(&self) -> [Vertex; VERTICES_PER_CELL] {
        let mut vert: [Vertex; VERTICES_PER_CELL] = Default::default();
        let mut quad = Quad { vert: &mut vert };

        let (x1, x2, y1, y2) = self.tex;
        quad.set_texture_discrete(x1, x2, y1, y2);

        let (left, top, right, bottom) = self.position;
        quad.set_position(left, top, right, bottom);

        quad.set_has_color_impl(self.has_color);
        let [hue, saturation, brightness] = self.hsv;
        quad.set_hsv(Some(HsbTransform {
            hue,
            saturation,
            brightness,
        }));
        quad.set_fg_color(LinearRgba::with_components(
            self.fg_color[0],
            self.fg_color[1],
            self.fg_color[2],
            self.fg_color[3],
        ));
        quad.set_alt_color_and_mix_value(self.alt_color.into(), self.mix_value);

        vert
    }
}

#[derive(Default)]
pub struct HeapQuadAllocator {
    layer0: Vec<Box<BoxedQuad>>,
    layer1: Vec<Box<BoxedQuad>>,
    layer2: Vec<Box<BoxedQuad>>,
}

impl std::fmt::Debug for HeapQuadAllocator {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("HeapQuadAllocator").finish()
    }
}

impl HeapQuadAllocator {
    /// Translate and clip glyph/background geometry, including its texture
    /// coordinates. Used for pixel scrolling without moving pane/status borders.
    pub fn apply_y_clipped(&self, other: &mut TripleLayerQuadAllocator, dy: f32, top: f32, bottom: f32) {
        self.apply_rect_clipped(other, dy, (f32::NEG_INFINITY, top, f32::INFINITY, bottom), false);
    }

    /// The cursor is a separate, stationary layer. Never retain its pixels in
    /// scroll snapshots, or paint moving glyphs underneath its current cell.
    pub fn apply_y_clipped_excluding(&self, other: &mut TripleLayerQuadAllocator,
        dy: f32, top: f32, bottom: f32, hole: Option<(f32, f32, f32, f32)>) {
        if let Some((left, y0, right, y1)) = hole {
            self.apply_y_clipped(other, dy, top, bottom.min(y0));
            self.apply_y_clipped(other, dy, top.max(y1), bottom);
            self.apply_rect_clipped(other, dy, (f32::NEG_INFINITY, top.max(y0), left, bottom.min(y1)), false);
            self.apply_rect_clipped(other, dy, (right, top.max(y0), f32::INFINITY, bottom.min(y1)), false);
        } else {
            self.apply_y_clipped(other, dy, top, bottom);
        }
    }

    /// Composite a moving screen through a mask expressed in screen pixels.
    pub fn apply_masked(&self, other: &mut TripleLayerQuadAllocator, dy: f32,
        rect: (f32, f32, f32, f32), holes: &[(f32, f32, f32, f32)]) {
        let mut regions = vec![rect];
        for &(x0, y0, x1, y1) in holes {
            let mut remaining = Vec::new();
            for (l, t, r, b) in regions {
                let (a, c, d, e) = (l.max(x0), t.max(y0), r.min(x1), b.min(y1));
                if a >= d || c >= e { remaining.push((l,t,r,b)); continue; }
                if t < c { remaining.push((l,t,r,c)); }
                if e < b { remaining.push((l,e,r,b)); }
                if l < a { remaining.push((l,c,a,e)); }
                if d < r { remaining.push((d,c,r,e)); }
            }
            regions = remaining;
        }
        for region in regions { self.apply_rect_clipped(other, dy, region, false); }
    }

    pub fn apply_rect_clipped(&self, other: &mut TripleLayerQuadAllocator,
        dy: f32, rect: (f32, f32, f32, f32), overlay: bool) {
        let (left, top, right, bottom) = rect;
        for (layer_num, quads) in [(0, &self.layer0), (1, &self.layer1), (2, &self.layer2)] {
            for quad in quads {
                let mut vertices = quad.to_vertices();
                if clip_y(&mut vertices, dy, top, bottom) && clip_x(&mut vertices, left, right) {
                    other.extend_with(if overlay { 2 } else { layer_num }, &vertices);
                }
            }
        }
    }

    pub fn apply_to(&self, other: &mut dyn TripleLayerQuadAllocatorTrait) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        for (layer_num, quads) in [(0, &self.layer0), (1, &self.layer1), (2, &self.layer2)] {
            for quad in quads {
                other.extend_with(layer_num, &quad.to_vertices());
            }
        }
        metrics::histogram!("quad_buffer_apply").record(start.elapsed());
        Ok(())
    }
}

impl TripleLayerQuadAllocatorTrait for HeapQuadAllocator {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        let quads = match layer_num {
            0 => &mut self.layer0,
            1 => &mut self.layer1,
            2 => &mut self.layer2,
            _ => unreachable!(),
        };

        quads.push(Box::new(BoxedQuad::default()));

        let quad = quads.last_mut().unwrap();
        Ok(QuadImpl::Boxed(quad))
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        if vertices.is_empty() {
            return;
        }

        let dest_quads = match layer_num {
            0 => &mut self.layer0,
            1 => &mut self.layer1,
            2 => &mut self.layer2,
            _ => unreachable!(),
        };

        // This is logically equivalent to
        // https://doc.rust-lang.org/std/primitive.slice.html#method.as_chunks_unchecked
        // which is currently nightly-only
        assert_eq!(vertices.len() % VERTICES_PER_CELL, 0);
        let src_quads: &[[Vertex; VERTICES_PER_CELL]] =
            unsafe { std::slice::from_raw_parts(vertices.as_ptr().cast(), vertices.len() / 4) };

        for quad in src_quads {
            dest_quads.push(Box::new(BoxedQuad::from_vertices(quad)));
        }
    }
}

pub enum TripleLayerQuadAllocator<'a> {
    Gpu(BorrowedLayers),
    Heap(&'a mut HeapQuadAllocator),
}

impl<'a> TripleLayerQuadAllocatorTrait for TripleLayerQuadAllocator<'a> {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        match self {
            Self::Gpu(b) => b.allocate(layer_num),
            Self::Heap(h) => h.allocate(layer_num),
        }
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        match self {
            Self::Gpu(b) => b.extend_with(layer_num, vertices),
            Self::Heap(h) => h.extend_with(layer_num, vertices),
        }
    }
}

#[cfg(test)]
#[test]
fn size() {
    assert_eq!(std::mem::size_of::<Vertex>() * VERTICES_PER_CELL, 272);
    assert_eq!(std::mem::size_of::<BoxedQuad>(), 84);
}

fn clip_y(vertices: &mut [Vertex; 4], dy: f32, top: f32, bottom: f32) -> bool {
    let y0 = vertices[V_TOP_LEFT].position[1] + dy;
    let y1 = vertices[V_BOT_LEFT].position[1] + dy;
    let lo = y0.max(top);
    let hi = y1.min(bottom);
    if hi <= lo || y1 <= y0 { return false; }
    let t0 = vertices[V_TOP_LEFT].tex[1];
    let t1 = vertices[V_BOT_LEFT].tex[1];
    for index in [V_TOP_LEFT, V_TOP_RIGHT] {
        vertices[index].position[1] = lo;
        vertices[index].tex[1] = t0 + (t1-t0) * (lo-y0)/(y1-y0);
    }
    for index in [V_BOT_LEFT, V_BOT_RIGHT] {
        vertices[index].position[1] = hi;
        vertices[index].tex[1] = t0 + (t1-t0) * (hi-y0)/(y1-y0);
    }
    true
}

#[cfg(test)]
#[test]
fn scroll_clip_preserves_texture_coordinates() {
    let mut q = BoxedQuad::default();
    q.set_position(0., 10., 10., 30.);
    q.set_texture_discrete(0., 1., 0., 1.);
    let mut v = q.to_vertices();
    assert!(clip_y(&mut v, -5., 10., 20.));
    assert_eq!(v[V_TOP_LEFT].position[1], 10.);
    assert_eq!(v[V_BOT_LEFT].position[1], 20.);
    assert_eq!(v[V_TOP_LEFT].tex[1], 0.25);
    assert_eq!(v[V_BOT_LEFT].tex[1], 0.75);
    assert!(!clip_y(&mut q.to_vertices(), 100., 0., 50.));
}

fn clip_x(vertices: &mut [Vertex; 4], left: f32, right: f32) -> bool {
    let x0 = vertices[V_TOP_LEFT].position[0];
    let x1 = vertices[V_TOP_RIGHT].position[0];
    let lo = x0.max(left);
    let hi = x1.min(right);
    if hi <= lo || x1 <= x0 { return false; }
    let t0 = vertices[V_TOP_LEFT].tex[0];
    let t1 = vertices[V_TOP_RIGHT].tex[0];
    for index in [V_TOP_LEFT, V_BOT_LEFT] {
        vertices[index].position[0] = lo;
        vertices[index].tex[0] = t0 + (t1-t0)*(lo-x0)/(x1-x0);
    }
    for index in [V_TOP_RIGHT, V_BOT_RIGHT] {
        vertices[index].position[0] = hi;
        vertices[index].tex[0] = t0 + (t1-t0)*(hi-x0)/(x1-x0);
    }
    true
}

#[cfg(test)]
#[test]
fn scrolling_content_excludes_current_cursor_cell() {
    let mut source = HeapQuadAllocator::default();
    let mut quad = source.allocate(0).unwrap();
    quad.set_position(0., 0., 40., 40.);
    quad.set_texture_discrete(0., 1., 0., 1.);
    drop(quad);
    let mut result = HeapQuadAllocator::default();
    source.apply_y_clipped_excluding(&mut TripleLayerQuadAllocator::Heap(&mut result),
        0., 0., 40., Some((10., 10., 20., 30.)));
    let area: f32 = result.layer0.iter().map(|q| {
        let (x0,y0,x1,y1) = q.position;
        assert!(x1 <= 10. || x0 >= 20. || y1 <= 10. || y0 >= 30.);
        (x1-x0)*(y1-y0)
    }).sum();
    assert_eq!(area, 1400.);
    let left = result.layer0.iter().find(|q| q.position == (0.,10.,10.,30.)).unwrap();
    assert_eq!(left.tex, (0.,0.25,0.25,0.75));
}
