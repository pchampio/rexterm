//! Program tiles in the tab bar, stacked as gorex (github.com/egoist/gorex)
//! stacks the programs of a tab.
//!
//! Tab bar text asks for a stack with `OSC 7778 ; icon ; FRONT,BACK1,BACK2 ;
//! CELLS ST`. The text parser turns that into CELLS blank cells. The first
//! cell carries a marker (a hyperlink with the `wezterm-icon:` scheme), so the
//! request stays with its cells when the tab bar line is put together. The
//! painter draws the stack over those cells, as high as the row allows.
//!
//! The files are `NAME.png` (the front tile, with its glyph) and
//! `NAME-back1.png`, `NAME-back2.png` (tiles behind it) in `tab_bar_icon_dir`.
//! All of them share one canvas of 25 x 20 units; a tile behind sits 5 units
//! more to the right (see scripts/build-program-icons).
//!
//! `OSC 7778 ; status ; KIND ; CELLS ST` asks for an agent status indicator
//! instead: three dots that move while the agent works, a "?" bubble that
//! pulses while it waits for an answer, "!" for an approval or a sign-in, a
//! check when it is done and a cross after an error. The painter draws them
//! (no files), so they stay sharp at every size.
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use image::{imageops, RgbaImage};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use termwiz::escape::OperatingSystemCommand;
use termwiz::image::{ImageData, ImageDataType};
use window::color::LinearRgba;

const MARKER: &str = "wezterm-icon:";
const STATUS_MARKER: &str = "wezterm-status:";
/// The status kinds that the painter knows.
pub(crate) const STATUS_KINDS: &[&str] = &["working", "question", "approval", "signin", "done", "error"];
/// The most tiles in one stack.
const MAX_STACK: usize = 3;
/// The canvas of every tile file, in units.
const CANVAS: (f32, f32) = (25., 20.);
/// How far a tile behind peeks out, in units.
const STEP: f32 = 5.;
/// The most cells that a stack takes.
const MAX_CELLS: usize = 8;

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

/// The marker and the number of cells for an icon request, or None for any
/// other sequence.
pub fn marker_from_osc(osc: &OperatingSystemCommand) -> Option<(String, usize)> {
    let OperatingSystemCommand::Unspecified(parts) = osc else {
        return None;
    };
    let text = |index: usize| parts.get(index).and_then(|part| std::str::from_utf8(part).ok());
    if text(0) == Some("7778") && text(1) == Some("status") {
        let kind = text(2).filter(|kind| STATUS_KINDS.contains(kind))?;
        let cells = text(3).and_then(|cells| cells.parse::<usize>().ok()).unwrap_or(2).clamp(1, 4);
        return Some((format!("{STATUS_MARKER}{kind}:{cells}"), cells));
    }
    if text(0) != Some("7778") || text(1) != Some("icon") {
        return None;
    }
    let names: Vec<&str> = text(2)?.split(',').take(MAX_STACK).collect();
    if !names.iter().all(|name| valid_name(name)) {
        return None;
    }
    let cells = text(3).and_then(|cells| cells.parse::<usize>().ok()).unwrap_or(2).clamp(1, MAX_CELLS);
    // `button`: the tiles sit on a rounded button (the agent button).
    let button = if text(4) == Some("button") { ":button" } else { "" };
    Some((format!("{MARKER}{}:{cells}{button}", names.join(",")), cells))
}

fn parse_marker(uri: &str) -> Option<(Vec<&str>, usize, bool)> {
    let rest = uri.strip_prefix(MARKER)?;
    let (rest, button) = match rest.strip_suffix(":button") {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let (names, cells) = rest.rsplit_once(':')?;
    Some((names.split(',').collect(), cells.parse().ok()?, button))
}

/// The program tiles that a tab bar title line asks for (front first).
pub(crate) fn icon_names(line: &wezterm_term::Line) -> Option<Vec<String>> {
    line.visible_cells().find_map(|cell| {
        let link = cell.attrs().hyperlink()?;
        let (names, _, button) = parse_marker(link.uri())?;
        (!button).then(|| names.iter().map(|n| n.to_string()).collect())
    })
}

fn parse_status_marker(uri: &str) -> Option<(&str, usize)> {
    let (kind, cells) = uri.strip_prefix(STATUS_MARKER)?.split_once(':')?;
    STATUS_KINDS.contains(&kind).then_some(())?;
    Some((kind, cells.parse().ok()?))
}

/// Frames of one animation cycle, and the time of one frame.
const FRAMES: u32 = 12;
const FRAME_TIME: Duration = Duration::from_millis(100);

/// Whether the indicator moves.
pub(crate) fn status_animated(kind: &str) -> bool {
    matches!(kind, "working" | "question" | "approval" | "signin")
}

/// The animation frame now (0 for an indicator that does not move).
fn status_frame(kind: &str) -> u32 {
    static START: OnceLock<Instant> = OnceLock::new();
    if !status_animated(kind) {
        return 0;
    }
    (START.get_or_init(Instant::now).elapsed().as_millis() / FRAME_TIME.as_millis()) as u32 % FRAMES
}

fn rgb(hex: u32) -> [f32; 3] {
    [((hex >> 16) & 255) as f32, ((hex >> 8) & 255) as f32, (hex & 255) as f32]
}

/// Distance from `p` to the segment `a`-`b`.
fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy).max(1e-6)).clamp(0., 1.);
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}

/// Distance from `p` to the arc of radius `r` around `c`, from angle `from`
/// to angle `to` (radians, y down, `from` < `to`).
fn arc_distance(p: (f32, f32), c: (f32, f32), r: f32, from: f32, to: f32) -> f32 {
    use std::f32::consts::TAU;
    let mut angle = (p.1 - c.1).atan2(p.0 - c.0);
    while angle < from {
        angle += TAU;
    }
    if angle <= to {
        return (((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt() - r).abs();
    }
    let end = |a: f32| (c.0 + r * a.cos(), c.1 + r * a.sin());
    let (s, e) = (end(from), end(to));
    ((p.0 - s.0).powi(2) + (p.1 - s.1).powi(2)).sqrt().min(((p.0 - e.0).powi(2) + (p.1 - e.1).powi(2)).sqrt())
}

/// The indicator `kind` at animation `frame`, `width` x `height` pixels.
/// Shapes are distance functions: every edge is anti-aliased.
pub(crate) fn status_image(kind: &str, frame: u32, width: u32, height: u32) -> RgbaImage {
    use std::f32::consts::PI;
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let mut image = RgbaImage::new(width.max(1), height.max(1));
    let phase = frame as f32 / FRAMES as f32;
    let (cx, cy) = (w / 2., h / 2.);
    let size = h.min(w);
    // Solarized accents.
    let (blue, yellow, orange, green, red, white) =
        (rgb(0x268bd2), rgb(0xb58900), rgb(0xcb4b16), rgb(0x859900), rgb(0xdc322f), rgb(0xfdf6e3));
    // Each shape: a colour and its coverage at a point (1 inside, 0 out).
    let mut layers: Vec<([f32; 3], Box<dyn Fn(f32, f32) -> f32>)> = vec![];
    let cover = |d: f32| (0.5 - d).clamp(0., 1.);
    match kind {
        "working" => {
            // Three dots; a bump runs through them.
            // Three dots and their gaps fit in 80% of the width.
            let r = (size * 0.17).min(w * 0.1).max(1.2);
            let gap = r * 3.;
            for i in 0..3 {
                let t = (phase - i as f32 * 0.16).rem_euclid(1.);
                let bump = if t < 0.45 { (PI * t / 0.45).sin() } else { 0. };
                let (x, y) = (cx + (i as f32 - 1.) * gap, cy - bump * size * 0.14);
                let alpha = 0.6 + 0.4 * bump;
                layers.push((blue, Box::new(move |px, py| alpha * cover(((px - x).powi(2) + (py - y).powi(2)).sqrt() - r))));
            }
        }
        "done" | "error" | "question" | "approval" | "signin" => {
            let colour = match kind {
                "done" => green,
                "error" => red,
                "question" => yellow,
                "approval" => orange,
                _ => blue,
            };
            let radius = size * 0.4;
            if status_animated(kind) {
                // A soft ring grows out of the bubble and fades.
                let grow = radius + phase * size * 0.12;
                let alpha = 0.45 * (1. - phase);
                layers.push((colour, Box::new(move |px, py| {
                    let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                    alpha * cover((d - grow).abs() - size * 0.03)
                })));
            }
            layers.push((colour, Box::new(move |px, py| cover(((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - radius))));
            let stroke = (size * 0.05).max(1.0);
            let dot = (size * 0.06).max(1.1);
            let mark: Box<dyn Fn(f32, f32) -> f32> = match kind {
                "done" => Box::new(move |px, py| {
                    let a = (cx - size * 0.17, cy + size * 0.01);
                    let b = (cx - size * 0.05, cy + size * 0.13);
                    let c = (cx + size * 0.18, cy - size * 0.14);
                    cover(segment_distance((px, py), a, b).min(segment_distance((px, py), b, c)) - stroke)
                }),
                "error" => Box::new(move |px, py| {
                    let k = size * 0.13;
                    let d = segment_distance((px, py), (cx - k, cy - k), (cx + k, cy + k))
                        .min(segment_distance((px, py), (cx - k, cy + k), (cx + k, cy - k)));
                    cover(d - stroke)
                }),
                "question" => Box::new(move |px, py| {
                    let (qc, r) = ((cx, cy - size * 0.09), size * 0.11);
                    let end = (qc.0 + r * (PI / 4.).cos(), qc.1 + r * (PI / 4.).sin());
                    let d = arc_distance((px, py), qc, r, PI * 1.1, PI * 2.25)
                        .min(segment_distance((px, py), end, (cx, cy + size * 0.07)));
                    let spot = ((px - cx).powi(2) + (py - cy - size * 0.19).powi(2)).sqrt() - dot;
                    cover(d - stroke).max(cover(spot))
                }),
                _ => Box::new(move |px, py| {
                    let d = segment_distance((px, py), (cx, cy - size * 0.2), (cx, cy + size * 0.06));
                    let spot = ((px - cx).powi(2) + (py - cy - size * 0.19).powi(2)).sqrt() - dot;
                    cover(d - stroke).max(cover(spot))
                }),
            };
            layers.push((white, mark));
        }
        _ => {}
    }
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        // "Over" compositing of the layers, in straight alpha.
        let (mut colour, mut alpha) = ([0f32; 3], 0f32);
        for (c, coverage) in &layers {
            let a = coverage(px, py);
            if a <= 0. {
                continue;
            }
            let out = a + alpha * (1. - a);
            for i in 0..3 {
                colour[i] = (c[i] * a + colour[i] * alpha * (1. - a)) / out.max(1e-6);
            }
            alpha = out;
        }
        pixel.0 = [colour[0] as u8, colour[1] as u8, colour[2] as u8, (alpha * 255.).round() as u8];
    }
    image
}

static STATUS: Mutex<Option<HashMap<(String, u32, u32, u32), Arc<ImageData>>>> = Mutex::new(None);

/// The indicator image for now, kept for later frames.
fn status_icon(kind: &str, width: u32, height: u32) -> Arc<ImageData> {
    let frame = status_frame(kind);
    let mut cache = STATUS.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache.len() > 1024 {
        cache.clear();
    }
    cache
        .entry((kind.to_owned(), frame, width, height))
        .or_insert_with(|| {
            let image = status_image(kind, frame, width, height);
            let (w, h) = (image.width(), image.height());
            Arc::new(ImageData::with_data(ImageDataType::new_single_frame(w, h, image.into_raw())))
        })
        .clone()
}

fn load(dir: &Path, file: &str) -> Option<RgbaImage> {
    Some(image::open(dir.join(format!("{file}.png"))).ok()?.into_rgba8())
}

/// The stack of `tiles` (front first) in a box of `width` x `height` pixels.
fn compose(tiles: &[Arc<RgbaImage>], width: u32, height: u32) -> RgbaImage {
    let behind = tiles.len().saturating_sub(1) as f32;
    let unit = (height as f32 / CANVAS.1).min(width as f32 / (CANVAS.0 + STEP * behind)).max(0.1);
    let tile_w = (CANVAS.0 * unit).round().max(1.) as u32;
    let tile_h = (CANVAS.1 * unit).round().max(1.) as u32;
    let total_w = ((CANVAS.0 + STEP * behind) * unit).round().max(1.) as u32;
    let mut canvas = RgbaImage::new(total_w, tile_h);
    for (depth, tile) in tiles.iter().enumerate().rev() {
        let layer = imageops::resize(tile.as_ref(), tile_w, tile_h, imageops::FilterType::Lanczos3);
        let left = (STEP * depth as f32 * unit).round() as i64;
        imageops::overlay(&mut canvas, &layer, left, 0);
    }
    canvas
}

static TILES: Mutex<Option<HashMap<String, Option<Arc<RgbaImage>>>>> = Mutex::new(None);
type Key = (String, u32, u32);
static ICONS: Mutex<Option<HashMap<Key, Option<Arc<ImageData>>>>> = Mutex::new(None);

fn tile(dir: &Path, file: &str) -> Option<Arc<RgbaImage>> {
    TILES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(format!("{}/{file}", dir.display()))
        .or_insert_with(|| load(dir, file).map(Arc::new))
        .clone()
}

/// The finished stack for `names`, in a box of `width` x `height` pixels.
pub(crate) fn icon(dir: &Path, names: &[&str], width: u32, height: u32) -> Option<Arc<ImageData>> {
    let key = (format!("{}/{}", dir.display(), names.join(",")), width, height);
    if let Some(found) = ICONS.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return found.clone();
    }
    let mut tiles = vec![];
    for (depth, name) in names.iter().enumerate() {
        let file = if depth == 0 { name.to_string() } else { format!("{name}-back{depth}") };
        // A file that is missing: the front tile alone is still useful.
        match tile(dir, &file) {
            Some(found) => tiles.push(found),
            None if depth == 0 => break,
            None => {}
        }
    }
    let image = (!tiles.is_empty()).then(|| {
        let canvas = compose(&tiles, width, height);
        let (w, h) = (canvas.width(), canvas.height());
        Arc::new(ImageData::with_data(ImageDataType::new_single_frame(w, h, canvas.into_raw())))
    });
    let mut icons = ICONS.lock().unwrap();
    let icons = icons.get_or_insert_with(HashMap::new);
    // Sizes change with the font size; do not keep every old size.
    if icons.len() > 512 {
        icons.clear();
    }
    icons.insert(key, image.clone());
    image
}

impl crate::TermWindow {
    /// Draw the status indicator `kind` in the box at `x`, `y` (pixels).
    pub(crate) fn paint_status(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        kind: &str,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> anyhow::Result<()> {
        let image = status_icon(kind, width.max(1.) as u32, height.max(1.) as u32);
        let gl_state = self.render_state.as_ref().unwrap();
        let (sprite, _, _) = gl_state.glyph_cache.borrow_mut().cached_image(&image, None, self.allow_images)?;
        if status_animated(kind) {
            self.update_next_frame_time(Some(Instant::now() + FRAME_TIME));
            self.status_animation.set(true);
        }
        let (w, h) = (sprite.coords.size.width as f32, sprite.coords.size.height as f32);
        let (left, top) = (x + ((width - w) / 2.).floor(), y + ((height - h) / 2.).floor());
        let mut quad = layers.allocate(2)?;
        let (ox, oy) = (self.dimensions.pixel_width as f32 / 2., self.dimensions.pixel_height as f32 / 2.);
        quad.set_position(left - ox, top - oy, left + w - ox, top + h - oy);
        quad.set_hsv(None);
        quad.set_fg_color(LinearRgba::with_components(1., 1., 1., 1.));
        quad.set_texture(sprite.texture_coords());
        quad.set_has_color(true);
        Ok(())
    }

    /// Draw a stack of program tiles in `area` (pixels), as high as it is.
    /// Returns the width that it took.
    pub(crate) fn paint_tiles(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        names: &[&str],
        x: f32,
        y: f32,
        max_width: f32,
        height: f32,
    ) -> anyhow::Result<f32> {
        let Some(dir) = self.config.tab_bar_icon_dir.as_ref() else {
            return Ok(0.);
        };
        let Some(image) = icon(dir, names, max_width.max(1.) as u32, height.max(1.) as u32) else {
            return Ok(0.);
        };
        let gl_state = self.render_state.as_ref().unwrap();
        let (sprite, next_due, _) = gl_state.glyph_cache.borrow_mut().cached_image(&image, None, self.allow_images)?;
        self.update_next_frame_time(next_due);
        let (w, h) = (sprite.coords.size.width as f32, sprite.coords.size.height as f32);
        let top = y + ((height - h) / 2.).floor();
        let mut quad = layers.allocate(2)?;
        let (ox, oy) = (self.dimensions.pixel_width as f32 / 2., self.dimensions.pixel_height as f32 / 2.);
        quad.set_position(x - ox, top - oy, x + w - ox, top + h - oy);
        quad.set_hsv(None);
        quad.set_fg_color(LinearRgba::with_components(1., 1., 1., 1.));
        quad.set_texture(sprite.texture_coords());
        quad.set_has_color(true);
        Ok(w)
    }

    /// Draw the icons that the cells of `line` ask for. `left` and `top` are
    /// the pixel position of the first cell of the line.
    pub fn paint_line_icons(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        palette: &wezterm_term::color::ColorPalette,
        line: &wezterm_term::Line,
        left: f32,
        top: f32,
    ) -> anyhow::Result<()> {
        let Some(dir) = self.config.tab_bar_icon_dir.as_ref() else {
            return Ok(());
        };
        let cell_width = self.render_metrics.cell_size.width as f32;
        // The text row (the strip under the tabs is below it).
        let cell_height = self.render_metrics.cell_size.height as f32;
        for cell in line.visible_cells() {
            let Some(link) = cell.attrs().hyperlink() else {
                continue;
            };
            if let Some((kind, cells)) = parse_status_marker(link.uri()) {
                let x = left + cell.cell_index() as f32 * cell_width;
                self.paint_status(layers, kind, x, top, cells as f32 * cell_width, cell_height)?;
                continue;
            }
            let Some((names, cells, button)) = parse_marker(link.uri()) else {
                continue;
            };
            if button {
                // A rounded button under the tiles; darker under the pointer.
                let bg = palette.resolve_bg(wezterm_term::color::ColorAttribute::Default).to_linear();
                let fg = palette.resolve_fg(wezterm_term::color::ColorAttribute::Default).to_linear();
                let r = euclid::rect(
                    left + cell.cell_index() as f32 * cell_width + 1.,
                    top + 1.,
                    cells as f32 * cell_width - 2.,
                    cell_height - 2.,
                );
                let hover = self.current_mouse_event.as_ref().is_some_and(|event| {
                    let (x, y) = (event.coords.x as f32, event.coords.y as f32);
                    x >= r.min_x() && x < r.max_x() && y >= r.min_y() && y < r.max_y()
                }) || self.agent_panel.is_some();
                let t = if hover { 0.22 } else { 0.12 };
                let colour = LinearRgba::with_components(bg.0 + (fg.0 - bg.0) * t, bg.1 + (fg.1 - bg.1) * t, bg.2 + (fg.2 - bg.2) * t, 1.);
                self.fill_rounded(r, (cell_height * 0.35).round(), colour, 1, layers)?;
            }
            // As high as the row, inside the cells (half a cell is left free
            // at each side). On a button, a little smaller.
            let width = if button {
                ((cells as f32 - 1.) * cell_width).max(cell_width).floor() as u32
            } else {
                (cells as f32 * cell_width - 2.).max(cell_width).floor() as u32
            };
            let height = if button { (cell_height * 0.8).floor() } else { cell_height.floor() } as u32;
            let Some(image) = icon(dir, &names, width, height) else {
                continue;
            };
            let gl_state = self.render_state.as_ref().unwrap();
            let (sprite, next_due, _) = gl_state
                .glyph_cache
                .borrow_mut()
                .cached_image(&image, None, self.allow_images)?;
            self.update_next_frame_time(next_due);
            let (w, h) = (sprite.coords.size.width as f32, sprite.coords.size.height as f32);
            let scale = (cell_height / h).min(1.);
            let (w, h) = (w * scale, h * scale);
            // In the middle of its cells.
            let x = left + cell.cell_index() as f32 * cell_width + ((cells as f32 * cell_width - w) / 2.).floor();
            let y = top + ((cell_height - h) / 2.).floor();
            let mut quad = layers.allocate(2)?;
            quad.set_position(
                x - self.dimensions.pixel_width as f32 / 2.,
                y - self.dimensions.pixel_height as f32 / 2.,
                x + w - self.dimensions.pixel_width as f32 / 2.,
                y + h - self.dimensions.pixel_height as f32 / 2.,
            );
            quad.set_hsv(None);
            quad.set_fg_color(LinearRgba::with_components(1., 1., 1., 1.));
            quad.set_texture(sprite.texture_coords());
            quad.set_has_color(true);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// A tile file: a 25 x 20 canvas at 4 pixels for each unit.
    fn tile(colour: [u8; 3]) -> Arc<RgbaImage> {
        let mut image = RgbaImage::new(100, 80);
        for y in 6..70 {
            for x in 8..92 {
                image.put_pixel(x, y, Rgba([colour[0], colour[1], colour[2], 255]));
            }
        }
        Arc::new(image)
    }

    #[test]
    fn marker_accepts_only_safe_names() {
        let osc = |text: &str| {
            OperatingSystemCommand::Unspecified(text.split(';').map(|part| part.as_bytes().to_vec()).collect())
        };
        assert_eq!(marker_from_osc(&osc("7778;icon;nvim,shell;3")), Some(("wezterm-icon:nvim,shell:3".into(), 3)));
        assert_eq!(marker_from_osc(&osc("7778;icon;shell")), Some(("wezterm-icon:shell:2".into(), 2)));
        assert_eq!(marker_from_osc(&osc("7778;icon;a,b,c,d;99")), Some(("wezterm-icon:a,b,c:8".into(), 8)));
        assert_eq!(marker_from_osc(&osc("7778;icon;../etc/passwd;1")), None);
        assert_eq!(marker_from_osc(&osc("7778;icon;nvim,../x;1")), None);
        assert_eq!(marker_from_osc(&osc("7777;icon;shell;1")), None);
        assert_eq!(parse_marker("wezterm-icon:nvim,shell:3"), Some((vec!["nvim", "shell"], 3, false)));
        assert_eq!(marker_from_osc(&osc("7778;icon;claude,codex;4;button")), Some(("wezterm-icon:claude,codex:4:button".into(), 4)));
        assert_eq!(parse_marker("wezterm-icon:claude,codex:4:button"), Some((vec!["claude", "codex"], 4, true)));
        assert_eq!(parse_marker("https://example.com"), None);
    }

    #[test]
    fn status_marker_accepts_known_kinds() {
        let osc = |text: &str| {
            OperatingSystemCommand::Unspecified(text.split(';').map(|part| part.as_bytes().to_vec()).collect())
        };
        assert_eq!(marker_from_osc(&osc("7778;status;working;2")), Some(("wezterm-status:working:2".into(), 2)));
        assert_eq!(marker_from_osc(&osc("7778;status;question")), Some(("wezterm-status:question:2".into(), 2)));
        assert_eq!(marker_from_osc(&osc("7778;status;../x;2")), None);
        assert_eq!(parse_status_marker("wezterm-status:done:2"), Some(("done", 2)));
        assert_eq!(parse_status_marker("wezterm-status:bogus:2"), None);
    }

    #[test]
    fn status_images_draw_their_shapes() {
        // The dots move: two frames differ. A bubble has its colour in the
        // middle and its white mark.
        let a = status_image("working", 2, 40, 20);
        let b = status_image("working", 6, 40, 20);
        assert_ne!(a.as_raw(), b.as_raw());
        assert!(a.pixels().any(|p| p.0[3] > 200));
        if let Ok(dir) = std::env::var("STATUS_DUMP") {
            for kind in STATUS_KINDS {
                for frame in [0, 3, 6] {
                    status_image(kind, frame, 40, 20).save(format!("{dir}/{kind}-{frame}.png")).unwrap();
                    status_image(kind, frame, 80, 40).save(format!("{dir}/{kind}-{frame}-big.png")).unwrap();
                }
            }
        }
        for kind in ["question", "approval", "signin", "done", "error"] {
            let image = status_image(kind, 0, 40, 20);
            let opaque = image.pixels().filter(|p| p.0[3] > 200).count();
            let white = image.pixels().filter(|p| p.0[3] > 200 && p.0[0] > 240 && p.0[1] > 230).count();
            assert!(opaque > 100, "{} {}", kind, opaque);
            assert!(white > 4, "{} {}", kind, white);
        }
        assert_eq!(status_image("done", 0, 40, 20), status_image("done", 7, 40, 20));
    }

    #[test]
    fn one_tile_is_as_high_as_the_row() {
        let image = compose(&[tile([200, 0, 0])], 40, 22);
        assert_eq!(image.height(), 22);
        assert!(image.width() <= 40);
        // The card fills most of the canvas.
        assert_eq!(image.get_pixel(image.width() / 2, 11).0[3], 255);
    }

    #[test]
    fn a_stack_shows_the_front_tile_and_the_tiles_behind() {
        let stack = compose(&[tile([200, 0, 0]), tile([0, 200, 0]), tile([0, 0, 200])], 60, 22);
        assert!(stack.width() <= 60 && stack.height() <= 22);
        let middle = stack.height() / 2;
        // The front tile at the left, the last one peeks out at the right.
        // (The resize filter can ring a little at an edge: compare the colour.)
        let colour = |x: u32| {
            let [r, g, b, _] = stack.get_pixel(x, middle).0;
            if r > 150 && g < 60 && b < 60 { 'r' } else if b > 150 && r < 60 && g < 60 { 'b' } else { '?' }
        };
        assert_eq!(colour(4), 'r');
        let right = (0..stack.width()).rev().find(|x| stack.get_pixel(*x, middle).0[3] == 255).unwrap();
        assert_eq!(colour(right - 1), 'b');
        // A narrow box makes the whole stack smaller, never cut.
        let narrow = compose(&[tile([200, 0, 0]), tile([0, 200, 0]), tile([0, 0, 200])], 20, 22);
        assert!(narrow.width() <= 20 && narrow.height() < 22);
    }
}
