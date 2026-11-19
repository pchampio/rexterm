use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use crate::utilsprites::RenderMetrics;
use config::ConfigHandle;
use mux::renderable::RenderableDimensions;
use wezterm_term::color::ColorAttribute;
use window::color::LinearRgba;

impl crate::TermWindow {
    /// The strip under the tabs (tab_bar_active_underline), in pixels; 0
    /// without it.
    pub fn tab_bar_underline_thickness(&self) -> f32 {
        Self::underline_thickness(&self.config, &self.render_metrics)
    }

    pub fn paint_tab_bar(&mut self, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        // While any tab shows the indeterminate progress spinner, ask to be
        // repainted when its next frame is due. The paint scheduler wakes us
        // when this is the soonest pending animation and rebuilds the tab bar
        // to advance the frame.
        self.update_next_frame_time(self.tab_bar.next_progress_frame_due());

        if self.config.use_fancy_tab_bar {
            if self.fancy_tab_bar.is_none() {
                let palette = self.palette().clone();
                let tab_bar = self.build_fancy_tab_bar(&palette)?;
                self.fancy_tab_bar.replace(tab_bar);
            }

            self.ui_items.append(&mut self.paint_fancy_tab_bar()?);
            return Ok(());
        }

        let border = self.get_os_border();

        let palette = self.palette().clone();
        let tab_bar_height = self.tab_bar_pixel_height()?;
        let tab_bar_y = if self.config.tab_bar_at_bottom {
            ((self.dimensions.pixel_height as f32) - (tab_bar_height + border.bottom.get() as f32))
                .max(0.)
        } else {
            border.top.get() as f32
        };

        // Register the tab bar location
        self.ui_items.append(&mut self.tab_bar.compute_ui_items(
            tab_bar_y as usize,
            self.render_metrics.cell_size.height as usize,
            self.render_metrics.cell_size.width as usize,
        ));

        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;
        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let default_bg = palette
            .resolve_bg(ColorAttribute::Default)
            .to_linear()
            .mul_alpha(if window_is_transparent {
                0.
            } else {
                self.config.text_background_opacity
            });

        self.render_screen_line(
            RenderScreenLineParams {
                top_pixel_y: tab_bar_y,
                left_pixel_x: 0.,
                pixel_width: self.dimensions.pixel_width as f32,
                stable_line_idx: None,
                line: self.tab_bar.line(),
                selection: 0..0,
                cursor: &Default::default(),
                palette: &palette,
                dims: &RenderableDimensions {
                    cols: self.dimensions.pixel_width
                        / self.render_metrics.cell_size.width as usize,
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

        // Program icons that the tab titles ask for.
        self.paint_line_icons(layers, &palette, self.tab_bar.line(), 0., tab_bar_y)?;

        // A strip along the bottom of every tab: red under the active tab,
        // the page colour under the others, so each tab ends above it (the
        // tiles are drawn above it too). The entry's first cell is the gap
        // before the tab: the strip starts after it.
        if let Some(colour) = self.config.tab_bar_active_underline {
            let cw = self.render_metrics.cell_size.width as f32;
            let thickness = self.tab_bar_underline_thickness();
            let colour = colour.to_linear();
            let page = palette.resolve_bg(ColorAttribute::Default).to_linear();
            let _ = page;
            let line = self.tab_bar.line().clone();
            for entry in self.tab_bar.items() {
                if let crate::tabbar::TabBarItem::Tab { active, .. } = entry.item {
                    let (x, width) = entry.span();
                    if width <= 1 {
                        continue;
                    }
                    let strip_y = tab_bar_y + tab_bar_height - thickness;
                    if active {
                        let rect = euclid::rect((x + 1) as f32 * cw, strip_y, (width - 1) as f32 * cw, thickness);
                        self.filled_rectangle(layers, 2, rect, colour)?;
                        continue;
                    }
                    // An inactive tab keeps its height: the strip takes the
                    // colour of each cell above it (the grey of the tab).
                    for cell in line.visible_cells() {
                        let i = cell.cell_index();
                        if i > x && i < x + width {
                            let bg = palette.resolve_bg(cell.attrs().background()).to_linear();
                            self.filled_rectangle(layers, 2, euclid::rect(i as f32 * cw, strip_y, cw, thickness), bg)?;
                        }
                    }
                }
            }
        }

        // Draw a real one-pixel rule only through unused background cells;
        // tab blocks, mode labels and transient messages remain unobstructed.
        if self.tab_bar_rule && self.status_message.is_empty() {
            let cw = self.render_metrics.cell_size.width as f32;
            let background = palette.resolve_bg(ColorAttribute::Default);
            let color = palette.foreground.to_linear().mul_alpha(0.4);
            for cell in self.tab_bar.line().visible_cells() {
                if cell.str() == " " && palette.resolve_bg(cell.attrs().background()) == background
                    && !self.tab_bar.status_text_contains_column(cell.cell_index()) {
                    self.filled_rectangle(layers, 2,
                        euclid::rect(cell.cell_index() as f32 * cw, tab_bar_y + self.render_metrics.cell_size.height as f32 / 2., cw, 1.), color)?;
                }
            }
        }

        Ok(())
    }

    pub fn tab_bar_pixel_height_impl(
        config: &ConfigHandle,
        fontconfig: &wezterm_font::FontConfiguration,
        render_metrics: &RenderMetrics,
    ) -> anyhow::Result<f32> {
        if config.use_fancy_tab_bar {
            let font = fontconfig.title_font()?;
            Ok((font.metrics().cell_height.get() as f32 * 1.75).ceil())
        } else {
            // The strip under the tabs (tab_bar_active_underline) is below
            // the text row: the bar is that much higher.
            Ok(render_metrics.cell_size.height as f32 + Self::underline_thickness(config, render_metrics))
        }
    }

    fn underline_thickness(config: &ConfigHandle, render_metrics: &RenderMetrics) -> f32 {
        if config.tab_bar_active_underline.is_none() {
            return 0.;
        }
        (render_metrics.cell_size.height as f32 * 0.18).round().max(3.)
    }

    pub fn tab_bar_pixel_height(&self) -> anyhow::Result<f32> {
        Self::tab_bar_pixel_height_impl(&self.config, &self.fonts, &self.render_metrics)
    }
}
