use crate::selection::{Selection, SelectionCoordinate, SelectionMode, SelectionRange, SelectionX};
use ::window::WindowOps;
use mux::pane::{Pane, PaneId};
use std::cell::RefMut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::surface::Line;
use wezterm_term::StableRowIndex;

impl super::TermWindow {
    pub fn selection(&self, pane_id: PaneId) -> RefMut<'_, Selection> {
        RefMut::map(self.pane_state(pane_id), |state| &mut state.selection)
    }

    /// Returns the selection region as a series of Line
    pub fn selection_lines(&self, pane: &Arc<dyn Pane>) -> Vec<Line> {
        let mut result = vec![];

        let rectangular = self.selection(pane.pane_id()).rectangular;
        let command_selection = self.pane_state(pane.pane_id()).overlay.as_ref()
            .and_then(|o| o.pane.downcast_ref::<crate::overlay::CopyOverlay>()).is_some_and(|o|o.is_command_selection());
        if let Some(sel) = self
            .selection(pane.pane_id())
            .range
            .as_ref()
            .map(|r| r.normalize())
        {
            let mut last_was_wrapped = false;
            let first_row = sel.rows().start;
            let last_row = sel.rows().end;

            for line in pane.get_logical_lines(sel.rows()) {
                if result.is_empty() || !last_was_wrapped {
                    result.push(Line::with_width(0, line.physical_lines[0].current_seqno()));
                }
                let last_idx = line.physical_lines.len().saturating_sub(1);
                for (idx, phys) in line.physical_lines.iter().enumerate() {
                    let this_row = line.first_row + idx as StableRowIndex;
                    if this_row >= first_row && this_row < last_row {
                        let last_phys_idx = phys.len().saturating_sub(1);
                        let cols = sel.cols_for_row(this_row, rectangular);
                        let cols = if command_selection {crate::overlay::copy::command_selection_columns(phys,cols)} else {cols};
                        let last_col_idx = cols.end.saturating_sub(1).min(last_phys_idx);
                        let mut col_span = phys.columns_as_line(cols);
                        let seqno = col_span.current_seqno();
                        // Only trim trailing whitespace if we are the last line
                        // in a wrapped sequence
                        if idx == last_idx {
                            col_span.prune_trailing_blanks(seqno);
                        }

                        result
                            .last_mut()
                            .map(|line| line.append_line(col_span, seqno));

                        last_was_wrapped = last_col_idx == last_phys_idx
                            && phys
                                .get_cell(last_col_idx)
                                .map(|c| c.attrs().wrapped())
                                .unwrap_or(false);
                    }
                }
            }
        }

        result
    }

    /// Returns the selection text only
    pub fn selection_text(&self, pane: &Arc<dyn Pane>) -> String {
        let mut s = String::new();
        let rectangular = self.selection(pane.pane_id()).rectangular;
        let command_selection = self.pane_state(pane.pane_id()).overlay.as_ref()
            .and_then(|o| o.pane.downcast_ref::<crate::overlay::CopyOverlay>()).is_some_and(|o|o.is_command_selection());
        let joined = {
            let sel = self.selection(pane.pane_id());
            sel.joined_range.is_some() && sel.joined_range == sel.range
        };
        if let Some(sel) = self
            .selection(pane.pane_id())
            .range
            .as_ref()
            .map(|r| r.normalize())
        {
            let mut last_was_wrapped = false;
            let first_row = sel.rows().start;
            let last_row = sel.rows().end;

            for line in pane.get_logical_lines(sel.rows()) {
                if !s.is_empty() && !last_was_wrapped && !joined {
                    s.push('\n');
                }
                let last_idx = line.physical_lines.len().saturating_sub(1);
                for (idx, phys) in line.physical_lines.iter().enumerate() {
                    let this_row = line.first_row + idx as StableRowIndex;
                    if this_row >= first_row && this_row < last_row {
                        let last_phys_idx = phys.len().saturating_sub(1);
                        let cols = sel.cols_for_row(this_row, rectangular);
                        let cols = if command_selection {crate::overlay::copy::command_selection_columns(phys,cols)} else {cols};
                        let last_col_idx = cols.end.saturating_sub(1).min(last_phys_idx);
                        let col_span = phys.columns_as_str(cols);
                        // Only trim trailing whitespace if we are the last line
                        // in a wrapped sequence
                        if idx == last_idx {
                            s.push_str(col_span.trim_end());
                        } else {
                            s.push_str(&col_span);
                        }

                        last_was_wrapped = last_col_idx == last_phys_idx
                            && phys
                                .get_cell(last_col_idx)
                                .map(|c| c.attrs().wrapped())
                                .unwrap_or(false);
                    }
                }
            }
        }

        s
    }

    /// True while based_copymode's click copy overlay covers this pane.
    pub fn is_click_copy_pane(&self, pane_id: PaneId) -> bool {
        self.pane_state(pane_id)
            .overlay
            .as_ref()
            .and_then(|o| {
                o.pane
                    .downcast_ref::<crate::overlay::quickselect::QuickSelectOverlay>()
                    .map(|q| q.is_click_copy())
            })
            .unwrap_or(false)
    }

    /// Imprecise drag selections often start one letter into a word or stop
    /// one letter before its end. In click copy, extend such an edge to the
    /// word boundary, as macOS does. Only a word character that
    /// completes a word is added; whitespace and separators never are.
    pub fn snap_click_copy_selection(&mut self, pane: &Arc<dyn Pane>) {
        let pane_id = pane.pane_id();
        let (range, rectangular) = {
            let selection = self.selection(pane_id);
            (selection.range, selection.rectangular)
        };
        let Some(range) = range else { return };
        if rectangular {
            return;
        }
        let boundary = self.config.selection_word_boundary.clone();
        let word = |line: &Line, x: usize| {
            line.get_cell(x)
                .map(|cell| {
                    let text = cell.str();
                    !text.is_empty()
                        && !text.chars().all(char::is_whitespace)
                        && !text.chars().any(|c| boundary.contains(c))
                })
                .unwrap_or(false)
        };
        let line_at = |y: StableRowIndex| pane.get_lines(y..y + 1).1.into_iter().next();
        // The pointer picks the nearest cell edge, so a pointer anywhere on
        // the first (or last) letter leaves out up to two cells of the word:
        // the letter under the pointer plus, from its far half, one more.
        // normalize() orders rows only: a right-to-left drag on one row
        // still has its start to the right of its end.
        let mut snapped = range.normalize();
        if snapped.start.y == snapped.end.y {
            if let (SelectionX::Cell(a), SelectionX::Cell(b)) = (snapped.start.x, snapped.end.x) {
                if a > b {
                    std::mem::swap(&mut snapped.start, &mut snapped.end);
                }
            }
        }
        let ordered = snapped;
        if let SelectionX::Cell(x) = snapped.start.x {
            if let Some(line) = line_at(snapped.start.y) {
                if word(&line, x) {
                    for missing in 1..=2usize.min(x) {
                        let edge = x - missing;
                        if !word(&line, edge) {
                            break;
                        }
                        if edge == 0 || !word(&line, edge - 1) {
                            snapped.start.x = SelectionX::Cell(edge);
                            break;
                        }
                    }
                }
            }
        }
        if let SelectionX::Cell(x) = snapped.end.x {
            if x < usize::MAX - 3 {
                if let Some(line) = line_at(snapped.end.y) {
                    if word(&line, x) {
                        for missing in 1..=2 {
                            let edge = x + missing;
                            if !word(&line, edge) {
                                break;
                            }
                            if !word(&line, edge + 1) {
                                snapped.end.x = SelectionX::Cell(edge);
                                break;
                            }
                        }
                    }
                }
            }
        }
        if snapped != ordered {
            self.selection(pane_id).range = Some(snapped);
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    /// Completes click copy on mouse release: snap, copy, close the overlay.
    /// The copied selection stays visible briefly as confirmation.
    pub fn finish_click_copy(&mut self, pane: &Arc<dyn Pane>) {
        self.snap_click_copy_selection(pane);
        let text = self.selection_text(pane);
        if !text.is_empty() {
            self.copy_to_clipboard(
                config::keyassignment::ClipboardCopyDestination::ClipboardAndPrimarySelection,
                text,
            );
        }
        let pane_id = pane.pane_id();
        self.cancel_overlay_for_pane(pane_id);
        let Some(window) = self.window.clone() else { return };
        promise::spawn::spawn(async move {
            smol::Timer::after(std::time::Duration::from_millis(250)).await;
            window.notify(super::TermWindowNotif::Apply(Box::new(move |term_window| {
                if let Some(pane) = mux::Mux::get().get_pane(pane_id) {
                    term_window.clear_selection(&pane);
                }
            })));
        })
        .detach();
    }

    /// Typing into a pane clears its selection, as in most terminals and in
    /// tmux: a highlight left behind (after a copy) never stays on screen.
    pub fn clear_selection_for_input(&mut self, pane: &Arc<dyn Pane>) {
        if !self.selection(pane.pane_id()).is_empty() {
            self.clear_selection(pane);
        }
    }

    pub fn clear_selection(&mut self, pane: &Arc<dyn Pane>) {
        let mut selection = self.selection(pane.pane_id());
        selection.clear();
        selection.seqno = pane.get_current_seqno();
        self.window.as_ref().unwrap().invalidate();
    }

    pub fn extend_selection_at_mouse_cursor(&mut self, mode: SelectionMode, pane: &Arc<dyn Pane>) {
        self.selection(pane.pane_id()).seqno = pane.get_current_seqno();
        let (position, y) = match self.pane_state(pane.pane_id()).mouse_terminal_coords {
            Some(coords) => coords,
            None => return,
        };
        let x = position.column;
        match mode {
            SelectionMode::Cell | SelectionMode::Block => {
                // Origin is the cell in which the selection action started. E.g. the cell
                // that had the mouse over it when the left mouse button was pressed
                let origin = self
                    .selection(pane.pane_id())
                    .origin
                    .unwrap_or(SelectionCoordinate::x_y(x, y));
                self.selection(pane.pane_id()).origin = Some(origin);
                self.selection(pane.pane_id()).rectangular = mode == SelectionMode::Block;

                // Compute the start and end horizontall cell of the selection.
                // The selection extent depends on the mouse cursor position in relation
                // to the origin.
                let (start_x, end_x) = if mode == SelectionMode::Block {
                    if x >= origin.x {
                        // If the selection is extending forwards from the origin,
                        // it includes the origin
                        (origin.x, SelectionX::Cell(x).saturating_sub(1))
                    } else {
                        // If the selection is extending backwards from the origin,
                        // it doesn't include the origin
                        (origin.x.saturating_sub(1), SelectionX::Cell(x))
                    }
                } else {
                    if (x >= origin.x && y == origin.y) || y > origin.y {
                        // If the selection is extending forwards from the origin, it includes the
                        // origin and doesn't include the cell under the cursor. Note that the
                        // reported cell here is offset by -50% from the real cell you see on the
                        // screen, so this causes a visual cell on the screen to be selected when
                        // the mouse moves over 50% of its width, which effectively means the next
                        // cell is being reported here, hence it's excluded
                        (origin.x, SelectionX::Cell(x).saturating_sub(1))
                    } else {
                        // If the selection is extending backwards from the origin, it doesn't
                        // include the origin and includes the cell under the cursor, which has
                        // the same effect as described above when going backwards
                        (origin.x.saturating_sub(1), SelectionX::Cell(x))
                    }
                };

                self.selection(pane.pane_id()).range =
                    if mode == SelectionMode::Block && origin.x == x {
                        // Ignore rectangle selections with a width of zero
                        None
                    } else if origin.x != x || origin.y != y {
                        // Only considers a selection if the cursor moved from the origin point
                        Some(
                            SelectionRange::start(SelectionCoordinate {
                                x: start_x,
                                y: origin.y,
                            })
                            .extend(SelectionCoordinate { x: end_x, y }),
                        )
                    } else {
                        None
                    };
                // Show the snapped word edges while dragging, as macOS does.
                // origin stays unsnapped, so each motion recomputes from it.
                if mode == SelectionMode::Cell && self.is_click_copy_pane(pane.pane_id()) {
                    self.snap_click_copy_selection(pane);
                }
            }
            SelectionMode::Word => {
                let end_word = SelectionRange::word_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = self
                    .selection(pane.pane_id())
                    .origin
                    .clone()
                    .unwrap_or(end_word.start);
                let start_word = SelectionRange::word_around(start_coord, &**pane);

                let selection_range = start_word.extend_with(end_word);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
            }
            SelectionMode::Line => {
                let end_line = SelectionRange::line_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = self
                    .selection(pane.pane_id())
                    .origin
                    .clone()
                    .unwrap_or(end_line.start);
                let start_line = SelectionRange::line_around(start_coord, &**pane);

                let selection_range = start_line.extend_with(end_line);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
            }
            SelectionMode::SemanticZone => {
                let end_word = SelectionRange::zone_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = self
                    .selection(pane.pane_id())
                    .origin
                    .clone()
                    .unwrap_or(end_word.start);
                let start_word = SelectionRange::zone_around(start_coord, &**pane);

                let selection_range = start_word.extend_with(end_word);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
            }
        }

        // Past the top or bottom of the pane the viewport keeps scrolling
        // while the button is held (a timer, not mouse motion), faster the
        // further past, as tmux's copy mode does.
        let dims = pane.get_dimensions();
        let ch = self.render_metrics.cell_size.height.max(1) as f32;
        let past = if position.row <= 0 && position.y_pixel_offset < 0 {
            Some((true, -position.y_pixel_offset as f32 / ch))
        } else if position.row >= dims.viewport_rows as i64 {
            let rows = (position.row - dims.viewport_rows as i64) as f32;
            Some((false, rows + position.y_pixel_offset.max(0) as f32 / ch))
        } else {
            None
        };
        self.update_selection_autoscroll(pane, mode, past);

        self.window.as_ref().unwrap().invalidate();
    }

    fn update_selection_autoscroll(
        &mut self,
        pane: &Arc<dyn Pane>,
        mode: SelectionMode,
        past: Option<(bool, f32)>,
    ) {
        let Some((up, rows_past)) = past else {
            if let Some(s) = self.selection_autoscroll.take() {
                s.active.store(false, Ordering::Relaxed);
            }
            return;
        };
        let rate = SelectionAutoScroll::rate(rows_past);
        if let Some(s) = self.selection_autoscroll.as_mut() {
            if s.pane_id == pane.pane_id() {
                s.up = up;
                s.rate = rate;
                s.mode = mode;
                return;
            }
            s.active.store(false, Ordering::Relaxed);
        }
        let active = Arc::new(AtomicBool::new(true));
        self.selection_autoscroll = Some(SelectionAutoScroll {
            pane_id: pane.pane_id(),
            mode,
            up,
            rate,
            // Start with one row at once, as tmux does.
            carry: 1.,
            last: Instant::now(),
            active: Arc::clone(&active),
        });
        let Some(window) = self.window.clone() else { return };
        promise::spawn::spawn(async move {
            while active.load(Ordering::Relaxed) {
                window.notify(super::TermWindowNotif::Apply(Box::new(|tw| {
                    tw.selection_autoscroll_tick();
                })));
                smol::Timer::after(Duration::from_millis(16)).await;
            }
        })
        .detach();
    }

    fn selection_autoscroll_tick(&mut self) {
        let Some(mut s) = self.selection_autoscroll.clone() else { return };
        let pane = mux::Mux::get().get_pane(s.pane_id);
        let held = self
            .current_mouse_buttons
            .iter()
            .any(|b| *b == ::window::MousePress::Left);
        let Some(pane) = pane.filter(|_| held) else {
            s.active.store(false, Ordering::Relaxed);
            self.selection_autoscroll = None;
            return;
        };
        let now = Instant::now();
        s.carry += s.rate * now.duration_since(s.last).as_secs_f32();
        s.last = now;
        let rows = s.carry.floor();
        s.carry -= rows;
        self.selection_autoscroll = Some(s.clone());
        if rows < 1. {
            return;
        }
        let dims = pane.get_dimensions();
        let top = self.get_viewport(s.pane_id).unwrap_or(dims.physical_top);
        let new_top = if s.up {
            top - rows as StableRowIndex
        } else {
            top + rows as StableRowIndex
        };
        self.set_viewport(s.pane_id, Some(new_top), dims);
        let dims = pane.get_dimensions();
        let top = self.get_viewport(s.pane_id).unwrap_or(dims.physical_top);
        // The selection follows to the row at the pane's edge.
        let edge = if s.up {
            top
        } else {
            top + dims.viewport_rows as StableRowIndex - 1
        };
        let coords = self.pane_state(s.pane_id).mouse_terminal_coords.clone();
        if let Some((position, _)) = coords {
            self.pane_state(s.pane_id)
                .mouse_terminal_coords
                .replace((position, edge));
            self.extend_selection_at_mouse_cursor(s.mode, &pane);
        }
    }

    pub fn select_text_at_mouse_cursor(&mut self, mode: SelectionMode, pane: &Arc<dyn Pane>) {
        let (x, y) = match self.pane_state(pane.pane_id()).mouse_terminal_coords {
            Some(coords) => (coords.0.column, coords.1),
            None => return,
        };
        match mode {
            SelectionMode::Line => {
                let start = SelectionCoordinate::x_y(x, y);
                let selection_range = SelectionRange::line_around(start, &**pane);

                self.selection(pane.pane_id()).origin = Some(start);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
            }
            SelectionMode::Word => {
                let selection_range =
                    SelectionRange::word_around(SelectionCoordinate::x_y(x, y), &**pane);

                self.selection(pane.pane_id()).origin = Some(selection_range.start);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
            }
            SelectionMode::SemanticZone => {
                let selection_range =
                    SelectionRange::zone_around(SelectionCoordinate::x_y(x, y), &**pane);

                self.selection(pane.pane_id()).origin = Some(selection_range.start);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
            }
            SelectionMode::Cell | SelectionMode::Block => {
                self.selection(pane.pane_id())
                    .begin(SelectionCoordinate::x_y(x, y));
                self.selection(pane.pane_id()).rectangular = mode == SelectionMode::Block;
            }
        }

        self.selection(pane.pane_id()).seqno = pane.get_current_seqno();
        self.window.as_ref().unwrap().invalidate();
    }
}

/// A native selection drag held past the top or bottom of its pane: a timer
/// scrolls the viewport and extends the selection while the button is held.
#[derive(Clone)]
pub struct SelectionAutoScroll {
    pane_id: PaneId,
    mode: SelectionMode,
    up: bool,
    /// Rows per second.
    rate: f32,
    /// Fraction of a row not scrolled yet.
    carry: f32,
    last: Instant,
    active: Arc<AtomicBool>,
}

impl SelectionAutoScroll {
    /// 10 rows/s just past the edge, 25% faster per further row (as the
    /// companion tmux's copy mode), at most 800 rows/s.
    fn rate(rows_past: f32) -> f32 {
        (10. * 1.25f32.powf(rows_past.max(0.))).min(800.)
    }
}
