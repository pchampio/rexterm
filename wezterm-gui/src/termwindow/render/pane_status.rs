//! Mode and search chrome lives on the pane separator, outside scrollback.
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::PositionedPane;
use wezterm_term::{Line, color::ColorAttribute};
use window::color::LinearRgba;

impl crate::TermWindow {
    pub fn paint_pane_mode_status(&mut self, pos: &PositionedPane, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        if !self.config.copy_mode_pane_status { return Ok(()); }
        let (line, caret) = if let Some(copy) = pos.pane.downcast_ref::<crate::overlay::CopyOverlay>() {
            copy.status_line(pos.width)
        } else if pos.pane.downcast_ref::<crate::overlay::QuickSelectOverlay>().is_some_and(|q| q.is_click_copy()) {
            let mut line = Line::new(0);
            crate::overlay::copy::render_pane_status(&mut line, pos.width, " CLICK COPY ", "");
            (line, None)
        } else { return Ok(()); };
        let border = self.get_os_border();
        let (pad_left, pad_top) = self.padding_left_top();
        let cw = self.render_metrics.cell_size.width as f32;
        let ch = self.render_metrics.cell_size.height as f32;
        let tab_height = if self.show_tab_bar { self.tab_bar_pixel_height()? } else { 0. };
        let x = border.left.get() as f32 + pad_left + pos.left as f32 * cw;
        let width = pos.width as f32 * cw;
        let y = if pos.top + pos.height >= self.terminal_size.rows && self.show_tab_bar && self.config.tab_bar_at_bottom {
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
        self.filled_rectangle(layers, 0, euclid::rect(x, y, width, ch), default_bg)?;
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
