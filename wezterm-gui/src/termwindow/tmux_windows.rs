//! tmux -CC windows drawn by WezTerm itself: the bottom tab strip, a preview
//! of a window while the pointer hovers its tab, and the window overview
//! (Exposé: every window as a live thumbnail, 4 or 9 at a time, filter by
//! name, scroll for more), opened from the user's prefix-a menu.
//!
//! tmux only provides the window list (mux/src/tmux_strip.rs); the thumbnails
//! are the mirror's native panes rendered with a smaller font.
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use crate::utilsprites::RenderMetrics;
use crate::TermWindow;
use mux::renderable::RenderableDimensions;
use mux::tab::{Tab, TabId};
use mux::tmux::TmuxDomain;
use mux::tmux_strip::StripWindow;
use mux::Mux;
use std::rc::Rc;
use std::sync::Arc;
use wezterm_font::LoadedFont;
use wezterm_term::color::ColorAttribute;
use wezterm_term::{CellAttributes, Line};
use window::{KeyCode, Modifiers as KeyModifiers};
use window::color::LinearRgba;
use window::{MouseEvent, MouseEventKind as WMEK, MousePress, WindowOps};

type RectF = euclid::Rect<f32, window::PixelUnit>;

/// Hover preview of the window under the pointer in the tab strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripHover {
    pub tab_id: TabId,
    /// Left pixel of the hovered tab label.
    pub x: f32,
}

/// A pane separator being dragged. The tab's local copy of the tmux layout
/// moves on every motion (panes and separators follow the pointer at once);
/// tmux gets the final size once, on release, then the panes are recaptured.
#[derive(Debug, Clone, Copy)]
pub struct SplitDrag {
    pub tab_id: TabId,
    pub pane: mux::pane::PaneId,
    /// Vertical separator (resizes the width), else a border row (height).
    pub vertical: bool,
    /// The pane's left (vertical) or top edge, in cells.
    pub origin: usize,
    /// Extent of the line drawn while dragging: (start, end) cells along it.
    pub span: (usize, usize),
    /// Separator position under the pointer, in cells.
    pub position: usize,
    pub limit: usize,
    pub sent: usize,
    pub sent_at: std::time::Instant,
    /// Pointer row (vertical separator) or column (border row) along it.
    pub along: usize,
    /// Separator position of the tab's local layout.
    pub applied: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WindowOverview {
    pub filter: String,
    /// `/` was pressed: letters go to the filter (Enter or Escape ends it).
    pub filtering: bool,
    pub selected: usize,
    /// First visible row of cards (scrolling).
    pub first_row: usize,
}

struct Card {
    rect: RectF,
    window: StripWindow,
    tab_id: Option<TabId>,
    index: usize,
}

pub(crate) fn rect(x: f32, y: f32, w: f32, h: f32) -> RectF {
    euclid::rect(x, y, w.max(0.), h.max(0.))
}

pub(crate) fn contains(r: &RectF, x: f32, y: f32) -> bool {
    x >= r.min_x() && x < r.max_x() && y >= r.min_y() && y < r.max_y()
}

pub(crate) fn text_line(text: &str, fg: ColorAttribute, bg: ColorAttribute, bold: bool) -> Line {
    let mut attrs = CellAttributes::default();
    attrs.set_foreground(fg);
    attrs.set_background(bg);
    if bold {
        attrs.set_intensity(wezterm_term::Intensity::Bold);
    }
    Line::from_text(text, &attrs, 0, None)
}

fn matches(filter: &str, w: &StripWindow) -> bool {
    let filter = filter.to_lowercase();
    filter.is_empty()
        || w.name.to_lowercase().contains(&filter)
        || w.index.to_string() == filter
}

impl TermWindow {
    pub(crate) fn tmux_domain_for_window(&self) -> Option<(Arc<Tab>, Arc<dyn mux::domain::Domain>)> {
        let mux = Mux::get();
        let tab = mux.get_active_tab_for_window(self.mux_window_id)?;
        let pane = tab.get_active_pane()?;
        let domain = mux.get_domain(pane.domain_id())?;
        domain.downcast_ref::<TmuxDomain>()?;
        Some((tab, domain))
    }

    fn overview_domain_for_window(&self) -> Option<(Arc<Tab>, Arc<dyn mux::domain::Domain>)> {
        let mux=Mux::get();let tab=mux.get_active_tab_for_window(self.mux_window_id)?;
        let domain=mux.get_domain(tab.get_active_pane()?.domain_id())?;
        if domain.is::<TmuxDomain>() || domain.is::<mux::rex::RexDomain>() {Some((tab,domain))}else{None}
    }

    /// Row of the tab strip: tmux's border row under its window. It is the
    /// bottom row, unless tmux's window is shorter than this tab (another,
    /// smaller client); the strip then stays on tmux's row, not mid-pane.
    fn tmux_strip_row(tmux: &TmuxDomain, tab: &Arc<Tab>) -> usize {
        let last = tab.get_size().rows.saturating_sub(1);
        tmux.tiled_panes(tab.tab_id())
            .iter()
            .map(|(_, _, top, _, height)| top + height)
            .max()
            .map_or(last, |row| row.min(last))
    }

    fn content_origin(&self) -> (f32, f32) {
        let (padding_left, padding_top) = self.padding_left_top();
        let border = self.get_os_border();
        let tab_bar = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        };
        (
            padding_left + border.left.get() as f32,
            tab_bar + padding_top + border.top.get() as f32,
        )
    }

    pub(crate) fn render_line_at(
        &self,
        line: &Line,
        x: f32,
        y: f32,
        cols: usize,
        font: Option<&Rc<LoadedFont>>,
        metrics: RenderMetrics,
        palette: &wezterm_term::color::ColorPalette,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let default_bg = palette.resolve_bg(ColorAttribute::Default).to_linear();
        self.render_screen_line(
            RenderScreenLineParams {
                top_pixel_y: y,
                left_pixel_x: x,
                pixel_width: cols as f32 * metrics.cell_size.width as f32,
                stable_line_idx: None,
                line,
                selection: 0..0,
                cursor: &Default::default(),
                palette,
                dims: &RenderableDimensions {
                    cols,
                    physical_top: 0,
                    scrollback_rows: 0,
                    scrollback_top: 0,
                    viewport_rows: 1,
                    dpi: self.terminal_size.dpi,
                    pixel_height: metrics.cell_size.height as usize,
                    pixel_width: self.terminal_size.pixel_width,
                    reverse_video: false,
                },
                config: &self.config,
                cursor_border_color: LinearRgba::default(),
                foreground: palette.foreground.to_linear(),
                pane: None,
                is_active: true,
                selection_fg: LinearRgba::default(),
                selection_bg: LinearRgba::default(),
                cursor_fg: LinearRgba::default(),
                cursor_bg: LinearRgba::default(),
                cursor_is_default_color: true,
                white_space,
                filled_box,
                window_is_transparent: false,
                default_bg,
                style: None,
                font: font.cloned(),
                use_pixel_positioning: self.config.experimental_pixel_positioning,
                render_metrics: metrics,
                shape_key: None,
                password_input: false,
            },
            layers,
        )?;
        Ok(())
    }

    /// The tab strip, drawn over the bottom row of a tmux window, around the
    /// panes in copy mode (their status stays in tmux's border row). The
    /// spans come from the native pane positions, so a separator drag moves
    /// the strip in the same frame. Returns whether the strip is shown.
    pub fn paint_tmux_window_strip(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<bool> {
        let Some((tab, domain)) = self.tmux_domain_for_window() else {
            return Ok(false);
        };
        let tmux = domain.downcast_ref::<TmuxDomain>().unwrap();
        let size = tab.get_size();
        let strip_row = Self::tmux_strip_row(tmux, &tab);
        let mode_panes = tmux.mode_panes(tab.tab_id());
        let hidden: Vec<(usize, usize)> = if mode_panes.is_empty() {
            vec![]
        } else {
            tab.iter_panes()
                .iter()
                .filter(|p| mode_panes.contains(&p.pane.pane_id()) && p.top + p.height > strip_row)
                .map(|p| (p.left, p.left + p.width))
                .collect()
        };
        let Some(segments) = tmux.window_strip(tab.tab_id(), size.cols, &hidden) else {
            return Ok(false);
        };
        let (x, top) = self.content_origin();
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let y = top + strip_row as f32 * ch;
        let palette = self.palette().clone();
        let metrics = self.render_metrics;
        for (left, line) in segments {
            let sx = x + left as f32 * cw;
            let cols = line.len();
            // Opaque: tmux's own border row (and its junctions) stay hidden.
            self.filled_rectangle(
                layers,
                0,
                rect(sx, y, cols as f32 * cw, ch),
                palette.resolve_bg(ColorAttribute::Default).to_linear(),
            )?;
            self.render_line_at(&line, sx, y, cols, None, metrics, &palette, layers)?;
        }
        Ok(true)
    }

    /// Vertical separators of a tmux window, drawn from the pane geometry so
    /// they change in the same frame as the layout (splits, resizing). A
    /// separator continues through the border/status row below its pane
    /// with `│` (no junction glyphs).
    pub fn paint_tmux_separators(
        &mut self,
        tab: &Arc<Tab>,
        strip_shown: bool,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let size = tab.get_size();
        let (ox, oy) = self.content_origin();
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let palette = self.palette().clone();
        let bg = palette.resolve_bg(ColorAttribute::Default).to_linear();
        let line = palette.resolve_fg(ColorAttribute::PaletteIndex(250)).to_linear();
        let thickness = (cw / 10.).max(1.).round();
        let (tmux_panes, _labels, strip_row) = match self.tmux_domain_for_window() {
            Some((_, d)) => {
                let tmux = d.downcast_ref::<TmuxDomain>().unwrap();
                (
                    tmux.tiled_panes(tab.tab_id()),
                    tmux.strip_regions(tab.tab_id()),
                    Self::tmux_strip_row(tmux, tab),
                )
            }
            None => (vec![], vec![], size.rows.saturating_sub(1)),
        };
        let all = tab.iter_panes();
        // A native pane includes its border row when it is taller than tmux's.
        let border_row_of = |pos: &mux::tab::PositionedPane| {
            let content = tmux_panes
                .iter()
                .find(|p| p.0 == pos.pane.pane_id())
                .map(|p| p.4)
                .unwrap_or(pos.height);
            (pos.height > content).then(|| pos.top + pos.height - 1)
        };
        for pos in all.iter() {
            let col = pos.left + pos.width;
            if col >= size.cols || pos.height == 0 {
                continue;
            }
            // tmux's pane height excludes its border row; the native pane
            // includes it (pane-border-status) when it is one row taller.
            let border_row = border_row_of(pos);
            let content_end = border_row.unwrap_or(pos.top + pos.height + 1).min(size.rows);
            let x = ox + col as f32 * cw;
            let line_x = x + ((cw - thickness) / 2.).floor();
            // Lines never touch: a line that ends stops half a cell short of
            // the line it meets, and the line that goes on is unbroken.
            if content_end > pos.top {
                let y = oy + pos.top as f32 * ch;
                let h = (content_end - pos.top) as f32 * ch;
                self.filled_rectangle(layers, 0, rect(x, y, cw, h), bg)?;
                self.filled_rectangle(layers, 2, rect(line_x, y, thickness, h), line)?;
            }
            let Some(row) = border_row else { continue };
            // The tab strip's line runs on through the separator column.
            if row >= size.rows || (strip_shown && row == strip_row) {
                continue;
            }
            let y = oy + row as f32 * ch;
            // The separator goes on below this border row when a pane beside
            // it does: it crosses the row, and the border lines stop short.
            let continues = row + 1 < size.rows
                && all.iter().any(|p| {
                    (p.left + p.width == col || p.left == col + 1) && p.top <= row + 1 && row + 1 < p.top + p.height
                });
            self.filled_rectangle(layers, 0, rect(x, y, cw, ch), bg)?;
            if continues {
                self.filled_rectangle(layers, 2, rect(line_x, y, thickness, ch), line)?;
                continue;
            }
            // The separator ends here: the border line goes on through this
            // cell when the pane on the right has its border row here too.
            let right_border = all.iter().any(|p| p.left == col + 1 && border_row_of(p) == Some(row));
            if right_border {
                let mid = y + ((ch - thickness) / 2.).floor();
                self.filled_rectangle(layers, 2, rect(x, mid, cw, thickness), line)?;
            }
        }
        Ok(())
    }

    /// A tab's panes scaled into `area`: centered to fit, or (fill) scaled to
    /// the area's width from its top edge, cut at its bottom edge.
    fn paint_tab_thumbnail(
        &mut self,
        tab: &Arc<Tab>,
        area: RectF,
        fill: bool,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let size = tab.get_size();
        let (cols, rows) = (size.cols.max(1), size.rows.max(1));
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let fit_w = area.width() / (cols as f32 * cw);
        let fit_h = area.height() / (rows as f32 * ch);
        let scale = if fill { fit_w } else { fit_w.min(fit_h) };
        let mut font_size = self.config.font_size * self.fonts.get_font_scale() * scale as f64;
        let mut font = self.fonts.preview_font(font_size.max(1.))?;
        let mut metrics = RenderMetrics::with_font_metrics(&font.metrics());
        // Small cells round to whole pixels: correct the size once so the
        // thumbnail uses the area's width.
        let wanted_cw = if fill { area.width() / cols as f32 } else { (area.width() / cols as f32).min(area.height() / rows as f32 * cw / ch) };
        let got_cw = metrics.cell_size.width as f32;
        if got_cw > 0. && (got_cw - wanted_cw).abs() / wanted_cw > 0.05 {
            font_size *= (wanted_cw / got_cw) as f64;
            let refined = self.fonts.preview_font(font_size.max(1.))?;
            let refined_metrics = RenderMetrics::with_font_metrics(&refined.metrics());
            if refined_metrics.cell_size.width as f32 * cols as f32 <= area.width() + 1. {
                font = refined;
                metrics = refined_metrics;
            }
        }
        // Whole-pixel cells can still add up to more than the area: shrink
        // until the full width fits, so no text crosses the card's edge.
        for _ in 0..4 {
            let width = metrics.cell_size.width as f32 * cols as f32;
            if width <= area.width() + 0.5 || font_size <= 1. {
                break;
            }
            font_size *= (area.width() / width * 0.98) as f64;
            font = self.fonts.preview_font(font_size.max(1.))?;
            metrics = RenderMetrics::with_font_metrics(&font.metrics());
        }
        let scw = metrics.cell_size.width as f32;
        let sch = metrics.cell_size.height as f32;
        let (w, h) = (cols as f32 * scw, rows as f32 * sch);
        let (ox, oy) = if fill {
            (area.min_x(), area.min_y())
        } else {
            (
                area.min_x() + ((area.width() - w) / 2.).max(0.),
                area.min_y() + ((area.height() - h) / 2.).max(0.),
            )
        };
        let palette = self.palette().clone();
        // Whole-pixel cells rarely add up to the area's width: in a card the
        // terminal background fills the area, the text stays left-aligned.
        let background = if fill { area } else { rect(ox, oy, w.min(area.width()), h.min(area.height())) };
        self.filled_rectangle(
            layers,
            0,
            background,
            palette.resolve_bg(ColorAttribute::Default).to_linear(),
        )?;
        let separator = palette.resolve_fg(ColorAttribute::PaletteIndex(250)).to_linear();
        for pos in tab.iter_panes() {
            pos.pane.prepare_for_display();
            let dims = pos.pane.get_dimensions();
            let height = pos.height.min(rows.saturating_sub(pos.top));
            let (_, lines) = pos
                .pane
                .get_lines(dims.physical_top..dims.physical_top + height as isize);
            let pane_palette = pos.pane.palette();
            for (i, line) in lines.iter().enumerate() {
                let y = oy + (pos.top + i) as f32 * sch;
                if y + sch > area.max_y() + 0.5 {
                    break;
                }
                // Cut at the area's right edge, as lines are cut at its bottom.
                let x = ox + pos.left as f32 * scw;
                let fits = ((area.max_x() + 0.5 - x) / scw).floor().max(0.) as usize;
                let mut line = line.clone();
                line.resize(pos.width.min(cols.saturating_sub(pos.left)).min(fits), 0);
                if line.len() == 0 {
                    continue;
                }
                self.render_line_at(
                    &line,
                    x,
                    y,
                    line.len(),
                    Some(&font),
                    metrics,
                    &pane_palette,
                    layers,
                )?;
            }
            // Separators come from the layout, as in the window itself.
            let col = pos.left + pos.width;
            if col < cols && ox + (col as f32 + 0.5) * scw < area.max_x() {
                let top = oy + pos.top as f32 * sch;
                let bottom = (oy + (pos.top + pos.height + 1) as f32 * sch).min(area.max_y());
                if bottom > top {
                    self.filled_rectangle(
                        layers,
                        2,
                        rect(ox + (col as f32 + 0.5) * scw, top, 1., bottom - top),
                        separator,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// A rectangle with rounded corners (radius in pixels), as stacked rows.
    pub(crate) fn fill_rounded(
        &self,
        r: RectF,
        radius: f32,
        colour: LinearRgba,
        layer: usize,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let radius = radius.min(r.width() / 2.).min(r.height() / 2.).floor().max(0.);
        self.filled_rectangle(
            layers,
            layer,
            rect(r.min_x(), r.min_y() + radius, r.width(), r.height() - 2. * radius),
            colour,
        )?;
        for i in 0..radius as usize {
            let dy = radius - i as f32 - 0.5;
            let inset = (radius - (radius * radius - dy * dy).max(0.).sqrt()).round();
            let width = r.width() - 2. * inset;
            self.filled_rectangle(layers, layer, rect(r.min_x() + inset, r.min_y() + i as f32, width, 1.), colour)?;
            self.filled_rectangle(layers, layer, rect(r.min_x() + inset, r.max_y() - 1. - i as f32, width, 1.), colour)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ separators

    fn pointer_cell(&self, event: &MouseEvent) -> (isize, isize) {
        let (ox, oy) = self.content_origin();
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        (
            ((event.coords.x as f32 - ox) / cw).floor() as isize,
            ((event.coords.y as f32 - oy) / ch).floor() as isize,
        )
    }

    /// Drags tmux pane separators locally; returns true when it handled the event.
    pub fn split_drag_mouse_event(&mut self, event: &MouseEvent, context: &dyn WindowOps) -> bool {
        if let Some(mut drag) = self.split_drag {
            let (col, row) = self.pointer_cell(event);
            let wanted = if drag.vertical { col } else { row };
            let position = wanted.clamp(drag.origin as isize + 2, drag.limit as isize) as usize;
            let tmux = Mux::get()
                .get_tab(drag.tab_id)
                .and_then(|t| t.get_active_pane())
                .and_then(|p| Mux::get().get_domain(p.domain_id()));
            let send = |drag: &SplitDrag| {
                if let Some(domain) = tmux.as_ref() {
                    if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                        let size = drag.position - drag.origin;
                        if drag.vertical {
                            tmux.resize_pane_to(drag.pane, Some(size), None);
                        } else {
                            tmux.resize_pane_to(drag.pane, None, Some(size));
                        }
                    }
                }
            };
            let tab = Mux::get().get_tab(drag.tab_id);
            match event.kind {
                WMEK::Move => {
                    drag.position = position;
                    if let Some(tab) = &tab {
                        if drag.position != drag.applied
                            && tab.drag_remote_separator(drag.vertical, drag.applied, drag.position, drag.along)
                        {
                            drag.applied = drag.position;
                        }
                    }
                    self.split_drag = Some(drag);
                }
                WMEK::Release(MousePress::Left) => {
                    drag.position = position;
                    if let Some(tab) = &tab {
                        if drag.position != drag.applied
                            && tab.drag_remote_separator(drag.vertical, drag.applied, drag.position, drag.along)
                        {
                            drag.applied = drag.position;
                        }
                    }
                    // tmux gets where the separator really is.
                    drag.position = drag.applied;
                    if drag.position != drag.sent {
                        send(&drag);
                        // The local resize reflowed the panes itself: take
                        // tmux's content once it has the same sizes.
                        if let Some(domain) = tmux.as_ref() {
                            if let Some(tmux) = domain.downcast_ref::<TmuxDomain>() {
                                tmux.recapture_tab(drag.tab_id);
                            }
                        }
                    }
                    self.split_drag = None;
                    self.current_mouse_buttons.retain(|p| *p != MousePress::Left);
                }
                _ => {}
            }
            context.invalidate();
            return true;
        }
        if !matches!(event.kind, WMEK::Press(MousePress::Left)) {
            return false;
        }
        let Some((tab, domain)) = self.tmux_domain_for_window() else {
            return false;
        };
        let tmux = domain.downcast_ref::<TmuxDomain>().unwrap();
        let size = tab.get_size();
        let (col, row) = self.pointer_cell(event);
        if col < 0 || row < 0 {
            return false;
        }
        let (col, row) = (col as usize, row as usize);
        let panes = tmux.tiled_panes(tab.tab_id());
        // A vertical separator: the column right of a pane, along its rows.
        let vertical = panes.iter().find(|(_, l, t, w, h)| col == l + w && row >= *t && row <= t + h);
        // A border row with a pane below it (the bottom row is the tab strip).
        let horizontal = panes.iter().find(|(_, l, t, w, h)| {
            row == t + h
                && col >= *l
                && col < l + w
                && panes.iter().any(|(_, l2, t2, w2, _)| *t2 == row + 1 && *l2 < l + w && l2 + w2 > *l)
        });
        let drag = match (vertical, horizontal) {
            (Some((pane, l, t, w, h)), _) => SplitDrag {
                tab_id: tab.tab_id(),
                pane: *pane,
                vertical: true,
                origin: *l,
                span: (*t, t + h + 1),
                position: l + w,
                limit: size.cols.saturating_sub(3),
                sent: l + w,
                sent_at: std::time::Instant::now(),
                along: 0,
                applied: l + w,
            },
            (None, Some((pane, l, t, w, h))) => SplitDrag {
                tab_id: tab.tab_id(),
                pane: *pane,
                vertical: false,
                origin: *t,
                span: (*l, l + w),
                position: t + h,
                limit: size.rows.saturating_sub(4),
                sent: t + h,
                sent_at: std::time::Instant::now(),
                along: 0,
                applied: t + h,
            },
            _ => return false,
        };
        let mut drag = drag;
        drag.along = if drag.vertical { row } else { col };
        log::debug!("split drag {:?}", drag);
        self.split_drag = Some(drag);
        self.current_mouse_buttons.retain(|p| *p != MousePress::Left);
        self.current_mouse_buttons.push(MousePress::Left);
        true
    }

    /// The panes themselves follow a separator drag; nothing extra to draw.
    pub fn paint_split_drag(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    // ---------------------------------------------------------------- hover

    /// Tracks the tab under the pointer; returns true when the preview changed.
    pub fn update_strip_hover(&mut self, event: &MouseEvent) -> bool {
        let hover = self.strip_hover_at(event);
        if hover != self.strip_hover {
            self.strip_hover = hover;
            return true;
        }
        false
    }

    fn strip_hover_at(&self, event: &MouseEvent) -> Option<StripHover> {
        if !matches!(event.kind, WMEK::Move) || self.window_overview.is_some() {
            return None;
        }
        let (tab, domain) = self.tmux_domain_for_window()?;
        let tmux = domain.downcast_ref::<TmuxDomain>()?;
        let size = tab.get_size();
        let (ox, oy) = self.content_origin();
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let row = ((event.coords.y as f32 - oy) / ch).floor();
        if row != Self::tmux_strip_row(tmux, &tab) as f32 {
            return None;
        }
        let col = ((event.coords.x as f32 - ox) / cw).floor();
        if col < 0. {
            return None;
        }
        let col = col as usize;
        let (start, _, window) = tmux
            .strip_regions(tab.tab_id())
            .into_iter()
            .find(|(start, end, _)| col >= *start && col < *end)?;
        let (w, tab_id) = tmux.strip_windows().into_iter().find(|(w, _)| w.id == window)?;
        if w.active {
            return None;
        }
        Some(StripHover {
            tab_id: tab_id?,
            x: ox + start as f32 * cw,
        })
    }

    /// Preview card above the strip for the hovered window.
    pub fn paint_strip_hover(&mut self) -> anyhow::Result<()> {
        let Some(hover) = self.strip_hover else {
            return Ok(());
        };
        let Some(tab) = Mux::get().get_tab(hover.tab_id) else {
            return Ok(());
        };
        let Some((current, domain)) = self.overview_domain_for_window() else {
            return Ok(());
        };
        let size = current.get_size();
        let (ox, oy) = self.content_origin();
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let strip_row = domain
            .downcast_ref::<TmuxDomain>()
            .map_or(size.rows.saturating_sub(1), |tmux| Self::tmux_strip_row(tmux, &current));
        let strip_y = oy + strip_row as f32 * ch;
        let window_w = size.cols as f32 * cw;
        let card_w = (window_w * 0.42).max(cw * 30.);
        let aspect = (size.rows as f32 * ch) / (size.cols as f32 * cw).max(1.);
        let card_h = (card_w * aspect).min(strip_y - oy - ch).max(ch * 4.);
        let x = hover.x.min(ox + window_w - card_w).max(ox);
        let y = strip_y - card_h - ch * 0.4;
        let layer = self.render_state.as_ref().unwrap().layer_for_zindex(4)?;
        let mut layers = layer.quad_allocator();
        let palette = self.palette().clone();
        let bg = palette.resolve_bg(ColorAttribute::Default).to_linear();
        let fg = palette.resolve_fg(ColorAttribute::Default).to_linear();
        let mix = |t: f32| LinearRgba::with_components(bg.0 + (fg.0 - bg.0) * t, bg.1 + (fg.1 - bg.1) * t, bg.2 + (fg.2 - bg.2) * t, 1.);
        let radius = 0.;
        let pad = (cw * 0.5).round();
        self.fill_rounded(rect(x - pad - 2., y - pad - 2., card_w + 2. * pad + 4., card_h + 2. * pad + 4.), radius + 2., mix(0.3), 0, &mut layers)?;
        self.fill_rounded(rect(x - pad, y - pad, card_w + 2. * pad, card_h + 2. * pad), radius, mix(0.13), 0, &mut layers)?;
        self.paint_tab_thumbnail(&tab, rect(x, y, card_w, card_h), true, &mut layers)
    }

    // ------------------------------------------------------------- overview

    pub fn open_window_overview(&mut self) {
        if self.overview_domain_for_window().is_none() {
            return;
        }
        let selected = self.overview_windows("").iter().position(|(w, _)| w.active).unwrap_or(0);
        self.window_overview = Some(WindowOverview {
            filter: String::new(),
            filtering: false,
            selected,
            first_row: 0,
        });
        self.strip_hover = None;
        self.overview_scroll_to_selected();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn overview_windows(&self, filter: &str) -> Vec<(StripWindow, Option<TabId>)> {
        let Some((_,domain))=self.overview_domain_for_window() else{return vec![]};
        if let Some(tmux)=domain.downcast_ref::<TmuxDomain>() {return tmux.strip_windows().into_iter().filter(|(w,_)|matches(filter,w)).collect()}
        let mux=Mux::get();let Some(window)=mux.get_window(self.mux_window_id) else{return vec![]};
        window.iter_tabs().enumerate().filter_map(|(index,tab)| {
            let w=StripWindow{id:tab.tab_id() as u64,index:index as u64+1,name:tab.get_title(),active:index==window.get_active_tab_idx(),zoomed:false,synchronized:false,bottom_border:true,prefix:"C-Space".into(),display_time:1000,message_style:String::new(),session:domain.domain_name().into()};
            matches(filter,&w).then_some((w,Some(tab.tab_id())))
        }).collect()
    }

    /// (columns per row, rows per page) for `count` windows: 2x2 up to four.
    fn overview_grid(count: usize) -> (usize, usize) {
        if count <= 4 {
            (2, 2)
        } else {
            (3, 3)
        }
    }

    fn overview_scroll_to_selected(&mut self) {
        let count = self
            .window_overview
            .as_ref()
            .map(|o| self.overview_windows(&o.filter).len())
            .unwrap_or(0);
        let (per_row, rows) = Self::overview_grid(count);
        if let Some(o) = self.window_overview.as_mut() {
            o.selected = o.selected.min(count.saturating_sub(1));
            let row = o.selected / per_row;
            if row < o.first_row {
                o.first_row = row;
            } else if row >= o.first_row + rows {
                o.first_row = row + 1 - rows;
            }
        }
    }

    /// Cards of the visible page. No header: the grid uses the whole window.
    fn overview_layout(&self) -> (RectF, Vec<Card>, usize) {
        let o = self.window_overview.clone().unwrap_or_default();
        let windows = self.overview_windows(&o.filter);
        let (per_row, rows) = Self::overview_grid(windows.len());
        let w = self.dimensions.pixel_width as f32;
        let h = self.dimensions.pixel_height as f32;
        let margin = (w.min(h) * 0.035).max(10.);
        let grid = rect(margin, margin, w - 2. * margin, h - 2. * margin);
        let gap = (w.min(h) * 0.02).max(8.);
        let card_w = (grid.width() - gap * (per_row - 1) as f32) / per_row as f32;
        let card_h = (grid.height() - gap * (rows - 1) as f32) / rows as f32;
        let cards = windows
            .into_iter()
            .enumerate()
            .skip(o.first_row * per_row)
            .take(per_row * rows)
            .map(|(index, (window, tab_id))| {
                let slot = index - o.first_row * per_row;
                let (col, row) = (slot % per_row, slot / per_row);
                Card {
                    rect: rect(
                        grid.min_x() + col as f32 * (card_w + gap),
                        grid.min_y() + row as f32 * (card_h + gap),
                        card_w,
                        card_h,
                    ),
                    window,
                    tab_id,
                    index,
                }
            })
            .collect();
        (grid, cards, per_row)
    }

    fn overview_choose(&mut self, index: usize) {
        let filter = match &self.window_overview {
            Some(o) => o.filter.clone(),
            None => return,
        };
        let windows = self.overview_windows(&filter);
        if let (Some((w, tab)), Some((_, domain))) = (windows.get(index), self.overview_domain_for_window()) {
            if let Some(tmux)=domain.downcast_ref::<TmuxDomain>() {tmux.select_window(w.id);}
            else if let Some(tab_id)=tab {
                let mux=Mux::get();
                let idx=mux.get_window(self.mux_window_id).and_then(|w|w.get_tab_idx_for_id(*tab_id));
                if let Some(idx)=idx {self.activate_tab(idx as isize).ok();}
            }
        }
        self.close_window_overview();
    }

    pub fn close_window_overview(&mut self) {
        self.window_overview = None;
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Keys while the overview is open: type to filter, arrows/Tab to move,
    /// PageUp/PageDown to scroll, Enter to switch, Escape to close.
    pub fn window_overview_key(&mut self, key: &KeyCode, mods: KeyModifiers) -> bool {
        let Some(o) = self.window_overview.clone() else {
            return false;
        };
        let count = self.overview_windows(&o.filter).len();
        let (per_row, rows) = Self::overview_grid(count);
        let mut o = o;
        let last = count.saturating_sub(1);
        match key {
            // While typing a filter, Escape and Enter end the typing.
            KeyCode::Char('\u{1b}') if o.filtering => o.filtering = false,
            KeyCode::Char('\r') | KeyCode::Char('\n') if o.filtering => o.filtering = false,
            KeyCode::Char('\u{1b}') => {
                self.close_window_overview();
                return true;
            }
            KeyCode::Char('\r') | KeyCode::Char('\n') => {
                self.overview_choose(o.selected);
                return true;
            }
            KeyCode::Char('\u{8}') | KeyCode::Char('\u{7f}') => {
                o.filter.pop();
                o.selected = 0;
                o.first_row = 0;
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CTRL) => {
                o.filter.clear();
                o.selected = 0;
                o.first_row = 0;
            }
            KeyCode::Char(c) if o.filtering && !mods.contains(KeyModifiers::CTRL) && !c.is_control() => {
                o.filter.push(*c);
                o.selected = 0;
                o.first_row = 0;
            }
            KeyCode::Char('/') => o.filtering = true,
            KeyCode::LeftArrow | KeyCode::Char('h') => o.selected = o.selected.saturating_sub(1),
            KeyCode::RightArrow | KeyCode::Char('l') | KeyCode::Char('\t') => o.selected = (o.selected + 1).min(last),
            KeyCode::UpArrow | KeyCode::Char('k') => o.selected = o.selected.saturating_sub(per_row),
            KeyCode::DownArrow | KeyCode::Char('j') => o.selected = (o.selected + per_row).min(last),
            KeyCode::PageUp => o.selected = o.selected.saturating_sub(per_row * rows),
            KeyCode::PageDown => o.selected = (o.selected + per_row * rows).min(last),
            KeyCode::Char('q') => {
                self.close_window_overview();
                return true;
            }
            _ => return true,
        }
        self.window_overview = Some(o);
        self.overview_scroll_to_selected();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        true
    }

    /// Mouse while the overview is open: click a window to switch, wheel to
    /// scroll, click elsewhere to close.
    pub fn window_overview_mouse(&mut self, event: &MouseEvent, context: &dyn WindowOps) -> bool {
        let Some(mut o) = self.window_overview.clone() else {
            return false;
        };
        // Typing hid the pointer; the overview handles the mouse itself.
        context.set_cursor(Some(window::CursorIcon::Default));
        let (_, cards, per_row) = self.overview_layout();
        let (x, y) = (event.coords.x as f32, event.coords.y as f32);
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                match cards.iter().find(|c| contains(&c.rect, x, y)) {
                    Some(card) => self.overview_choose(card.index),
                    None => self.close_window_overview(),
                }
                return true;
            }
            WMEK::VertWheel(n) => {
                let count = self.overview_windows(&o.filter).len();
                let (_, rows) = Self::overview_grid(count);
                let total_rows = (count + per_row - 1) / per_row;
                if n > 0 {
                    o.first_row = o.first_row.saturating_sub(1);
                } else if o.first_row + rows < total_rows {
                    o.first_row += 1;
                }
                let first = o.first_row * per_row;
                o.selected = o.selected.clamp(first, (first + per_row * rows).min(count).saturating_sub(1));
                self.window_overview = Some(o);
            }
            WMEK::Move => {
                if let Some(card) = cards.iter().find(|c| contains(&c.rect, x, y)) {
                    if o.selected != card.index {
                        o.selected = card.index;
                        self.window_overview = Some(o);
                    }
                }
            }
            _ => {}
        }
        context.invalidate();
        true
    }

    pub fn paint_window_overview(&mut self) -> anyhow::Result<()> {
        let Some(o) = self.window_overview.clone() else {
            return Ok(());
        };
        let (grid, cards, per_row) = self.overview_layout();
        let total = self.overview_windows(&o.filter).len();
        let layer = self.render_state.as_ref().unwrap().layer_for_zindex(5)?;
        let mut layers = layer.quad_allocator();
        let palette = self.palette().clone();
        let bg = palette.resolve_bg(ColorAttribute::Default).to_linear();
        let fg = palette.resolve_fg(ColorAttribute::Default).to_linear();
        let mix = |a: LinearRgba, b: LinearRgba, t: f32| {
            LinearRgba::with_components(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t, 1.)
        };
        let w = self.dimensions.pixel_width as f32;
        let h = self.dimensions.pixel_height as f32;
        // The windows are the only content: an opaque, slightly darker backdrop.
        self.filled_rectangle(&mut layers, 0, rect(0., 0., w, h), mix(bg, fg, 0.07))?;

        let metrics = self.render_metrics;
        let ch = metrics.cell_size.height as f32;
        let cw = metrics.cell_size.width as f32;
        let accent = palette.resolve_fg(ColorAttribute::PaletteIndex(124)).to_linear();
        let current = palette.resolve_fg(ColorAttribute::PaletteIndex(2)).to_linear();
        let card_bg = mix(bg, fg, 0.13);
        let radius = 0.;
        for card in &cards {
            let r = card.rect;
            let selected = card.index == o.selected;
            let (border, width) = if selected {
                (accent, 3.)
            } else if card.window.active {
                (current, 2.)
            } else {
                (mix(bg, fg, 0.2), 1.)
            };
            self.fill_rounded(rect(r.min_x() - width, r.min_y() - width, r.width() + 2. * width, r.height() + 2. * width), radius + width, border, 0, &mut layers)?;
            self.fill_rounded(r, radius, card_bg, 0, &mut layers)?;
            let label_h = ch * 1.6;
            let inset = (cw * 0.6).round();
            let thumb = rect(r.min_x() + inset, r.min_y() + inset, r.width() - 2. * inset, r.height() - label_h - inset);
            if let Some(tab) = card.tab_id.and_then(|id| Mux::get().get_tab(id)) {
                self.paint_tab_thumbnail(&tab, thumb, true, &mut layers)?;
            }
            // "1 editor" (index badge as in the strip) ... session on the right.
            let (index_fg, index_bg) = if card.window.active {
                (ColorAttribute::PaletteIndex(7), ColorAttribute::PaletteIndex(124))
            } else {
                (ColorAttribute::PaletteIndex(8), ColorAttribute::PaletteIndex(59))
            };
            // The badge colour is its own rectangle of exactly its cells, so
            // every card has the same badge width and the same gap.
            // The tab's program tiles (as in the tab bar) stand where the
            // number was. Without tiles the number stays.
            let bar_title = card.tab_id.and_then(|id| {
                let index = Mux::get().get_window(self.mux_window_id)?.get_tab_idx_for_id(id)?;
                self.tab_bar.items().iter().find_map(|item| match item.item {
                    crate::tabbar::TabBarItem::Tab { tab_idx, .. } if tab_idx == index => Some(item.title.clone()),
                    _ => None,
                })
            });
            let tiles = bar_title.as_ref().and_then(crate::termwindow::render::tab_icons::icon_names);
            // A window without a name of its own shows the tab bar's name
            // (the program), as the tab bar does.
            let bar_name = bar_title.as_ref().map(|line| line.as_str().trim().to_owned()).filter(|name| !name.is_empty());
            let tile_cols = if tiles.is_some() { 4 } else { 0 };
            let badge = text_line(&if tiles.is_some() { " ".repeat(tile_cols) } else { format!(" {} ", card.window.index) }, index_fg, ColorAttribute::Default, true);
            let badge_cols = badge.len();
            let mut label = badge;
            let mut name = match bar_name {
                Some(bar) if card.window.name.starts_with("Window ") => format!(" {bar}"),
                _ => format!(" {}", card.window.name),
            };
            if card.window.zoomed {
                name.push_str(" 🔍");
            }
            label.append_line(text_line(&name, ColorAttribute::PaletteIndex(59), ColorAttribute::Default, selected), 0);
            let session = text_line(&card.window.session, ColorAttribute::PaletteIndex(245), ColorAttribute::Default, false);
            let max_cols = ((r.width() - 2. * inset) / cw) as usize;
            let room = max_cols.saturating_sub(session.len() + 2);
            if label.len() > room {
                label.resize(room, 0);
            }
            let y = r.max_y() - label_h + (label_h - ch) / 2.;
            let lx = (r.min_x() + inset).round();
            let badge_w = badge_cols.min(label.len()) as f32 * cw;
            if let Some(names) = tiles.as_ref() {
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                let tile_h = (ch * 1.3).round();
                self.render_line_at(&label, lx, y, label.len(), None, metrics, &palette, &mut layers)?;
                self.paint_tiles(&mut layers, &names, lx, y + (ch - tile_h) / 2., tile_cols as f32 * cw, tile_h)?;
            } else {
                self.filled_rectangle(&mut layers, 0, rect(lx, y, badge_w, ch), palette.resolve_fg(index_bg).to_linear())?;
                self.render_line_at(&label, lx, y, label.len(), None, metrics, &palette, &mut layers)?;
            }
            if max_cols > session.len() + 4 {
                let x = r.max_x() - inset - session.len() as f32 * cw;
                self.render_line_at(&session, x, y, session.len(), None, metrics, &palette, &mut layers)?;
            }
        }

        // The filter, only while typing: a pill at the bottom.
        if !o.filter.is_empty() || o.filtering {
            let caret = if o.filtering { "▏" } else { "" };
            let text = format!(" /{}{}  {} of {} ", o.filter, caret, total, self.overview_windows("").len());
            let line = text_line(&text, ColorAttribute::PaletteIndex(59), ColorAttribute::Default, true);
            let pw = line.len() as f32 * cw + 2. * cw;
            let pill = rect((w - pw) / 2., h - ch * 2.2, pw, ch * 1.6);
            self.fill_rounded(rect(pill.min_x() - 2., pill.min_y() - 2., pill.width() + 4., pill.height() + 4.), 0., accent, 1, &mut layers)?;
            self.fill_rounded(pill, 0., bg, 1, &mut layers)?;
            self.render_line_at(&line, pill.min_x() + cw, pill.min_y() + ch * 0.3, line.len(), None, metrics, &palette, &mut layers)?;
        }

        // Scroll position, when there are more windows than fit.
        let (_, rows) = Self::overview_grid(total);
        let total_rows = (total + per_row - 1) / per_row;
        if total_rows > rows {
            let track = rect(w - grid.min_x() * 0.6, grid.min_y(), 3., grid.height());
            self.fill_rounded(track, 1.5, mix(bg, fg, 0.15), 0, &mut layers)?;
            let thumb_h = track.height() * rows as f32 / total_rows as f32;
            let thumb_y = track.min_y() + track.height() * o.first_row as f32 / total_rows as f32;
            self.fill_rounded(rect(track.min_x(), thumb_y, 3., thumb_h), 1.5, accent, 1, &mut layers)?;
        }
        Ok(())
    }
}
