//! Frames of tmux popups shown as native floating panes (companion tmux:
//! display-popup for a client-ui client becomes `new-pane -O -G -B none`).
//! tmux draws no border for them; the frame and title are drawn here, in the
//! cell ring around the pane, so they move with the pane when it is dragged.
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use mux::pane::PopupFrame;
use mux::renderable::RenderableDimensions;
use mux::tab::PositionedPane;
use wezterm_term::color::{ColorAttribute, ColorPalette, SrgbaTuple};
use wezterm_term::{CellAttributes, Line};
use window::color::LinearRgba;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Centre,
    Right,
}

/// One `#[...]` style list from a tmux format: only what a frame needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TmuxStyle {
    pub fg: Option<ColorAttribute>,
    pub bg: Option<ColorAttribute>,
    pub bold: Option<bool>,
    pub align: Option<Align>,
}

fn tmux_colour(value: &str) -> Option<ColorAttribute> {
    let value = value.trim().to_ascii_lowercase();
    if value == "default" || value == "terminal" {
        return Some(ColorAttribute::Default);
    }
    for prefix in ["colour", "color"] {
        if let Some(n) = value.strip_prefix(prefix) {
            return n.parse::<u8>().ok().map(ColorAttribute::PaletteIndex);
        }
    }
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 {
            let n = u32::from_str_radix(hex, 16).ok()?;
            let c = |shift: u32| ((n >> shift) & 0xff) as f32 / 255.0;
            return Some(ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
                c(16),
                c(8),
                c(0),
                1.0,
            )));
        }
        return None;
    }
    let names = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    let (bright, name) = match value.strip_prefix("bright") {
        Some(name) => (8, name),
        None => (0, value.as_str()),
    };
    names
        .iter()
        .position(|n| *n == name)
        .map(|i| ColorAttribute::PaletteIndex((i + bright) as u8))
}

/// Parses a tmux style list such as `align=centre,fg=color0,bg=color248`.
pub fn parse_tmux_style(style: &str, into: &mut TmuxStyle) {
    for item in style.split([',', ' ']).filter(|s| !s.is_empty()) {
        let (key, value) = item.split_once('=').unwrap_or((item, ""));
        match key.trim() {
            "fg" => into.fg = tmux_colour(value),
            "bg" => into.bg = tmux_colour(value),
            "bold" => into.bold = Some(true),
            "nobold" => into.bold = Some(false),
            "default" | "none" | "noattr" => *into = TmuxStyle::default(),
            "align" => {
                into.align = match value.trim() {
                    "left" => Some(Align::Left),
                    "right" => Some(Align::Right),
                    "centre" | "center" => Some(Align::Centre),
                    _ => into.align,
                }
            }
            _ => {}
        }
    }
}

/// Splits a title into styled runs: `#[...]` changes the style, `##` is `#`.
/// Returns the runs and the alignment requested anywhere in the title.
pub fn title_runs(title: &str) -> (Vec<(TmuxStyle, String)>, Align) {
    let mut runs: Vec<(TmuxStyle, String)> = vec![];
    let mut style = TmuxStyle::default();
    // tmux draws a popup title on the left unless it asks for align=centre.
    let mut align = Align::Left;
    let mut text = String::new();
    let mut rest = title;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("##") {
            text.push('#');
            rest = after;
        } else if let Some(after) = rest.strip_prefix("#[") {
            let Some(end) = after.find(']') else {
                text.push_str(rest);
                break;
            };
            if !text.is_empty() {
                runs.push((style.clone(), std::mem::take(&mut text)));
            }
            parse_tmux_style(&after[..end], &mut style);
            if let Some(a) = style.align {
                align = a;
            }
            rest = &after[end + 1..];
        } else {
            let mut chars = rest.chars();
            text.push(chars.next().unwrap());
            rest = chars.as_str();
        }
    }
    if !text.is_empty() {
        runs.push((style, text));
    }
    (runs, align)
}

/// Title for window decorations and tab titles: the text without styles.
pub fn plain_title(title: &str) -> String {
    title_runs(title)
        .0
        .into_iter()
        .map(|(_, text)| text)
        .collect::<String>()
        .trim()
        .to_string()
}

impl crate::TermWindow {
    /// The pane's pixel rectangle grown by one cell: the frame's ring.
    pub fn popup_outer_rect(
        &self,
        background: euclid::Rect<f32, window::PixelUnit>,
    ) -> euclid::Rect<f32, window::PixelUnit> {
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        euclid::rect(
            background.min_x() - cw,
            background.min_y() - ch,
            background.width() + 2. * cw,
            background.height() + 2. * ch,
        )
    }

    /// The outer rect of a popup frame: the ring, and the subtitle row.
    pub fn popup_frame_outer_rect(
        &self,
        frame: &PopupFrame,
        background: euclid::Rect<f32, window::PixelUnit>,
    ) -> euclid::Rect<f32, window::PixelUnit> {
        let outer = self.popup_outer_rect(background);
        if frame.subtitle.is_empty() {
            return outer;
        }
        let ch = self.render_metrics.cell_size.height as f32;
        euclid::rect(outer.min_x(), outer.min_y() - ch, outer.width(), outer.height() + ch)
    }

    pub fn paint_popup_frame(
        &mut self,
        pos: &PositionedPane,
        frame: &PopupFrame,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let (padding_left, padding_top) = self.padding_left_top();
        let os_border = self.get_os_border();
        let tab_bar_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height()?
        } else {
            0.
        };
        let top_pixel_y = tab_bar_height + padding_top + os_border.top.get() as f32;
        let inner = self.compute_background_rect_with_scrollbar(
            pos,
            padding_left,
            padding_top,
            &os_border,
            top_pixel_y,
        );
        let outer = self.popup_frame_outer_rect(frame, inner);
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let palette = self.palette().clone();
        // The subtitle row, between the title row and the terminal.
        let subtitle_row = (!frame.subtitle.is_empty()).then(|| euclid::rect(outer.min_x(), inner.min_y() - ch, outer.width(), ch));

        // The ring is opaque, like tmux's border cells.
        let bg = palette.resolve_bg(ColorAttribute::Default).to_linear();
        for rect in [
            euclid::rect(outer.min_x(), outer.min_y(), outer.width(), ch),
            euclid::rect(outer.min_x(), inner.max_y(), outer.width(), ch),
            euclid::rect(outer.min_x(), inner.min_y(), cw, inner.height()),
            euclid::rect(inner.max_x(), inner.min_y(), cw, inner.height()),
        ] {
            self.filled_rectangle(layers, 0, rect, bg)?;
        }
        if let Some(row) = subtitle_row {
            self.filled_rectangle(layers, 0, row, bg)?;
        }

        let mut border = TmuxStyle::default();
        parse_tmux_style(&frame.border_style, &mut border);
        let line_colour: LinearRgba = palette
            .resolve_fg(border.fg.unwrap_or(ColorAttribute::Default))
            .to_linear();

        // Title runs, centred (or aligned) in the top row of the ring.
        let (runs, align) = title_runs(&frame.title);
        let mut title = Line::with_width(0, 0);
        for (style, text) in &runs {
            let mut attrs = CellAttributes::default();
            if let Some(fg) = style.fg {
                attrs.set_foreground(fg);
            }
            if let Some(bg) = style.bg {
                attrs.set_background(bg);
            }
            if style.bold == Some(true) {
                attrs.set_intensity(wezterm_term::Intensity::Bold);
            }
            title.append_line(Line::from_text(text, &attrs, 0, None), 0);
        }
        let ring_cols = (outer.width() / cw).round() as usize;
        let title_cols = title.len().min(ring_cols.saturating_sub(4));
        let title_x = match align {
            Align::Left => outer.min_x() + 2. * cw,
            Align::Right => outer.max_x() - (2 + title_cols) as f32 * cw,
            Align::Centre => {
                outer.min_x() + ((ring_cols.saturating_sub(title_cols)) / 2) as f32 * cw
            }
        };

        // Thin lines through the middle of the ring cells; the top line
        // stops one cell before and after the title.
        let t = (cw / 8.).max(1.).round();
        let mid_x0 = outer.min_x() + cw / 2. - t / 2.;
        let mid_x1 = inner.max_x() + cw / 2. - t / 2.;
        let mid_y0 = outer.min_y() + ch / 2. - t / 2.;
        let mid_y1 = inner.max_y() + ch / 2. - t / 2.;
        let mut lines = vec![
            euclid::rect(mid_x0, mid_y1, mid_x1 - mid_x0 + t, t),
            euclid::rect(mid_x0, mid_y0, t, mid_y1 - mid_y0 + t),
            euclid::rect(mid_x1, mid_y0, t, mid_y1 - mid_y0 + t),
        ];
        if title_cols == 0 {
            lines.push(euclid::rect(mid_x0, mid_y0, mid_x1 - mid_x0 + t, t));
        } else {
            let gap0 = title_x - cw;
            let gap1 = title_x + (title_cols + 1) as f32 * cw;
            lines.push(euclid::rect(mid_x0, mid_y0, (gap0 - mid_x0).max(0.), t));
            lines.push(euclid::rect(gap1, mid_y0, (mid_x1 + t - gap1).max(0.), t));
        }
        for rect in lines {
            self.filled_rectangle(layers, 2, rect, line_colour)?;
        }

        if title_cols > 0 {
            if title.len() > title_cols {
                title.resize(title_cols, 0);
            }
            self.paint_popup_title(&title, title_x, outer.min_y(), &palette, layers)?;
        }
        if let Some(row) = subtitle_row {
            // The command, in bold.
            let mut attrs = CellAttributes::default();
            attrs.set_foreground(ColorAttribute::PaletteIndex(4));
            attrs.set_intensity(wezterm_term::Intensity::Bold);
            let mut line = Line::from_text(&frame.subtitle, &attrs, 0, None);
            let cols = ((inner.width() / cw).floor() as usize).max(1);
            if line.len() > cols {
                line.resize(cols.saturating_sub(1), 0);
                line.append_line(Line::from_text("…", &attrs, 0, None), 0);
            }
            self.paint_popup_title(&line, inner.min_x(), row.min_y(), &palette, layers)?;
        }
        Ok(())
    }

    fn paint_popup_title(
        &mut self,
        title: &Line,
        left_pixel_x: f32,
        top_pixel_y: f32,
        palette: &ColorPalette,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let default_bg = palette.resolve_bg(ColorAttribute::Default).to_linear();
        self.render_screen_line(
            RenderScreenLineParams {
                top_pixel_y,
                left_pixel_x,
                pixel_width: title.len() as f32 * self.render_metrics.cell_size.width as f32,
                stable_line_idx: None,
                line: title,
                selection: 0..0,
                cursor: &Default::default(),
                palette,
                dims: &RenderableDimensions {
                    cols: title.len(),
                    physical_top: 0,
                    scrollback_rows: 0,
                    scrollback_top: 0,
                    viewport_rows: 1,
                    dpi: self.terminal_size.dpi,
                    pixel_height: self.render_metrics.cell_size.height as usize,
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
                font: None,
                use_pixel_positioning: self.config.experimental_pixel_positioning,
                render_metrics: self.render_metrics,
                shape_key: None,
                password_input: false,
            },
            layers,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_popup_titles() {
        let (runs, align) = title_runs("#[align=centre,fg=color0,bg=color248] Python pad ");
        assert_eq!(align, Align::Centre);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1, " Python pad ");
        assert_eq!(runs[0].0.fg, Some(ColorAttribute::PaletteIndex(0)));
        assert_eq!(runs[0].0.bg, Some(ColorAttribute::PaletteIndex(248)));
        assert_eq!(plain_title("#[fg=colour4]Readline ##1"), "Readline #1");
    }

    #[test]
    fn border_styles() {
        let mut style = TmuxStyle::default();
        parse_tmux_style("fg=colour001", &mut style);
        assert_eq!(style.fg, Some(ColorAttribute::PaletteIndex(1)));
        parse_tmux_style("fg=brightred,bg=default", &mut style);
        assert_eq!(style.fg, Some(ColorAttribute::PaletteIndex(9)));
        assert_eq!(style.bg, Some(ColorAttribute::Default));
        parse_tmux_style("fg=#d33682", &mut style);
        assert!(matches!(style.fg, Some(ColorAttribute::TrueColorWithDefaultFallback(_))));
    }
}
