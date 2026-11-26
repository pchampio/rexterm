use crate::termwindow::{RenderFrame, TermWindowNotif};
use ::window::bitmaps::atlas::OutOfTextureSpace;
use ::window::WindowOps;
use anyhow::Context;
use smol::Timer;
use std::time::{Duration, Instant};
use wezterm_font::ClearShapeCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowImage {
    Yes,
    Scale(usize),
    No,
}

impl crate::TermWindow {
    pub fn paint_impl(&mut self, frame: &mut RenderFrame) {
        self.num_frames += 1;
        // If nothing on screen needs animating, then we can avoid
        // invalidating as frequently
        *self.has_animation.borrow_mut() = None;
        self.status_animation.set(false);
        // Start with the assumption that we should allow images to render
        self.allow_images = AllowImage::Yes;

        let start = Instant::now();

        {
            let diff = start.duration_since(self.last_fps_check_time);
            if diff > Duration::from_secs(1) {
                let seconds = diff.as_secs_f32();
                self.fps = self.num_frames as f32 / seconds;
                self.num_frames = 0;
                self.last_fps_check_time = start;
            }
        }

        'pass: for pass in 0.. {
            match self.paint_pass() {
                Ok(_) => match self.render_state.as_mut().unwrap().allocated_more_quads() {
                    Ok(allocated) => {
                        if !allocated {
                            break 'pass;
                        }
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                    }
                    Err(err) => {
                        log::error!("{:#}", err);
                        break 'pass;
                    }
                },
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        let result = if pass == 0 {
                            // Let's try clearing out the atlas and trying again
                            // self.clear_texture_atlas()
                            log::trace!("recreate_texture_atlas");
                            self.recreate_texture_atlas(Some(current_size))
                        } else {
                            log::trace!("grow texture atlas to {}", size);
                            self.recreate_texture_atlas(Some(size))
                        };
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();

                        if let Err(err) = result {
                            self.allow_images = match self.allow_images {
                                AllowImage::Yes => AllowImage::Scale(2),
                                AllowImage::Scale(2) => AllowImage::Scale(4),
                                AllowImage::Scale(4) => AllowImage::Scale(8),
                                AllowImage::Scale(8) => AllowImage::No,
                                AllowImage::No | _ => {
                                    log::error!(
                                        "Failed to {} texture: {}",
                                        if pass == 0 { "clear" } else { "resize" },
                                        err
                                    );
                                    break 'pass;
                                }
                            };

                            log::info!(
                                "Not enough texture space ({:#}); \
                                     will retry render with {:?}",
                                err,
                                self.allow_images,
                            );
                        }
                    } else if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                        self.shape_generation += 1;
                        self.shape_cache.borrow_mut().clear();
                        self.line_to_ele_shape_cache.borrow_mut().clear();
                    } else {
                        log::error!("paint_pass failed: {:#}", err);
                        break 'pass;
                    }
                }
            }
        }
        log::debug!("paint_impl before call_draw elapsed={:?}", start.elapsed());

        self.call_draw(frame).ok();
        self.last_frame_duration = start.elapsed();
        log::debug!(
            "paint_impl elapsed={:?}, fps={}",
            self.last_frame_duration,
            self.fps
        );
        let count=mux::Mux::get().get_window(self.mux_window_id).map(|w|w.count_tabs()).unwrap_or(0);
        if count!=self.last_painted_tab_count {
            self.last_painted_tab_count=count;
            log::debug!(target:"tab_latency","TAB_PAINTED {count}");
        }
        metrics::histogram!("gui.paint.impl").record(self.last_frame_duration);
        metrics::histogram!("gui.paint.impl.rate").record(1.);

        // If self.has_animation is some, then the last render detected
        // image attachments with multiple frames, so we also need to
        // invalidate the viewport when the next frame is due
        if self.focused.is_some() || self.status_animation.get() {
            if let Some(next_due) = *self.has_animation.borrow() {
                let prior = self.scheduled_animation.borrow_mut().take();
                match prior {
                    Some(prior) if prior <= next_due => {
                        // Already due before that time
                    }
                    _ => {
                        self.scheduled_animation.borrow_mut().replace(next_due);
                        let window = self.window.clone().take().unwrap();
                        promise::spawn::spawn(async move {
                            Timer::at(next_due).await;
                            let win = window.clone();
                            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                                tw.scheduled_animation.borrow_mut().take();
                                if tw.tab_bar.next_progress_frame_due().is_some() {
                                    tw.update_title_post_status();
                                }
                                win.invalidate();
                            })));
                        })
                        .detach();
                    }
                }
            }
        }
    }

    pub fn paint_modal(&mut self) -> anyhow::Result<()> {
        if let Some(modal) = self.get_modal() {
            for computed in modal.computed_element(self)?.iter() {
                let mut ui_items = computed.ui_items();

                let gl_state = self.render_state.as_ref().unwrap();
                self.render_element(&computed, gl_state, None)?;

                self.ui_items.append(&mut ui_items);
            }
        }

        Ok(())
    }

    pub fn paint_pass(&mut self) -> anyhow::Result<()> {
        {
            let gl_state = self.render_state.as_ref().unwrap();
            for layer in gl_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
        }

        // Clear out UI item positions; we'll rebuild these as we render
        self.ui_items.clear();

        let panes = self.get_panes_to_render();
        let focused = self.focused.is_some();
        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;

        let start = Instant::now();
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(0)
            .context("layer_for_zindex(0)")?;
        let mut layers = layer.quad_allocator();

        log::trace!("quad map elapsed {:?}", start.elapsed());
        metrics::histogram!("quad.map").record(start.elapsed());

        let mut paint_terminal_background = false;

        // Render the full window background
        match (self.window_background.is_empty(), self.allow_images) {
            (false, AllowImage::Yes | AllowImage::Scale(_)) => {
                let bg_color = self.palette().background.to_linear();

                let top = panes
                    .iter()
                    .find(|p| p.is_active)
                    .map(|p| match self.get_viewport(p.pane.pane_id()) {
                        Some(top) => top,
                        None => p.pane.get_dimensions().physical_top,
                    })
                    .unwrap_or(0);

                let loaded_any = self
                    .render_backgrounds(bg_color, top)
                    .context("render_backgrounds")?;

                if !loaded_any {
                    // Either there was a problem loading the background(s)
                    // or they haven't finished loading yet.
                    // Use the regular terminal background until that changes.
                    paint_terminal_background = true;
                }
            }
            _ if window_is_transparent => {
                // Avoid doubling up the background color: the panes
                // will render out through the padding so there
                // should be no gaps that need filling in
            }
            _ => {
                paint_terminal_background = true;
            }
        }

        if paint_terminal_background {
            // Regular window background color
            let background = if panes.len() == 1 {
                // If we're the only pane, use the pane's palette
                // to draw the padding background
                panes[0].pane.palette().background
            } else {
                self.palette().background
            }
            .to_linear()
            .mul_alpha(self.config.window_background_opacity);

            self.filled_rectangle(
                &mut layers,
                0,
                euclid::rect(
                    0.,
                    0.,
                    self.dimensions.pixel_width as f32,
                    self.dimensions.pixel_height as f32,
                ),
                background,
            )
            .context("filled_rectangle for window background")?;
        }

        let floating_panes = self.get_floating_panes_to_render();
        // One more sublayer above the popups for the zoom out animation.
        self.render_state
            .as_ref()
            .unwrap()
            .retain_floating_sublayers(floating_panes.len() + 1);
        let floating_layers = (0..floating_panes.len())
            .map(|order| {
                self.render_state
                    .as_ref()
                    .unwrap()
                    .layer_for_zindex_and_sublayer(2, order)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        // A pane overlay (click copy, quick select) shows its indicator in the
        // pane status row, beneath the continuous window tab strip.
        let pane_overlay = panes
            .iter()
            .any(|pos| self.pane_state(pos.pane.pane_id()).overlay.is_some());
        // Zoom in: the pane (tiled or popup) grows from its place to the
        // whole tab. Zoom out: it shrinks back from the whole tab.
        const ZOOM_DURATION: std::time::Duration = std::time::Duration::from_millis(150);
        {
            let tab = mux::Mux::get().get_active_tab_for_window(self.mux_window_id);
            let zoomed = tab.as_ref().is_some_and(|t| t.get_zoomed_pane().is_some());
            let tab_id = tab.as_ref().map(|t| t.tab_id());
            if let (Some(tab_id), Some((last_tab, was_zoomed, places))) = (tab_id, self.last_layout.as_ref()) {
                if *last_tab == tab_id && zoomed && !was_zoomed && panes.len() == 1 {
                    let id = panes[0].pane.pane_id();
                    let full = (panes[0].left, panes[0].top, panes[0].width, panes[0].height);
                    if let Some(from) = places.get(&id).copied().filter(|from| *from != full) {
                        self.zoom_animation = Some((id, from, Instant::now()));
                    }
                }
            }
            if !zoomed { self.zoom_animation = None; }
            let place = |p: &mux::tab::PositionedPane| (p.left, p.top, p.width, p.height);
            if let (Some(tab_id), Some((last_tab, id, full))) = (tab_id, self.last_zoomed) {
                if last_tab == tab_id && !zoomed {
                    let to = panes.iter().chain(floating_panes.iter()).find(|p| p.pane.pane_id() == id).map(place);
                    if to.is_some_and(|to| to != full) {
                        self.unzoom_animation = Some((id, full, Instant::now()));
                    }
                }
            }
            if zoomed { self.unzoom_animation = None; }
            self.last_zoomed = match (tab_id, zoomed && panes.len() == 1) {
                (Some(tab_id), true) => Some((tab_id, panes[0].pane.pane_id(), place(&panes[0]))),
                _ => None,
            };
            if let Some(tab_id) = tab_id {
                let places = panes.iter().chain(floating_panes.iter()).map(|p| (p.pane.pane_id(), place(p))).collect();
                self.last_layout = Some((tab_id, zoomed, places));
            }
            if self.zoom_animation.is_some_and(|(_, _, start)| start.elapsed() >= ZOOM_DURATION) {
                self.zoom_animation = None;
            }
            if self.unzoom_animation.is_some_and(|(_, _, start)| start.elapsed() >= ZOOM_DURATION) {
                self.unzoom_animation = None;
            }
        }
        let unzoom_id = self.unzoom_animation.map(|(id, _, _)| id);
        let mut unzoom_pane = None;
        let mut unzoom_popup = false;
        for mut pos in panes {
            // The tiled pane retains its mux selection while a floating pane
            // has focus. Render it as inactive so that its cursor and IME
            // preedit do not duplicate the focused floating pane's input.
            if floating_panes.iter().any(|p| p.is_active) {
                pos.is_active = false;
            }
            if pos.is_active {
                self.update_text_cursor(&pos);
                if focused {
                    pos.pane.advise_focus();
                    mux::Mux::get().record_focus_for_current_identity(pos.pane.pane_id());
                }
            }
            if let Some((_, from, start)) = self.zoom_animation.filter(|(id, _, _)| *id == pos.pane.pane_id()) {
                // Paint the zoomed pane, then show it through a rectangle that
                // grows from its old place, its text held at the bottom left
                // corner (a top pane's prompt goes down with its bottom edge).
                let mut buffer = crate::quad::HeapQuadAllocator::default();
                self.paint_pane(&pos, &mut crate::quad::TripleLayerQuadAllocator::Heap(&mut buffer)).context("paint_pane")?;
                let t = (start.elapsed().as_secs_f32() / ZOOM_DURATION.as_secs_f32()).min(1.);
                let t = 1. - (1. - t).powi(3);
                let cw = self.render_metrics.cell_size.width as f32;
                let ch = self.render_metrics.cell_size.height as f32;
                let (pad_left, pad_top) = self.padding_left_top();
                let border = self.get_os_border();
                let tab_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom { self.tab_bar_pixel_height().unwrap_or(0.) } else { 0. };
                let x0 = self.dimensions.pixel_width as f32 / -2. + pad_left + border.left.get() as f32;
                let y0 = self.dimensions.pixel_height as f32 / -2. + pad_top + border.top.get() as f32 + tab_height;
                let px = |(left, top, width, height): (usize, usize, usize, usize)| {
                    (x0 + left as f32 * cw, y0 + top as f32 * ch, x0 + (left + width) as f32 * cw, y0 + (top + height) as f32 * ch)
                };
                let (a, b) = (px(from), px((pos.left, pos.top, pos.width, pos.height)));
                let mix = |p: f32, q: f32| p + (q - p) * t;
                let rect = (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2), mix(a.3, b.3));
                log::debug!(target: "zoom_anim", "ZOOM_RECT {:.1} {:.1} {:.1} {:.1}", rect.0, rect.1, rect.2, rect.3);
                buffer.apply_moved_clipped(&mut layers, rect.0 - b.0, rect.3 - b.3, rect);
                self.update_next_frame_time(Some(Instant::now() + std::time::Duration::from_millis(8)));
                continue;
            }
            if unzoom_id == Some(pos.pane.pane_id()) {
                // Painted last, above the other panes (below).
                unzoom_pane = Some(pos);
                continue;
            }
            self.paint_pane(&pos, &mut layers).context("paint_pane")?;
        }

        let active_tab = mux::Mux::get().get_active_tab_for_window(self.mux_window_id);
        let remote_chrome = active_tab.as_ref().and_then(|t| t.get_remote_chrome());
        // tmux's border capture is only needed for the frames of tmux floating
        // panes; separators and the tab strip are drawn natively, so they
        // follow layout changes in the same frame.
        let tmux_float_frames = floating_panes
            .iter()
            .any(|p| p.pane.has_server_frame() && p.pane.popup_frame().is_none());
        let tmux_tab = self.tmux_domain_for_window().map(|(tab, _)| tab);
        let _ = &active_tab;
        if let Some(tab) = &tmux_tab {
            let chrome_layer = self.render_state.as_ref().unwrap().layer_for_zindex(1)?;
            let mut chrome_layers = chrome_layer.quad_allocator();
            if let (true, Some(chrome)) = (tmux_float_frames, &remote_chrome) {
                if let Some(local) = chrome.pane.downcast_ref::<mux::localpane::LocalPane>() {
                    local.set_tmux_strip_hidden(pane_overlay);
                }
                self.paint_pane(chrome, &mut chrome_layers)
                    .context("paint_tmux_chrome")?;
            }
            // A pane overlay (click copy, quick select) uses the status row.
            let strip_shown = !pane_overlay
                && self
                    .paint_tmux_window_strip(&mut chrome_layers)
                    .context("paint_tmux_window_strip")?;
            self.paint_tmux_separators(tab, strip_shown, &mut chrome_layers)
                .context("paint_tmux_separators")?;
        }
        let floating_count = floating_panes.len();
        for (floating_pane, float_layer) in floating_panes.into_iter().zip(floating_layers) {
            let mut floating_pane_layers = float_layer.quad_allocator();
            if floating_pane.is_active {
                self.update_text_cursor(&floating_pane);
                if focused {
                    floating_pane.pane.advise_focus();
                    mux::Mux::get().record_focus_for_current_identity(floating_pane.pane.pane_id());
                }
            }
            if unzoom_id == Some(floating_pane.pane.pane_id()) {
                unzoom_popup = true;
                unzoom_pane = Some(floating_pane);
                continue;
            }
            self.paint_pane(&floating_pane, &mut floating_pane_layers)
                .context("paint_pane")?;
            self.paint_floating_pane_border(floating_pane, &mut floating_pane_layers)
                .context("paint_floating_pane_border")?;
        }
        if let (Some(pos), Some((_, from, start))) = (unzoom_pane, self.unzoom_animation) {
            // Zoom out: a rectangle shrinks from the whole tab to the pane's
            // place, filled with its background, its text held at the bottom
            // left corner (a top pane's prompt goes up with its bottom edge).
            let layer = self.render_state.as_ref().unwrap().layer_for_zindex_and_sublayer(2, floating_count)?;
            let mut unzoom_layers = layer.quad_allocator();
            let mut buffer = crate::quad::HeapQuadAllocator::default();
            self.paint_pane(&pos, &mut crate::quad::TripleLayerQuadAllocator::Heap(&mut buffer)).context("paint_pane")?;
            let t = (start.elapsed().as_secs_f32() / ZOOM_DURATION.as_secs_f32()).min(1.);
            let t = 1. - (1. - t).powi(3);
            let cw = self.render_metrics.cell_size.width as f32;
            let ch = self.render_metrics.cell_size.height as f32;
            let (pad_left, pad_top) = self.padding_left_top();
            let border = self.get_os_border();
            let tab_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom { self.tab_bar_pixel_height().unwrap_or(0.) } else { 0. };
            let (half_w, half_h) = (self.dimensions.pixel_width as f32 / 2., self.dimensions.pixel_height as f32 / 2.);
            let x0 = -half_w + pad_left + border.left.get() as f32;
            let y0 = -half_h + pad_top + border.top.get() as f32 + tab_height;
            let px = |(left, top, width, height): (usize, usize, usize, usize)| {
                (x0 + left as f32 * cw, y0 + top as f32 * ch, x0 + (left + width) as f32 * cw, y0 + (top + height) as f32 * ch)
            };
            let (a, b) = (px(from), px((pos.left, pos.top, pos.width, pos.height)));
            let mix = |p: f32, q: f32| p + (q - p) * t;
            let rect = (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2), mix(a.3, b.3));
            log::debug!(target: "zoom_anim", "UNZOOM_RECT {:.1} {:.1} {:.1} {:.1}", rect.0, rect.1, rect.2, rect.3);
            let background = pos.pane.palette().background.to_linear()
                .mul_alpha(self.config.window_background_opacity);
            // A popup's frame is one cell outside it: cover that cell too.
            let (mx, my) = if unzoom_popup { (cw, ch) } else { (0., 0.) };
            self.filled_rectangle(&mut unzoom_layers, 0,
                euclid::rect(rect.0 - mx + half_w, rect.1 - my + half_h, rect.2 - rect.0 + 2. * mx, rect.3 - rect.1 + 2. * my), background)
                .context("filled_rectangle for zoom out")?;
            buffer.apply_moved_clipped(&mut unzoom_layers, rect.0 - b.0, rect.3 - b.3, rect);
            if unzoom_popup {
                // The popup's own frame and title, around the rectangle.
                let mut frame = pos.clone();
                frame.left = ((rect.0 - x0) / cw).round().max(0.) as usize;
                frame.top = ((rect.1 - y0) / ch).round().max(0.) as usize;
                frame.width = (((rect.2 - x0) / cw).round() as usize).saturating_sub(frame.left).max(1);
                frame.height = (((rect.3 - y0) / ch).round() as usize).saturating_sub(frame.top).max(1);
                self.paint_floating_pane_border(frame, &mut unzoom_layers)
                    .context("paint_floating_pane_border for zoom out")?;
            }
            self.update_next_frame_time(Some(Instant::now() + std::time::Duration::from_millis(8)));
        }

        if let Some(pane) = self.get_active_pane_or_overlay() {
            let splits = self.get_splits();
            for split in splits.iter().filter(|_| tmux_tab.is_none()) {
                self.paint_split(&mut layers, split, &pane)
                    .context("paint_split")?;
            }
        }

        if self.show_tab_bar {
            // Keep tab controls above floating backgrounds/borders at the
            // viewport edge, matching UI hit testing.
            let tab_layer = self.render_state.as_ref().unwrap().layer_for_zindex(3)?;
            let mut tab_layers = tab_layer.quad_allocator();
            self.paint_tab_bar(&mut tab_layers)
                .context("paint_tab_bar")?;
        }

        // Draw mode chrome after separators and tabs, in their existing row.
        // Its text never replaces the last line of a terminal pane.
        // A tiled pane's bar on a separator goes under the popups (layer 1,
        // the popups are on 2); in the tab bar row it stays above the tab
        // bar (popups never cover that row). A popup's own bar is on top.
        let mode_layer = self.render_state.as_ref().unwrap().layer_for_zindex(4)?;
        let mut mode_layers = mode_layer.quad_allocator();
        let under_layer = self.render_state.as_ref().unwrap().layer_for_zindex(1)?;
        let mut under_layers = under_layer.quad_allocator();
        for pos in self.get_panes_to_render().iter() {
            let in_tab_bar_row = pos.top + pos.height >= self.terminal_size.rows && self.show_tab_bar && self.config.tab_bar_at_bottom;
            if in_tab_bar_row {
                self.paint_pane_mode_status(pos, &mut mode_layers)?;
            } else {
                self.paint_pane_mode_status(pos, &mut under_layers)?;
            }
        }
        for pos in self.get_floating_panes_to_render().iter() {
            self.paint_pane_mode_status(pos, &mut mode_layers)?;
        }
        drop(under_layers);
        drop(mode_layers);

        self.paint_window_borders(&mut layers)
            .context("paint_window_borders")?;
        drop(layers);
        self.paint_strip_hover().context("paint_strip_hover")?;
        self.paint_agent_panel().context("paint_agent_panel")?;
        self.paint_split_drag().context("paint_split_drag")?;
        self.paint_window_overview().context("paint_window_overview")?;
        self.paint_modal().context("paint_modal")?;

        Ok(())
    }
}
