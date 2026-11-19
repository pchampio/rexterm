use crate::quad::{HeapQuadAllocator, QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::selection::SelectionRange;
use crate::termwindow::box_model::*;
use crate::termwindow::render::{
    same_hyperlink, CursorProperties, LineQuadCacheKey, LineQuadCacheValue, LineToEleShapeCacheKey,
    RenderScreenLineParams,
};
use crate::termwindow::{ScrollHit, UIItem, UIItemType};
use ::window::bitmaps::TextureRect;
use ::window::DeadKeyStatus;
use anyhow::Context;
use config::VisualBellTarget;
use mux::pane::{PaneId, WithPaneLines};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::PositionedPane;
use ordered_float::NotNan;
use std::time::Instant;
use wezterm_dynamic::Value;
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{Line, StableRowIndex};
use window::color::LinearRgba;

impl crate::TermWindow {
    fn paint_pane_box_model(&mut self, pos: &PositionedPane) -> anyhow::Result<()> {
        let computed = self.build_pane(pos)?;
        let mut ui_items = computed.ui_items();
        self.ui_items.append(&mut ui_items);
        let gl_state = self.render_state.as_ref().unwrap();
        self.render_element(&computed, gl_state, None)
    }

    pub fn paint_pane(
        &mut self,
        pos: &PositionedPane,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        if self.config.use_box_model_render {
            return self.paint_pane_box_model(pos);
        }

        self.check_for_dirty_lines_and_invalidate_selection(&pos.pane);
        /*
        let zone = {
            let dims = pos.pane.get_dimensions();
            let position = self
                .get_viewport(pos.pane.pane_id())
                .unwrap_or(dims.physical_top);

            let zones = self.get_semantic_zones(&pos.pane);
            let idx = match zones.binary_search_by(|zone| zone.start_y.cmp(&position)) {
                Ok(idx) | Err(idx) => idx,
            };
            let idx = ((idx as isize) - 1).max(0) as usize;
            zones.get(idx).cloned()
        };
        */

        let global_cursor_fg = self.palette().cursor_fg;
        let global_cursor_bg = self.palette().cursor_bg;
        let config = self.config.clone();
        let mut palette = pos.pane.palette();
        if self.pane_state(pos.pane.pane_id()).marked {
            let (r,g,b,a)=palette.background.to_tuple_rgba();
            palette.background=(r*0.94,g*0.94,b*0.94,a).into();
        }
        let style=self.pane_state(pos.pane.pane_id()).selection_style.clone();
        let style=if let Some((until,style))=style {
            if Instant::now()<until {self.update_next_frame_time(Some(until));Some(style)}
            else {self.pane_state(pos.pane.pane_id()).selection_style=None;self.quad_generation+=1;None}
        } else {None};
        let copy_overlay=pos.pane.is::<crate::overlay::copy::CopyOverlay>();
        let command_selection=pos.pane.downcast_ref::<crate::overlay::CopyOverlay>().is_some_and(|o|o.is_command_selection());
        if copy_overlay {
            if let Some(bg)=config.copy_mode_selection_background {
                palette.selection_bg=bg.into();
                palette.selection_fg=(0.,0.,0.,0.).into();
            }
        }
        if pos.pane.downcast_ref::<crate::overlay::CopyOverlay>().is_some_and(|c| c.selection_is_search_match()) {
            if let Some(bg) = config.resolved_palette.copy_mode_active_highlight_bg { palette.selection_bg = palette.resolve_bg(bg.into()); }
            if let Some(fg) = config.resolved_palette.copy_mode_active_highlight_fg { palette.selection_fg = palette.resolve_fg(fg.into()); }
        }
        if let Some(style)=&style {palette.selection_fg=style.foreground.into();palette.selection_bg=style.background.into();}
        let selection_bold=(!copy_overlay && config.selection_bold) || style.as_ref().is_some_and(|s|s.bold);
        let selection_underline=(!copy_overlay && config.selection_underline) || style.as_ref().is_some_and(|s|s.underline);

        let (padding_left, padding_top) = self.padding_left_top();

        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height()
                .context("tab_bar_pixel_height")?
        } else {
            0.
        };
        let (top_bar_height, bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };

        let border = self.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        let cursor = pos.pane.get_cursor_position();
        if pos.is_active {
            self.prev_cursor.update(&cursor);
        }

        let pane_id = pos.pane.pane_id();
        // The overlay updates its viewport synchronously; the window receives
        // a queued notification. Paint rows and animation from the same anchor
        // even if a key arrives before that notification is processed.
        let current_viewport = pos.pane.copy_mode_scroll_position()
            .map(|(position, _)| -position)
            .or_else(|| self.get_viewport(pane_id));
        let mut dims = pos.pane.get_dimensions();
        if pos.pane.downcast_ref::<crate::overlay::CopyOverlay>().is_some() {
            log::debug!(target: "copy_scroll", "COPY_PAINTED {} {}", pane_id, current_viewport.unwrap_or(dims.physical_top));
        }
        // While a tmux separator is dragged the pane is narrower than its
        // terminal until tmux resizes it: draw only what fits.
        dims.cols = dims.cols.min(pos.width.max(1));

        let gl_state = self.render_state.as_ref().unwrap();

        let cursor_border_color = palette.cursor_border.to_linear();
        let foreground = palette.foreground.to_linear();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();

        let window_is_transparent = !pos.is_floating
            && (!self.window_background.is_empty() || config.window_background_opacity != 1.0);

        let default_bg = palette
            .resolve_bg(ColorAttribute::Default)
            .to_linear()
            .mul_alpha(if pos.is_floating {
                1.0
            } else if window_is_transparent {
                0.
            } else {
                config.text_background_opacity
            });

        let background_rect = self.compute_background_rect_with_scrollbar(
            &pos,
            padding_left,
            padding_top,
            &border,
            top_pixel_y,
        );

        if self.window_background.is_empty() || pos.is_floating {
            // Per-pane, palette-specified background

            let mut quad = self
                .filled_rectangle(
                    layers,
                    0,
                    background_rect,
                    palette
                        .background
                        .to_linear()
                        .mul_alpha(if pos.is_floating {
                            1.0
                        } else {
                            config.window_background_opacity
                        }),
                )
                .context("filled_rectangle")?;
            quad.set_hsv(if pos.is_active {
                None
            } else {
                Some(config.inactive_pane_hsb)
            });
        }

        {
            // If the bell is ringing, we draw another background layer over the
            // top of this in the configured bell color
            if let Some(intensity) = self.get_intensity_if_bell_target_ringing(
                &pos.pane,
                &config,
                VisualBellTarget::BackgroundColor,
            ) {
                // target background color
                let LinearRgba(r, g, b, _) = config
                    .resolved_palette
                    .visual_bell
                    .as_deref()
                    .unwrap_or(&palette.foreground)
                    .to_linear();

                let background = if window_is_transparent {
                    // for transparent windows, we fade in the target color
                    // by adjusting its alpha
                    LinearRgba::with_components(r, g, b, intensity)
                } else {
                    // otherwise We'll interpolate between the background color
                    // and the target color
                    let (r1, g1, b1, a) = palette
                        .background
                        .to_linear()
                        .mul_alpha(config.window_background_opacity)
                        .tuple();
                    LinearRgba::with_components(
                        r1 + (r - r1) * intensity,
                        g1 + (g - g1) * intensity,
                        b1 + (b - b1) * intensity,
                        a,
                    )
                };
                log::trace!("bell color is {:?}", background);

                let mut quad = self
                    .filled_rectangle(layers, 0, background_rect, background)
                    .context("filled_rectangle")?;

                quad.set_hsv(if pos.is_active {
                    None
                } else {
                    Some(config.inactive_pane_hsb)
                });
            }
        }

        // TODO: we only have a single scrollbar in a single position.
        // We only update it for the active pane, but we should probably
        // do a per-pane scrollbar.  That will require more extensive
        // changes to ScrollHit, mouse positioning, PositionedPane
        // and tab size calculation.
        if pos.is_active
            && self.show_scroll_bar
            && (!self.is_floating_pane_active() || pos.is_floating)
        {
            let padding = self.effective_right_padding(&config) as f32;
            let (thumb_y_offset, temp_bottom, thumb_x) = if self.is_floating_pane_active() {
                let pos_y = background_rect.origin.y;
                let pixel_height = background_rect.size.height;
                let x = background_rect.width() + background_rect.origin.x - padding;
                (
                    pos_y as usize,
                    pos_y as usize + pixel_height as usize,
                    x as usize,
                )
            } else {
                let win_y_offset = top_bar_height as usize;
                let win_bottom = self
                    .dimensions
                    .pixel_height
                    .saturating_sub(border.bottom.get() + bottom_bar_height as usize);
                let x = self.dimensions.pixel_width - padding as usize - border.right.get();
                (win_y_offset + border.top.get(), win_bottom, x)
            };

            let min_height = self.min_scroll_bar_height();

            let info = ScrollHit::thumb(
                &*pos.pane,
                current_viewport,
                temp_bottom.saturating_sub(thumb_y_offset),
                min_height as usize,
            );

            let scroll_height = info.height;

            let abs_thumb_top = thumb_y_offset + info.top;
            let thumb_size = scroll_height;
            let color = palette.scrollbar_thumb.to_linear();

            // Adjust the scrollbar thumb position
            let config = &self.config;
            let padding = self.effective_right_padding(&config) as f32;

            // Register the scroll bar location
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: thumb_y_offset,
                height: info.top,
                item_type: UIItemType::AboveScrollThumb,
            });
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: abs_thumb_top,
                height: scroll_height,
                item_type: UIItemType::ScrollThumb,
            });
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: abs_thumb_top + thumb_size,
                height: temp_bottom.saturating_sub(abs_thumb_top + thumb_size),
                item_type: UIItemType::BelowScrollThumb,
            });

            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    thumb_x as f32,
                    abs_thumb_top as f32,
                    padding,
                    scroll_height as f32,
                ),
                color,
            )
            .context("filled_rectangle")?;
        }

        let (selrange, rectangular) = {
            let sel = self.selection(pos.pane.pane_id());
            (sel.range.clone(), sel.rectangular)
        };

        let mut scroll_position = if self.config.copy_mode_smooth_scrolling && pos.pane.copy_mode_scroll_position().is_some() {
            pos.pane.copy_mode_scroll_position()
        } else if self.config.tmux_smooth_scrolling && current_viewport.is_none() {
            pos.pane.tmux_copy_scroll_position()
        } else { None };
        if scroll_position.is_none() { self.pane_state(pane_id).copy_scroll = None; }
        let buffered_scroll = scroll_position.is_some();
        let mut scroll_buffer = HeapQuadAllocator::default();
        let mut cursor_buffer = HeapQuadAllocator::default();
        let mut plain_buffer = HeapQuadAllocator::default();
        let mut styled_buffer = HeapQuadAllocator::default();
        let mut copy_selection = None;
        let mut cursor_rect = None;

        let start = Instant::now();
        let selection_fg = palette.selection_fg.to_linear();
        let selection_bg = palette.selection_bg.to_linear();
        let cursor_fg = palette.cursor_fg.to_linear();
        let cursor_bg = palette.cursor_bg.to_linear();
        let cursor_is_default_color =
            palette.cursor_fg == global_cursor_fg && palette.cursor_bg == global_cursor_bg;

        {
            let stable_range = match current_viewport {
                Some(top) => top..top + dims.viewport_rows as StableRowIndex,
                None => dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex,
            };

            pos.pane
                .apply_hyperlinks(stable_range.clone(), &self.config.hyperlink_rules);

            struct LineRender<'a> {
                animate_copy_scroll: bool,
                native_copy_scroll: bool,
                search_selection: bool,
                selection_bold: bool,
                selection_underline: bool,
                copy_scroll_position: Option<(isize, usize)>,
                command_selection: bool,
                term_window: &'a mut crate::TermWindow,
                selrange: Option<SelectionRange>,
                rectangular: bool,
                dims: RenderableDimensions,
                top_pixel_y: f32,
                left_pixel_x: f32,
                pos: &'a PositionedPane,
                pane_id: PaneId,
                cursor: StableCursorPosition,
                cursor_layers: &'a mut HeapQuadAllocator,
                plain_layers: &'a mut HeapQuadAllocator,
                styled_layers: &'a mut HeapQuadAllocator,
                copy_selection: Option<std::sync::Arc<mux::pane::TmuxCopySelection>>,
                cursor_rect: Option<(f32, f32, f32, f32)>,
                palette: &'a ColorPalette,
                default_bg: LinearRgba,
                cursor_border_color: LinearRgba,
                selection_fg: LinearRgba,
                selection_bg: LinearRgba,
                cursor_fg: LinearRgba,
                cursor_bg: LinearRgba,
                foreground: LinearRgba,
                cursor_is_default_color: bool,
                white_space: TextureRect,
                filled_box: TextureRect,
                window_is_transparent: bool,
                layers: &'a mut dyn TripleLayerQuadAllocatorTrait,
                error: Option<anyhow::Error>,
            }

            let left_pixel_x = padding_left
                + border.left.get() as f32
                + (pos.left as f32 * self.render_metrics.cell_size.width as f32);

            let mut render = LineRender {
                animate_copy_scroll: buffered_scroll,
                native_copy_scroll: buffered_scroll && pos.pane.copy_mode_scroll_position().is_some(),
                search_selection: pos.pane.downcast_ref::<crate::overlay::CopyOverlay>().is_some_and(|c|c.selection_is_search_match()),
                selection_bold,
                selection_underline,
                copy_scroll_position: scroll_position,
                command_selection,
                term_window: self,
                selrange,
                rectangular,
                dims,
                top_pixel_y,
                left_pixel_x,
                pos,
                pane_id,
                cursor,
                cursor_layers: &mut cursor_buffer,
                plain_layers: &mut plain_buffer,
                styled_layers: &mut styled_buffer,
                copy_selection: None,
                cursor_rect: None,
                palette: &palette,
                cursor_border_color,
                selection_fg,
                selection_bg,
                cursor_fg,
                default_bg,
                cursor_bg,
                foreground,
                cursor_is_default_color,
                white_space,
                filled_box,
                window_is_transparent,
                layers: if scroll_position.is_some() { &mut scroll_buffer } else { layers },
                error: None,
            };

            impl<'a> LineRender<'a> {
                fn render_line(
                    &mut self,
                    stable_top: StableRowIndex,
                    line_idx: usize,
                    line: &Line,
                    pass: u8,
                ) -> anyhow::Result<()> {
                    // A pane narrower than its terminal (tmux separator drag):
                    // cells past the pane's width are not drawn.
                    let clipped;
                    let line = if line.len() > self.dims.cols {
                        let mut copy = line.clone();
                        copy.resize(self.dims.cols, 0);
                        clipped = copy;
                        &clipped
                    } else {
                        line
                    };
                    let cursor_pass = pass == 1;
                    let cursor = if self.copy_scroll_position.is_some() && !cursor_pass {
                        StableCursorPosition { y: StableRowIndex::MIN, ..self.cursor }
                    } else { self.cursor };
                    let stable_row = stable_top + line_idx as StableRowIndex;
                    let selrange = self
                        .selrange
                        .map_or(0..0, |sel| sel.cols_for_row(stable_row, self.rectangular));
                    let selrange = if self.command_selection {crate::overlay::copy::command_selection_columns(line,selrange)} else {selrange};
                    // Constrain to the pane width!
                    let selrange = if self.native_copy_scroll && pass==2 {0..0}
                        else if self.native_copy_scroll && pass==3 {0..self.dims.cols}
                        else {selrange.start..selrange.end.min(self.dims.cols)};

                    // Selection decoration is a GUI style; it never changes
                    // application cells or the text copied from the terminal.
                    let decorated;
                    let line=if !selrange.is_empty() && (self.selection_bold || self.selection_underline) {
                        let mut copy=line.clone();
                        for x in selrange.clone() {
                            if let Some(cell)=copy.cells_mut_for_attr_changes_only().get_mut(x) {
                                if self.selection_bold {cell.attrs_mut().set_intensity(wezterm_term::Intensity::Bold);}
                                if self.selection_underline {cell.attrs_mut().set_underline(wezterm_term::Underline::Single);}
                            }
                        }
                        copy.clear_appdata();
                        decorated=copy;
                        &decorated
                    } else {line};

                    let (cursor_properties, composing, password_input) = if cursor.y == stable_row {
                        (
                            Some(CursorProperties {
                                position: StableCursorPosition {
                                    y: 0,
                                    ..cursor
                                },
                                dead_key_or_leader: self.term_window.dead_key_status
                                    != DeadKeyStatus::None
                                    || self.term_window.leader_is_active(),
                                cursor_fg: self.cursor_fg,
                                cursor_bg: self.cursor_bg,
                                cursor_border_color: self.cursor_border_color,
                                cursor_is_default_color: self.cursor_is_default_color,
                            }),
                            match (self.pos.is_active, &self.term_window.dead_key_status) {
                                (true, DeadKeyStatus::Composing(composing)) => {
                                    Some(composing.to_string())
                                }
                                _ => None,
                            },
                            if self.term_window.config.detect_password_input {
                                match self.pos.pane.get_metadata() {
                                    Value::Object(obj) => {
                                        match obj.get(&Value::String("password_input".to_string()))
                                        {
                                            Some(Value::Bool(b)) => *b,
                                            _ => false,
                                        }
                                    }
                                    _ => false,
                                }
                            } else {
                                false
                            },
                        )
                    } else {
                        (None, None, false)
                    };

                    if cursor_pass {
                        let cw = self.term_window.render_metrics.cell_size.width as f32;
                        let ch = self.term_window.render_metrics.cell_size.height as f32;
                        let width = line.get_cell(cursor.x).map(|c| c.width()).unwrap_or(1).max(1);
                        let x = self.term_window.dimensions.pixel_width as f32 / -2. + self.left_pixel_x + cursor.x as f32 * cw;
                        let y = self.term_window.dimensions.pixel_height as f32 / -2. + self.top_pixel_y + (line_idx + self.pos.top) as f32 * ch;
                        self.cursor_rect = Some((x, y, x + width as f32 * cw, y + ch));
                    }

                    let shape_hash = self.term_window.shape_hash_for_line(line);

                    let quad_key = LineQuadCacheKey {
                        pane_id: self.pane_id,
                        password_input,
                        pane_is_active: self.pos.is_active,
                        config_generation: self.term_window.config.generation(),
                        shape_generation: self.term_window.shape_generation,
                        quad_generation: self.term_window.quad_generation,
                        composing: composing.clone(),
                        selection: selrange.clone(),
                        cursor: cursor_properties,
                        shape_hash,
                        top_pixel_y: NotNan::new(self.top_pixel_y).unwrap()
                            + (line_idx + self.pos.top) as f32
                                * self.term_window.render_metrics.cell_size.height as f32,
                        left_pixel_x: NotNan::new(self.left_pixel_x).unwrap(),
                        phys_line_idx: line_idx,
                        reverse_video: self.dims.reverse_video,
                    };

                    if let Some(cached_quad) =
                        self.term_window.line_quad_cache.borrow_mut().get(&quad_key)
                    {
                        let expired = cached_quad
                            .expires
                            .map(|i| Instant::now() >= i)
                            .unwrap_or(false);
                        let hover_changed = if cached_quad.invalidate_on_hover_change {
                            !same_hyperlink(
                                cached_quad.current_highlight.as_ref(),
                                self.term_window.current_highlight.as_ref(),
                            )
                        } else {
                            false
                        };
                        if !expired && !hover_changed {
                            cached_quad
                                .layers
                                .apply_to(match pass { 1 => self.cursor_layers, 2 => self.plain_layers, 3 => self.styled_layers, _ => self.layers })
                                .context("cached_quad.layers.apply_to")?;
                            self.term_window.update_next_frame_time(cached_quad.expires);
                            return Ok(());
                        }
                    }

                    let mut buf = HeapQuadAllocator::default();
                    let next_due = self.term_window.has_animation.borrow_mut().take();

                    let shape_key = LineToEleShapeCacheKey {
                        shape_hash,
                        shape_generation: quad_key.shape_generation,
                        composing: if cursor.y == stable_row && self.pos.is_active {
                            if let DeadKeyStatus::Composing(composing) =
                                &self.term_window.dead_key_status
                            {
                                Some((cursor.x, composing.to_string()))
                            } else {
                                None
                            }
                        } else {
                            None
                        },
                    };

                    let render_result = self
                        .term_window
                        .render_screen_line(
                            RenderScreenLineParams {
                                top_pixel_y: *quad_key.top_pixel_y,
                                left_pixel_x: self.left_pixel_x,
                                pixel_width: self.dims.cols as f32
                                    * self.term_window.render_metrics.cell_size.width as f32,
                                stable_line_idx: Some(stable_row),
                                line: &line,
                                selection: selrange.clone(),
                                cursor: &cursor,
                                palette: &self.palette,
                                dims: &self.dims,
                                config: &self.term_window.config,
                                cursor_border_color: self.cursor_border_color,
                                foreground: self.foreground,
                                is_active: self.pos.is_active,
                                pane: Some(&self.pos.pane),
                                selection_fg: self.selection_fg,
                                selection_bg: self.selection_bg,
                                cursor_fg: self.cursor_fg,
                                cursor_bg: self.cursor_bg,
                                cursor_is_default_color: self.cursor_is_default_color,
                                white_space: self.white_space,
                                filled_box: self.filled_box,
                                window_is_transparent: self.window_is_transparent,
                                default_bg: self.default_bg,
                                font: None,
                                style: None,
                                use_pixel_positioning: self
                                    .term_window
                                    .config
                                    .experimental_pixel_positioning,
                                render_metrics: self.term_window.render_metrics,
                                shape_key: Some(shape_key),
                                password_input,
                            },
                            &mut TripleLayerQuadAllocator::Heap(&mut buf),
                        )
                        .context("render_screen_line")?;

                    let expires = self.term_window.has_animation.borrow().as_ref().cloned();
                    self.term_window.update_next_frame_time(next_due);

                    buf.apply_to(match pass { 1 => self.cursor_layers, 2 => self.plain_layers, 3 => self.styled_layers, _ => self.layers })
                        .context("HeapQuadAllocator::apply_to")?;

                    let quad_value = LineQuadCacheValue {
                        layers: buf,
                        expires,
                        invalidate_on_hover_change: render_result.invalidate_on_hover_change,
                        current_highlight: if render_result.invalidate_on_hover_change {
                            self.term_window.current_highlight.clone()
                        } else {
                            None
                        },
                    };

                    self.term_window
                        .line_quad_cache
                        .borrow_mut()
                        .put(quad_key, quad_value);

                    Ok(())
                }
            }

            impl<'a> WithPaneLines for LineRender<'a> {
                fn set_tmux_copy_selection(&mut self, selection: Option<std::sync::Arc<mux::pane::TmuxCopySelection>>) {
                    self.copy_selection = if self.animate_copy_scroll { selection } else { None };
                }
                fn set_tmux_copy_scroll_position(&mut self, position: Option<(isize, usize)>, cursor: StableCursorPosition) {
                    self.copy_scroll_position = if self.animate_copy_scroll { position } else { None };
                    // Position, viewport and cells belong to one terminal snapshot.
                    // Preserve the domain's inactive-pane visibility mask.
                    if self.copy_scroll_position.is_some() {
                        self.cursor.x = cursor.x;
                        self.cursor.y = cursor.y;
                        self.cursor.shape = cursor.shape;
                        if self.cursor.visibility == termwiz::surface::CursorVisibility::Visible {
                            self.cursor.visibility = cursor.visibility;
                        }
                    }
                }
                fn with_lines_mut(&mut self, stable_top: StableRowIndex, lines: &mut [&mut Line]) {
                    if self.native_copy_scroll {
                        if let Some(range)=self.selrange {
                            let column=|x:crate::selection::SelectionX|match x {crate::selection::SelectionX::Cell(x)=>x.min(self.dims.cols.saturating_sub(1)) as i64,crate::selection::SelectionX::BeforeZero=>0};
                            self.copy_selection=Some(std::sync::Arc::new(mux::pane::TmuxCopySelection {
                                endpoints:[column(range.start.x),(range.start.y-stable_top) as i64,column(range.end.x),(range.end.y-stable_top) as i64,i64::from(self.rectangular),1,0,i64::from(!self.search_selection)],
                                plain:Vec::new(),styled:Vec::new(),
                            }));
                            for pass in [2,3] {
                                for (line_idx,line) in lines.iter().enumerate() {
                                    if let Err(err)=self.render_line(stable_top,line_idx,line,pass) {self.error.replace(err);return;}
                                }
                            }
                        }
                    }
                    if let Some(selection) = self.copy_selection.clone() {
                        for (pass, variant) in [(2, &selection.plain), (3, &selection.styled)] {
                            for (line_idx, line) in variant.iter().enumerate() {
                                if let Err(err) = self.render_line(stable_top, line_idx, line, pass) {
                                    self.error.replace(err); return;
                                }
                            }
                        }
                    }
                    for (line_idx, line) in lines.iter().enumerate() {
                        if let Err(err) = self.render_line(stable_top, line_idx, line, 0) {
                            self.error.replace(err);
                            return;
                        }
                        if self.copy_scroll_position.is_some()
                            && self.cursor.visibility == termwiz::surface::CursorVisibility::Visible
                            && self.cursor.y == stable_top + line_idx as isize {
                            if let Err(err) = self.render_line(stable_top, line_idx, line, 1) {
                                self.error.replace(err);
                                return;
                            }
                        }
                    }
                }
            }

            pos.pane.with_lines_mut(stable_range.clone(), &mut render);
            if buffered_scroll { scroll_position = render.copy_scroll_position; cursor_rect = render.cursor_rect; copy_selection = render.copy_selection.clone(); }
            if let Some(error) = render.error.take() {
                return Err(error).context("error while calling with_lines_mut");
            }
        }

        if let Some((position, rows)) = scroll_position {
            let now = Instant::now();
            let height = self.render_metrics.cell_size.height as f32;
            let top = self.dimensions.pixel_height as f32 / -2. + top_pixel_y + pos.top as f32 * height;
            let bottom = top + rows as f32 * height;
            let geometry = (pos.left, pos.top, pos.width, pos.height, self.dimensions.pixel_width, self.dimensions.pixel_height);
            let generation = (self.quad_generation, self.shape_generation, self.config.generation());
            let left = self.dimensions.pixel_width as f32 / -2. + padding_left + border.left.get() as f32 + pos.left as f32 * self.render_metrics.cell_size.width as f32;
            let cell_width = self.render_metrics.cell_size.width as f32;
            let animated = {
                let mut state = self.pane_state(pane_id);
                state.copy_scroll.get_or_insert_with(|| super::copyscroll::CopyScroll::new(position, geometry, generation, now))
                    .paint(position, scroll_buffer, geometry, generation, now, height, top, bottom, layers, cursor_rect,
                        copy_selection.map(|s| (s.endpoints, plain_buffer, styled_buffer)),
                        left, cell_width)?
            };
            if let Some(rect) = cursor_rect {
                cursor_buffer.apply_rect_clipped(layers, 0., rect, true);
            }
            if animated {
                self.update_next_frame_time(Some(now + std::time::Duration::from_millis(8)));
            }
        } else if buffered_scroll {
            // Copy mode ended while this frame was being assembled.
            self.pane_state(pane_id).copy_scroll = None;
            scroll_buffer.apply_to(layers)?;
        }

        /*
        if let Some(zone) = zone {
            // TODO: render a thingy to jump to prior prompt
        }
        */
        metrics::histogram!("paint_pane.lines").record(start.elapsed());
        log::trace!("lines elapsed {:?}", start.elapsed());

        Ok(())
    }

    pub fn build_pane(&mut self, pos: &PositionedPane) -> anyhow::Result<ComputedElement> {
        // First compute the bounds for the pane background

        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;
        let (padding_left, padding_top) = self.padding_left_top();
        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height()?
        } else {
            0.
        };
        let (top_bar_height, _bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };

        let border = self.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        // We want to fill out to the edges of the splits
        let (x, width_delta) = if pos.left == 0 {
            (
                0.,
                padding_left + border.left.get() as f32 + (cell_width / 2.0),
            )
        } else {
            (
                padding_left + border.left.get() as f32 - (cell_width / 2.0)
                    + (pos.left as f32 * cell_width),
                cell_width,
            )
        };

        let (y, height_delta) = if pos.top == 0 {
            (
                (top_pixel_y - padding_top),
                padding_top + (cell_height / 2.0),
            )
        } else {
            (
                top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
                cell_height,
            )
        };

        let background_rect = euclid::rect(
            x,
            y,
            // Go all the way to the right edge if we're right-most
            if pos.left + pos.width >= self.terminal_size.cols as usize {
                self.dimensions.pixel_width as f32 - x
            } else {
                (pos.width as f32 * cell_width) + width_delta
            },
            // Go all the way to the bottom if we're bottom-most
            if pos.top + pos.height >= self.terminal_size.rows as usize {
                self.dimensions.pixel_height as f32 - y
            } else {
                (pos.height as f32 * cell_height) + height_delta as f32
            },
        );

        // Bounds for the terminal cells
        let content_rect = euclid::rect(
            padding_left + border.left.get() as f32 - (cell_width / 2.0)
                + (pos.left as f32 * cell_width),
            top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
            pos.width as f32 * cell_width,
            pos.height as f32 * cell_height,
        );

        let palette = pos.pane.palette();

        // TODO: visual bell background layer
        // TODO: scrollbar

        Ok(ComputedElement {
            item_type: None,
            zindex: 0,
            bounds: background_rect,
            border: PixelDimension::default(),
            border_rect: background_rect,
            border_corners: None,
            colors: ElementColors {
                border: BorderColor::default(),
                bg: if self.window_background.is_empty() {
                    palette
                        .background
                        .to_linear()
                        .mul_alpha(self.config.window_background_opacity)
                        .into()
                } else {
                    InheritableColor::Inherited
                },
                text: InheritableColor::Inherited,
            },
            hover_colors: None,
            padding: background_rect,
            content_rect,
            baseline: 1.0,
            content: ComputedElementContent::Children(vec![]),
        })
    }
}
