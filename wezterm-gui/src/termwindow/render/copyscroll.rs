//! Animate server-owned copy screens without changing their selection or input.
use crate::quad::{HeapQuadAllocator, TripleLayerQuadAllocator};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_millis(160);

struct Snapshot {
    position: isize,
    quads: HeapQuadAllocator,
    selection: Option<([i64; 8], HeapQuadAllocator, HeapQuadAllocator)>,
}

pub struct CopyScroll {
    frames: VecDeque<Snapshot>,
    position: isize,
    from: f32,
    started: Instant,
    duration: Duration,
    geometry: (usize, usize, usize, usize, usize, usize),
    generation: (usize, usize, usize),
}

impl CopyScroll {
    pub fn new(position: isize, geometry: (usize, usize, usize, usize, usize, usize), generation: (usize, usize, usize), now: Instant) -> Self {
        Self { frames: VecDeque::new(), position, from: 0., started: now, duration: DURATION, geometry, generation }
    }

    fn offset(&self, now: Instant) -> f32 {
        eased_offset(self.from, now.saturating_duration_since(self.started), self.duration)
    }

    fn retarget(&mut self, position: isize, now: Instant, cell_height: f32) {
        let previous = self.offset(now);
        self.from = previous - (position-self.position) as f32 * cell_height;
        let moving = previous.abs() >= 0.1;
        // Repeated input must catch up rather than accumulate a long tail.
        // Keep the displayed position continuous, but shorten the remaining
        // transition while a wheel/key burst is already moving the viewport.
        self.duration = if moving { Duration::from_millis(80) } else { DURATION };
        self.started = if moving { now } else { now - Duration::from_millis(8) };
        self.position = position;
    }

    pub fn paint(&mut self, position: isize, frame: HeapQuadAllocator,
        geometry: (usize, usize, usize, usize, usize, usize), generation: (usize, usize, usize),
        now: Instant, cell_height: f32, top: f32, bottom: f32,
        layers: &mut TripleLayerQuadAllocator, cursor_rect: Option<(f32, f32, f32, f32)>,
        selection: Option<([i64; 8], HeapQuadAllocator, HeapQuadAllocator)>, left: f32, cell_width: f32) -> anyhow::Result<bool> {
        let delta = (position - self.position) as f32 * cell_height;
        if self.geometry != geometry || self.generation != generation
            || delta.abs() > ((bottom-top)*0.5).max(8.*cell_height) {
            *self = Self::new(position, geometry, generation, now);
        } else if position != self.position {
            // Retarget from the currently displayed position, not from a row
            // boundary. Bursts and direction reversals must stay continuous.
            self.retarget(position, now, cell_height);
        }
        if self.frames.back().map(|f| f.position) == Some(position) {
            self.frames.pop_back();
        }
        self.frames.push_back(Snapshot { position, quads: frame, selection });
        while self.frames.len() > 32 { self.frames.pop_front(); }
        let offset = self.offset(now);
        log::debug!(target: "tmux_scroll", "position={} offset={:.2} age_ms={:.1}", position, offset, now.saturating_duration_since(self.started).as_secs_f32()*1000.);
        if offset.abs() < 0.1 {
            self.from = 0.;
            while self.frames.len() > 1 { self.frames.pop_front(); }
            self.frames.back().unwrap().quads.apply_y_clipped_excluding(layers, 0., f32::NEG_INFINITY, f32::INFINITY, cursor_rect);
            return Ok(false);
        }

        // New snapshots cover most of the viewport. Retained old edge rows
        // fill the exposed strip, so there is no blank band or duplicated text.
        let mask = self.frames.back().unwrap().selection.as_ref().map(|(endpoints, _, _)|
            selection_rects(*endpoints, offset, left, top, geometry.2 as f32 * cell_width, cell_width, cell_height));
        let mut gaps = vec![(top, bottom)];
        for snapshot in self.frames.iter().rev() {
            let dy = offset + (position - snapshot.position) as f32 * cell_height;
            let mut remaining = Vec::new();
            for (lo, hi) in gaps {
                let start = lo.max(top + dy);
                let end = hi.min(bottom + dy);
                if start < end {
                    if let (Some(mask), Some((_, plain, styled))) = (&mask, &snapshot.selection) {
                        let cursor_holes: Vec<_> = cursor_rect.into_iter().collect();
                        let mut holes = mask.clone();
                        holes.extend(cursor_holes.iter().copied());
                        plain.apply_masked(layers, dy, (f32::NEG_INFINITY, start, f32::INFINITY, end), &holes);
                        for &(x0,y0,x1,y1) in mask {
                            styled.apply_masked(layers, dy, (x0, y0.max(start), x1, y1.min(end)), &cursor_holes);
                        }
                    } else {
                        snapshot.quads.apply_y_clipped_excluding(layers, dy, start, end, cursor_rect);
                    }
                    if lo < start { remaining.push((lo, start)); }
                    if end < hi { remaining.push((end, hi)); }
                } else { remaining.push((lo, hi)); }
            }
            gaps = remaining;
            if gaps.is_empty() { break; }
        }
        // The tmux status/prompt row is pinned below the animated body.
        self.frames.back().unwrap().quads.apply_y_clipped(layers, 0., bottom, f32::INFINITY);
        Ok(true)
    }
}

/// Project the history anchor through the scrolling transform, but keep the
/// actively dragged endpoint at its screen position. Text and selection share
/// one clock; neither endpoint snaps when a newer server snapshot arrives.
fn selection_rects(e: [i64; 8], offset: f32, left: f32, top: f32, width: f32,
    cw: f32, ch: f32) -> Vec<(f32,f32,f32,f32)> {
    let [sx, sy, ex, ey, rectangle, vi, clipx, drag] = e;
    let mut start = (sx as f32 * cw + left, sy as f32 * ch + top + if drag == 2 { 0. } else { offset });
    let mut end = (ex as f32 * cw + left, ey as f32 * ch + top + if drag == 1 { 0. } else { offset });
    let clip = left + clipx as f32 * cw;
    let right = left + width;
    if rectangle == 1 {
        return vec![(start.0.min(end.0).max(clip), start.1.min(end.1), (start.0.max(end.0)+cw).min(right), start.1.max(end.1)+ch)];
    }
    if (start.1, start.0) > (end.1, end.0) { std::mem::swap(&mut start, &mut end); }
    // Whole-line selection keeps its start column at the text margin even
    // while the interpolated anchor crosses the active row. Each endpoint
    // carries that line's end column for whichever is visually lower.
    if rectangle == 2 { start.0 = clip; }
    let end_x = (end.0 + if vi != 0 { cw } else { 0. }).min(right);
    if (start.1-end.1).abs() < 0.01 {
        return vec![(start.0.max(clip), start.1, end_x, end.1+ch)];
    }
    // Disjoint bands also handle the anchor crossing the stationary cursor
    // within a single cell-height, without painting the same pixel twice.
    let split_top = (start.1+ch).min(end.1);
    vec![(start.0.max(clip), start.1, right, split_top),
         (clip, split_top, right, end.1),
         (clip, end.1, end_x, end.1+ch)]
}

fn eased_offset(from: f32, elapsed: Duration, duration: Duration) -> f32 {
    let t = (elapsed.as_secs_f32()/duration.as_secs_f32()).min(1.);
    from * (1.-t).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_selection_crossing_keeps_the_cursor_row_highlighted() {
        for offset in [-57., -38., -19.5, -19., -18.5, -0.1, 0., 19.] {
            let rects = selection_rects([50,29,50,30,2,1,0,2], offset, 0., 0., 540., 10., 19.);
            // A text column in the middle of the line must remain selected
            // throughout the fixed cursor row, including anchor overlap.
            for y in 551..570 {
                assert!(rects.iter().any(|&(l,t,r,b)| l <= 150. && 150. < r && t <= y as f32 && (y as f32) < b), "offset={offset} y={y} rects={rects:?}");
            }
        }
    }
    #[test]
    fn active_rectangle_head_is_pinned_while_anchor_tracks_text() {
        for (drag, sy, ey) in [(1, 18, 8), (2, 8, 18), (1, 8, 18), (2, 18, 8)] {
            let e = [12, sy, 15, ey, 1, 1, 0, drag];
            for offset in [-38., -19.5, 0., 17.2, 38.] {
                let rect = selection_rects(e, offset, 0., 0., 540., 10., 19.)[0];
                let head = if drag == 1 { ey } else { sy } as f32 * 19.;
                let anchor = if drag == 1 { sy } else { ey } as f32 * 19. + offset;
                assert_eq!(rect, (120., head.min(anchor), 160., head.max(anchor)+19.));
            }
        }
    }
    #[test]
    fn stopped_selection_moves_with_history_and_keeps_size() {
        for offset in [-38., -19.5, 0., 17.2, 38.] {
            let rect = selection_rects([12,8,15,18,1,1,0,0], offset, 0., 0., 540., 10., 19.)[0];
            assert_eq!(rect.1, 152.+offset);
            assert_eq!(rect.3, 361.+offset);
        }
    }
    #[test]
    fn character_selection_preserves_cursor_row_and_anchor_column() {
        let rects = selection_rects([12,18,15,8,0,1,0,1], -22.5, 0., 0., 540., 10., 19.);
        assert_eq!(rects[0], (150.,152.,540.,171.));
        assert_eq!(rects[2], (0.,319.5,130.,338.5));
    }
    #[test]
    fn scroll_easing_is_continuous_and_settles() {
        let values: Vec<_> = (0..=10).map(|i| eased_offset(-38., Duration::from_millis(i*16), DURATION)).collect();
        assert_eq!(values[0], -38.);
        assert_eq!(values[10], 0.);
        assert!(values.windows(2).all(|v| v[0] < v[1]));
        assert!(values[1..9].iter().all(|v| (v/19.).fract().abs() > 0.001));
    }
    #[test]
    fn scroll_retarget_preserves_displayed_position() {
        let now = Instant::now();
        let mut state = CopyScroll::new(0, (0,0,0,0,0,0), (0,0,0), now);
        state.position = 3;
        state.from = -57.;
        let later = now + Duration::from_millis(48);
        let displayed = 3.*19. + state.offset(later);
        state.retarget(1, later, 19.);
        assert!((1.*19. + state.offset(later) - displayed).abs() < 0.001);
        assert_eq!(state.offset(later + Duration::from_millis(80)), 0.);
    }
    #[test]
    fn scroll_first_frame_moves_without_waiting_for_another_repaint() {
        let now = Instant::now();
        let mut state = CopyScroll::new(0, (0,0,0,0,0,0), (0,0,0), now);
        state.retarget(2, now, 19.);
        assert!(state.offset(now) > -38.);
        assert!(state.offset(now) < -30.);
    }
}
