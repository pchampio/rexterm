//! Mode and search chrome lives on the pane separator, outside scrollback.
//! A floating pane (a popup) has no separator: the chrome sits on its bottom
//! frame line, its texts in gaps of the line, like the title on the top line.
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::PositionedPane;
use wezterm_term::{Line, color::ColorAttribute};
use window::color::LinearRgba;

impl crate::TermWindow {
    pub fn paint_pane_mode_status(&mut self, pos: &PositionedPane, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        if !self.config.copy_mode_pane_status { return Ok(()); }
        let is_copy = pos.pane.downcast_ref::<crate::overlay::CopyOverlay>().is_some();
        let is_click_copy = pos.pane.downcast_ref::<crate::overlay::QuickSelectOverlay>().is_some_and(|q| q.is_click_copy());
        if !is_copy && !is_click_copy { return Ok(()); }
        let border = self.get_os_border();
        let (pad_left, pad_top) = self.padding_left_top();
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let tab_height = if self.show_tab_bar { self.tab_bar_pixel_height()? } else { 0. };
        let floating = if pos.is_floating && !pos.pane.has_server_frame() {
            let top = if self.show_tab_bar && !self.config.tab_bar_at_bottom { tab_height } else { 0. };
            let inner = self.compute_background_rect_with_scrollbar(pos, pad_left, pad_top, &border, top + pad_top + border.top.get() as f32);
            let y = if pos.pane.popup_frame().is_some() {
                // The frame ring's bottom row: its line runs through the middle of the row.
                inner.max_y()
            } else {
                // A thin border under the pane: centre the row on it.
                inner.max_y() + self.get_floating_pane_border().bottom.get() as f32 / 2. - ch / 2.
            };
            Some((inner.min_x() + cw, y, ((inner.width() / cw).floor() as usize).saturating_sub(2).max(1)))
        } else { None };
        let cols = floating.map(|(_, _, cols)| cols).unwrap_or(pos.width);
        let (mut line, caret) = if let Some(copy) = pos.pane.downcast_ref::<crate::overlay::CopyOverlay>() {
            copy.status_line(cols)
        } else {
            let mut line = Line::new(0);
            crate::overlay::copy::render_pane_status(&mut line, cols, " CLICK COPY ", "");
            (line, None)
        };
        let x = floating.map(|(x, _, _)| x).unwrap_or(border.left.get() as f32 + pad_left + pos.left as f32 * cw);
        let width = cols as f32 * cw;
        let y = if let Some((_, y, _)) = floating { y } else if pos.top + pos.height >= self.terminal_size.rows && self.show_tab_bar && self.config.tab_bar_at_bottom {
            self.dimensions.pixel_height as f32 - border.bottom.get() as f32 - tab_height
        } else {
            border.top.get() as f32 + pad_top + if self.show_tab_bar && !self.config.tab_bar_at_bottom { tab_height } else { 0. }
                + (pos.top + pos.height) as f32 * ch
        };
        let palette = pos.pane.palette();
        let cursor = StableCursorPosition {
            x: caret.unwrap_or(0).min(pos.width.saturating_sub(1)), y: 0,
            shape: termwiz::surface::CursorShape::SteadyBlock,
            visibility: if caret.is_some() && pos.is_active { termwiz::surface::CursorVisibility::Visible } else { termwiz::surface::CursorVisibility::Hidden },
        };
        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let window_is_transparent = false;
        let default_bg = palette.resolve_bg(ColorAttribute::Default).to_linear();
        if floating.is_some() {
            // Keep the frame line: drop the bar's own line, and clear only
            // behind each text, with one cell of gap on both sides.
            let mut runs: Vec<(usize, usize)> = vec![];
            for cell in line.visible_cells() {
                let i = cell.cell_index();
                let text = cell.str() != "─" && cell.str() != " ";
                if text {
                    match runs.last_mut() {
                        Some((_, end)) if *end + 2 >= i => *end = i,
                        _ => runs.push((i, i)),
                    }
                }
            }
            let blank = wezterm_term::Cell::new(' ', wezterm_term::CellAttributes::default());
            for i in 0..line.len() {
                if line.get_cell(i).is_some_and(|c| c.str() == "─") {
                    line.set_cell(i, blank.clone(), 0);
                }
            }
            for (start, end) in runs {
                let x0 = x + start.saturating_sub(1) as f32 * cw;
                let x1 = (x + (end + 2) as f32 * cw).min(x + width);
                self.filled_rectangle(layers, 0, euclid::rect(x0, y, x1 - x0, ch), default_bg)?;
            }
            if let Some(caret) = caret {
                self.filled_rectangle(layers, 0, euclid::rect(x + caret as f32 * cw, y, cw, ch), default_bg)?;
            }
        } else {
            self.filled_rectangle(layers, 0, euclid::rect(x, y, width, ch), default_bg)?;
        }
        self.render_screen_line(
            RenderScreenLineParams {
                top_pixel_y: y,
                left_pixel_x: x,
                pixel_width: width,
                stable_line_idx: Some(0),
                line: &line,
                selection: 0..0,
                cursor: &cursor,
                palette: &palette,
                dims: &RenderableDimensions {
                    cols: pos.width,
                    physical_top: 0,
                    scrollback_rows: 0,
                    scrollback_top: 0,
                    viewport_rows: 1,
                    dpi: self.terminal_size.dpi,
                    pixel_height: self.render_metrics.cell_size.height as usize,
                    pixel_width: width as usize,
                    reverse_video: false,
                },
                config: &self.config,
                cursor_border_color: palette.cursor_border.to_linear(),
                foreground: palette.foreground.to_linear(),
                pane: Some(&pos.pane),
                is_active: true,
                selection_fg: LinearRgba::default(),
                selection_bg: LinearRgba::default(),
                cursor_fg: palette.cursor_fg.to_linear(),
                cursor_bg: palette.cursor_bg.to_linear(),
                cursor_is_default_color: true,
                white_space,
                filled_box,
                window_is_transparent,
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

        log::debug!(target: "pane_status", "MODE_BAR pane={} x={} y={} width={} body_bottom={}", pos.pane.pane_id(), x, y, width, pos.top + pos.height);
        Ok(())
    }
}
