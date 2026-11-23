use crate::selection::{SelectionCoordinate, SelectionRange, SelectionX};
use crate::termwindow::keyevent::KeyTableArgs;
use crate::termwindow::{TermWindow, TermWindowNotif};
use config::keyassignment::{
    ClipboardCopyDestination, CopyModeAssignment, KeyAssignment, KeyTable, KeyTableEntry,
    ScrollbackEraseMode, SelectionMode,
};
use mux::domain::DomainId;
use mux::pane::{
    CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, Pattern, PatternType,
    PerformAssignmentResult, SearchResult, WithPaneLines,
};
use mux::renderable::*;
use mux::tab::TabId;
use ordered_float::NotNan;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use termwiz::cell::{Cell, CellAttributes};
use termwiz::color::AnsiColor;
use termwiz::lineedit::{LineEditBuffer, Movement};
use termwiz::surface::{CursorVisibility, SequenceNo, SEQ_ZERO};
use std::time::Duration;
use unicode_segmentation::*;
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    unicode_column_width, Clipboard, KeyCode, KeyModifiers, Line, MouseEvent, SemanticType,
    StableRowIndex, TerminalSize,
};
use window::{KeyCode as WKeyCode, Modifiers, WindowOps};

lazy_static::lazy_static! {
    static ref SAVED_PATTERN: Mutex<HashMap<TabId, Pattern>> = Mutex::new(HashMap::new());
    /// The last visual selection of each pane (its start, the cursor and the
    /// mode), for gv: it outlives copy mode.
    static ref LAST_SELECTION: Mutex<HashMap<PaneId, (SelectionCoordinate, usize, StableRowIndex, SelectionMode)>> = Mutex::new(HashMap::new());
    /// Patterns searched in this GUI, oldest first, for Up/Down in search.
    static ref SEARCH_HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

const SEARCH_HISTORY_LIMIT: usize = 200;

/// Keep `pattern` as the newest entry of the search history.
fn remember_search(pattern: &str) {
    if pattern.is_empty() {
        return;
    }
    let mut history = SEARCH_HISTORY.lock();
    history.retain(|old| old != pattern);
    history.push(pattern.to_owned());
    let extra = history.len().saturating_sub(SEARCH_HISTORY_LIMIT);
    history.drain(..extra);
}

fn highlight_commands(line: &mut Line, cols: usize, config: &config::ConfigHandle) {
    if let Some(bg)=config.copy_mode_command_background {
        if !line.visible_cells().any(|cell| cell.attrs().semantic_type()==SemanticType::Input && !cell.str().trim().is_empty()) {
            return;
        }
        // Shade the complete command row, including continuation rows and
        // trailing space. Foreground/style attributes remain unchanged.
        line.resize(cols, SEQ_ZERO);
        for cell in line.cells_mut_for_attr_changes_only() {
            cell.attrs_mut().set_background(termwiz::color::ColorAttribute::TrueColorWithDefaultFallback(bg.into()));
        }
        // Cloned lines inherit cached shaping data. Do not reuse or overwrite
        // the underlying terminal's cache for an overlay-only decoration.
        line.clear_appdata();
    }
}

const SEARCH_CHUNK_SIZE: StableRowIndex = 1000;

pub(crate) fn command_selection_columns(line: &Line, mut range: Range<usize>) -> Range<usize> {
    while range.start < range.end && line.get_cell(range.start).is_some_and(|c| c.attrs().semantic_type()==SemanticType::Prompt) {
        range.start += 1;
    }
    range
}

pub(crate) fn render_pane_status(line: &mut Line, cols: usize, left: &str, right: &str) {
    let mut attrs = CellAttributes::default();
    attrs.set_foreground(termwiz::color::ColorAttribute::TrueColorWithDefaultFallback(termwiz::color::SrgbaTuple::from((0xbc, 0xbc, 0xbc))));
    line.fill_range(0..cols, &Cell::new('─', attrs.clone()), SEQ_ZERO);
    let right = termwiz_funcs::truncate_right(right, cols.saturating_sub(8));
    let right_width = unicode_column_width(&right, None);
    let searching = left.starts_with('/');
    let label = if searching { left.to_owned() } else { format!("──  {}  ──", left) };
    let left = termwiz_funcs::truncate_right(&label, cols.saturating_sub(right_width + if searching { 0 } else { 2 }));
    let mut accent = attrs.clone();
    accent.set_foreground(termwiz::color::ColorAttribute::TrueColorWithDefaultFallback(termwiz::color::SrgbaTuple::from((0x00, 0x9f, 0xd7))));
    line.overlay_text_with_attribute(if searching {0} else {2}, &left, accent, SEQ_ZERO);
    if right_width > 0 {
        line.overlay_text_with_attribute(cols.saturating_sub(right_width), &right, attrs, SEQ_ZERO);
    }
    line.clear_appdata();
}

pub struct CopyOverlay {
    delegate: Arc<dyn Pane>,
    render: Arc<Mutex<CopyRenderable>>,
    writer: Mutex<SearchOverlayPatternWriter>,
}

impl CopyOverlay {
    pub fn is_command_selection(&self) -> bool { self.render.lock().command_selection }
    pub fn selection_is_search_match(&self) -> bool {
        let r = self.render.lock();
        r.result_pos.and_then(|i| r.results.get(i)).is_some_and(|m|
            r.cursor.y == m.end_y && r.cursor.x == m.end_x.saturating_sub(1)
            && r.start == Some(SelectionCoordinate::x_y(m.start_x, m.start_y)))
    }
    pub fn status_line(&self, cols: usize) -> (Line, Option<usize>) {
        let mut r = self.render.lock();
        let mut line = Line::new(SEQ_ZERO);
        let position = r.status_position();
        r.render_pane_status(&mut line, cols, &position);
        let cursor = r.editing_search.then(|| 1 + unicode_column_width(&r.search_line.get_line()[..r.search_line.get_cursor()], None));
        (line, cursor)
    }
    /// The next keys go to f/t (their target or a label), not to bindings.
    pub fn captures_keys(&self) -> bool {
        self.render.lock().captures_keys()
    }
    /// For tests: the labels on the screen and whether the matches show.
    pub fn jump_state(&self) -> (Vec<(char, isize, usize)>, usize, bool) {
        let r = self.render.lock();
        (r.sneak_labels.clone(), r.sneak_marks.values().map(|m| m.len()).sum(), r.pending_jump.is_some())
    }
    pub fn cursor_and_selection(&self)->(usize,isize,Option<(usize,isize)>,bool,isize,usize,String,bool,bool) {
        let r=self.render.lock();
        (r.cursor.x,r.cursor.y,r.start.map(|p|(match p.x {SelectionX::Cell(x)=>x,SelectionX::BeforeZero=>0},p.y)),r.selection_mode==SelectionMode::Block,r.viewport.unwrap_or(self.delegate.get_dimensions().physical_top),r.results.len(),r.search_line.get_line().to_owned(),r.editing_search,r.highlight_commands)
    }
}

/// The labels of a two-character jump, in the order of vim-sneak with t
/// mapped to Sneak_s and sneak#s_next (t first: t again goes to the next match).
const SNEAK_LABELS: &str = "t;sfunq/SFGHLTUNRMQZ?0";

#[derive(Clone, Debug)]
struct PendingJump {
    forward: bool,
    prev_char: bool,
    /// Two characters and labels (vim-sneak s), else one character (f).
    label: bool,
    typed: String,
}

#[derive(Clone, Debug)]
struct Jump {
    forward: bool,
    prev_char: bool,
    label: bool,
    target: String,
}

struct CopyRenderable {
    cursor: StableCursorPosition,
    delegate: Arc<dyn Pane>,
    start: Option<SelectionCoordinate>,
    selection_mode: SelectionMode,
    viewport: Option<StableRowIndex>,
    scrolling: bool,
    viewport_request: Arc<AtomicU64>,
    highlight_commands: bool,
    command_selection: bool,
    status_seqno: Option<SequenceNo>,
    status_prompts: Vec<StableRowIndex>,
    /// We use this to cancel ourselves later
    window: ::window::Window,

    /// The text that the user entered
    pattern_type: PatternType,
    search_line: LineEditBuffer,
    /// The most recently queried set of matches
    results: Vec<SearchResult>,
    by_line: HashMap<StableRowIndex, Vec<MatchResult>>,
    last_result_seqno: SequenceNo,
    last_bar_pos: Option<StableRowIndex>,
    dirty_results: RangeSet<StableRowIndex>,
    width: usize,
    height: usize,
    editing_search: bool,
    result_pos: Option<usize>,
    reflow_result_pos: Option<usize>,
    directed_search: Option<bool>,
    search_origin: (StableRowIndex, usize),
    search_generation: u64,
    /// Up/Down in the search editor: the history entry shown, and the text
    /// that was typed before the first Up.
    history_pos: Option<usize>,
    history_draft: String,
    /// The highlights on screen belong to the previous pattern. They stay
    /// until the first results of the new search arrive, then are replaced
    /// in one step: typing a pattern does not blank and redraw them.
    replace_results: bool,
    activate_search_results: bool,
    tab_id: TabId,
    searching: Option<Searching>,
    pending_jump: Option<PendingJump>,
    /// g was typed: the next key completes gg, gv, ge or gE.
    pending_g: bool,
    last_jump: Option<Jump>,
    /// Right after f/t: the matches show, and f/t again go to the next one.
    sneak_active: bool,
    /// The highlighted matches by row: the cells, and the label on them.
    sneak_marks: HashMap<StableRowIndex, Vec<(Range<usize>, Option<char>)>>,
    /// Label mode: the label and the cell of each labeled match.
    sneak_labels: Vec<(char, StableRowIndex, usize)>,
    /// The matches after the labeled ones (Tab labels them).
    sneak_rest: Vec<(StableRowIndex, usize, usize)>,
    /// The assignment of the key that ends label mode, done after it.
    sneak_deferred: Option<KeyAssignment>,
}

struct Searching {
    remain: StableRowIndex,
    pending: VecDeque<Range<StableRowIndex>>,
}

#[derive(Debug)]
struct MatchResult {
    range: Range<usize>,
}

struct Dimensions {
    vertical_gap: isize,
    dims: RenderableDimensions,
    top: StableRowIndex,
}

#[derive(Debug)]
pub struct CopyModeParams {
    pub pattern: Pattern,
    pub editing_search: bool,
}

impl CopyOverlay {
    pub fn with_pane(
        term_window: &TermWindow,
        pane: &Arc<dyn Pane>,
        params: CopyModeParams,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mut cursor = pane.get_cursor_position();
        cursor.shape = termwiz::surface::CursorShape::SteadyBlock;
        cursor.visibility = CursorVisibility::Visible;

        let (_domain, _window, tab_id) = mux::Mux::get()
            .resolve_pane_id(pane.pane_id())
            .ok_or_else(|| anyhow::anyhow!("no tab contains the current pane"))?;

        let window = term_window
            .window
            .clone()
            .ok_or_else(|| anyhow::anyhow!("failed to clone window handle"))?;
        let dims = pane.get_dimensions();
        let pattern = if params.pattern.is_empty() && !config::configuration().copy_mode_pane_status {
            SAVED_PATTERN
                .lock()
                .get(&tab_id)
                .map(|p| p.clone())
                .unwrap_or(params.pattern)
        } else {
            params.pattern
        };
        let search_line = LineEditBuffer::new(&pattern, pattern.len());

        let mut render = CopyRenderable {
            cursor,
            window,
            delegate: Arc::clone(pane),
            start: None,
            viewport: term_window.get_viewport(pane.pane_id()),
            scrolling: false,
            viewport_request: Arc::new(AtomicU64::new(0)),
            highlight_commands: false,
            command_selection: false,
            status_seqno: None,
            status_prompts: vec![],
            results: vec![],
            by_line: HashMap::new(),
            dirty_results: RangeSet::default(),
            width: dims.cols,
            height: dims.viewport_rows,
            last_result_seqno: SEQ_ZERO,
            last_bar_pos: None,
            tab_id,
            pattern_type: PatternType::from(&pattern),
            search_line,
            editing_search: params.editing_search,
            result_pos: None,
            reflow_result_pos: None,
            directed_search: None,
            search_origin: (cursor.y, cursor.x),
            search_generation: 0,
            history_pos: None,
            history_draft: String::new(),
            replace_results: false,
            activate_search_results: params.editing_search,
            selection_mode: SelectionMode::Cell,
            searching: None,
            pending_jump: None,
            pending_g: false,
            last_jump: None,
            sneak_active: false,
            sneak_marks: HashMap::new(),
            sneak_labels: vec![],
            sneak_rest: vec![],
            sneak_deferred: None,
        };

        let search_row = render.compute_search_row();
        render.dirty_results.add(search_row);
        render.update_search();

        let shared_render = Arc::new(Mutex::new(render));
        let writer = SearchOverlayPatternWriter {
            render: Arc::clone(&shared_render),
        };

        Ok(Arc::new(CopyOverlay {
            delegate: Arc::clone(pane),
            render: shared_render,
            writer: Mutex::new(writer),
        }))
    }

    pub fn get_params(&self) -> CopyModeParams {
        let render = self.render.lock();
        CopyModeParams {
            pattern: render.get_pattern(),
            editing_search: render.editing_search,
        }
    }

    pub fn apply_params(&self, params: CopyModeParams) {
        let mut render = self.render.lock();
        render.editing_search = params.editing_search;
        if params.editing_search {render.activate_search_results=true;render.directed_search=None;}
        if render.get_pattern() != params.pattern {
            render.pattern_type = PatternType::from(&params.pattern);
            render
                .search_line
                .set_line_and_cursor(&params.pattern, params.pattern.len());
            render.schedule_update_search();
        }
        let search_row = render.compute_search_row();
        render.dirty_results.add(search_row);
    }

    pub fn viewport_changed(&self, viewport: Option<StableRowIndex>) {
        let mut render = self.render.lock();
        if render.viewport != viewport {
            if let Some(last) = render.last_bar_pos.take() {
                render.dirty_results.add(last);
            }
            if let Some(pos) = viewport.as_ref() {
                render.dirty_results.add(*pos);
            }
            render.viewport = viewport;
        }
    }
}

impl CopyRenderable {
    /// Select the shell command above, below or at the cursor, and shade
    /// the commands on screen. The cursor stays on the last cell of the
    /// command, as the anchor of the next jump.
    fn select_command(&mut self, direction: isize, prompt_marker: Option<&str>, select: bool) {
        use wezterm_term::commands::{find_command, CommandTarget};
        let zones = self.delegate.get_semantic_zones().unwrap_or_default();
        // Visual mode (a selection that the user started): extend it by
        // whole commands instead of selecting one.
        if self.start.is_some() && !self.command_selection && direction != 0 {
            self.extend_by_command(&zones, direction);
            self.set_command_highlight(true);
            return;
        }
        let target = match direction {
            d if d < 0 => CommandTarget::Previous,
            0 => CommandTarget::Current,
            _ => CommandTarget::Next,
        };
        let delegate = self.delegate.clone();
        let found = find_command(&zones, self.cursor.y, target, prompt_marker, &mut |row| {
            delegate.get_lines(row..row + 1).1.pop()
        });
        if let Some(range) = found {
            self.clear_selection_mode();
            if !select {
                // Only move: the cursor goes to the start of the command.
                self.selection_mode = SelectionMode::Cell;
                self.command_selection = false;
                self.cursor.x = range.start_x;
                self.cursor.y = range.start_y;
                self.select_to_cursor_pos();
                self.set_command_highlight(true);
                return;
            }
            self.cursor.x = range.start_x;
            self.cursor.y = range.start_y;
            self.start.replace(SelectionCoordinate::x_y(range.start_x, range.start_y));
            self.selection_mode = SelectionMode::Cell;
            self.cursor.x = range.end_x;
            self.cursor.y = range.end_y;
            self.command_selection = true;
            self.select_to_cursor_pos();
        }
        self.set_command_highlight(true);
    }
    /// Visual mode: Ctrl-j moves the end of the selection to the end of the
    /// command's block (its output, up to the row before the next prompt);
    /// Ctrl-k moves it to the start of the row of the previous command.
    fn extend_by_command(&mut self, zones: &[wezterm_term::SemanticZone], direction: isize) {
        let dims = self.delegate.get_dimensions();
        let last_row = dims.physical_top + dims.viewport_rows as StableRowIndex - 1;
        let delegate = self.delegate.clone();
        let blocks = wezterm_term::commands::command_blocks(zones, last_row, &mut |row| {
            delegate.get_lines(row..row + 1).1.pop()
        });
        let y = self.cursor.y;
        if direction > 0 {
            let Some(end) = blocks.iter().map(|(_, end)| *end).find(|end| *end > y) else { return; };
            let line = self.delegate.get_lines(end..end + 1).1.pop();
            let width = line.map(|l| unicode_column_width(l.as_str().trim_end(), None)).unwrap_or(0);
            self.cursor.y = end;
            self.cursor.x = width.saturating_sub(1);
        } else {
            let start = blocks
                .iter()
                .map(|(range, _)| range.start_y)
                .filter(|start| *start < y || (*start == y && self.cursor.x > 0))
                .last();
            let Some(start) = start else { return; };
            self.cursor.y = start;
            self.cursor.x = 0;
        }
        self.select_to_cursor_pos();
    }
    fn set_command_highlight(&mut self, value: bool) {
        if self.highlight_commands == value { return; }
        self.highlight_commands = value;
        let dims = self.delegate.get_dimensions();
        let top = self.viewport.unwrap_or(dims.physical_top);
        for row in top..top + dims.viewport_rows as isize { self.dirty_results.add(row); }
        self.window.invalidate();
    }
    fn status_position(&mut self) -> String {
        let dims = self.delegate.get_dimensions();
        let seqno = self.delegate.get_current_seqno();
        if self.status_seqno != Some(seqno) {
            self.status_prompts = self.delegate.get_semantic_zones().unwrap_or_default().into_iter()
                .filter(|z| z.semantic_type == SemanticType::Prompt).map(|z|z.start_y).collect();
            self.status_seqno = Some(seqno);
        }
        let command = self.status_prompts.partition_point(|y| *y <= self.cursor.y);
        let cmd = if self.status_prompts.is_empty() { String::new() } else { format!("cmd {command}/{}  ", self.status_prompts.len()) };
        format!(" {cmd}[{}/{}] ", self.cursor.y.saturating_sub(dims.scrollback_top), dims.scrollback_rows)
    }

    fn render_pane_status(&mut self, line: &mut Line, cols: usize, position: &str) {
        let pattern = self.get_pattern();
        let left = if self.editing_search { format!("/{} ", *pattern) } else { " COPY ".to_string() };
        let right = if !pattern.is_empty() {
            format!(" {}/{} matches {}", self.result_pos.map(|i| i+1).unwrap_or(0), self.results.len(), position)
        } else { position.to_owned() };
        render_pane_status(line, cols, &left, &right);

    }

    fn compute_search_row(&self) -> StableRowIndex {
        let dims = self.delegate.get_dimensions();
        let top = self.viewport.unwrap_or_else(|| dims.physical_top);
        let bottom = (top + dims.viewport_rows as StableRowIndex).saturating_sub(1);
        bottom
    }

    fn check_for_resize(&mut self) {
        let dims = self.delegate.get_dimensions();
        if dims.cols == self.width && dims.viewport_rows == self.height {
            return;
        }

        self.width = dims.cols;
        self.height = dims.viewport_rows;

        // Reflow invalidates every physical selection coordinate. Keep the
        // query, but never paint an old-width range while the new search runs.
        self.reflow_result_pos = self.result_pos.or(self.reflow_result_pos);
        self.clear_selection();
        self.activate_search_results = true;
        self.update_search();
    }

    fn incrementally_recompute_results(&mut self, mut results: Vec<SearchResult>) {
        results.sort();
        results.reverse();
        for res in &results {
            for idx in res.start_y..=res.end_y {
                let range = if idx == res.start_y && idx == res.end_y {
                    // Range on same line
                    res.start_x..res.end_x
                } else if idx == res.end_y {
                    // final line of multi-line
                    0..res.end_x
                } else if idx == res.start_y {
                    // first line of multi-line
                    res.start_x..self.width
                } else {
                    // a middle line
                    0..self.width
                };

                let result = MatchResult {
                    range,
                };

                let matches = self.by_line.entry(idx).or_insert_with(|| vec![]);
                matches.push(result);

                self.dirty_results.add(idx);
            }
        }
        // Chunks grow outward from the viewport. Newer chunks precede the
        // existing descending list; preserve the selected match's identity.
        if results.first().zip(self.results.first()).is_some_and(|(new,old)|
            (new.start_y,new.start_x) > (old.start_y,old.start_x)) {
            if let Some(index)=self.result_pos.as_mut() {*index+=results.len();}
            results.append(&mut self.results);
            self.results=results;
        } else {
            self.results.append(&mut results);
        }
    }

    fn schedule_update_search(&mut self) {
        // Searching runs against the client's terminal in bounded chunks.
        // Start immediately: waiting for typing to stop makes incremental
        // highlighting lag every keystroke. Generation checks discard old work.
        self.reflow_result_pos = None;
        self.update_search();
    }

    fn update_search(&mut self) {
        self.search_generation+=1;
        let generation=self.search_generation;
        for idx in self.by_line.keys() {
            self.dirty_results.add(*idx);
        }
        if let Some(idx) = self.last_bar_pos.as_ref() {
            self.dirty_results.add(*idx);
        }

        // Keep the current highlights (see `replace_results`).
        self.replace_results = true;

        SAVED_PATTERN.lock().insert(self.tab_id, self.get_pattern());

        let bar_pos = self.compute_search_row();
        self.dirty_results.add(bar_pos);
        self.last_result_seqno = self.delegate.get_current_seqno();

        let pattern = self.get_pattern();
        if !pattern.is_empty() {
            let pane: Arc<dyn Pane> = self.delegate.clone();
            let window = self.window.clone();
            let dims = pane.get_dimensions();

            let end = dims.scrollback_top + dims.scrollback_rows as StableRowIndex;
            let top=self.viewport.unwrap_or(dims.physical_top).clamp(dims.scrollback_top,end);
            let range=top..(top+dims.viewport_rows as StableRowIndex).min(end);
            let mut above=VecDeque::new();let mut below=VecDeque::new();
            let mut edge=range.start;
            while edge>dims.scrollback_top {
                let start=(edge-SEARCH_CHUNK_SIZE).max(dims.scrollback_top);
                above.push_back(start..edge);edge=start;
            }
            edge=range.end;
            while edge<end {
                let next=(edge+SEARCH_CHUNK_SIZE).min(end);
                below.push_back(edge..next);edge=next;
            }
            // Search visible text first, then expand in the search direction.
            // Text below a scrolled-back viewport must not delay its preview.
            let pending=if self.directed_search==Some(true) {
                below.extend(above);below
            } else {above.extend(below);above};
            self.searching.replace(Searching {
                remain: pending.iter().map(|r| r.end-r.start).sum(), pending,
            });

            promise::spawn::spawn(async move {
                let limit = None;
                log::trace!("Searching for {pattern:?} in {range:?}");
                let results = pane.search(pattern.clone(), range.clone(), limit).await?;

                let pane_id = pane.pane_id();
                let mut results = Some(results);
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let state = term_window.pane_state(pane_id);
                    if let Some(overlay) = state.overlay.as_ref() {
                        if let Some(copy_overlay) = overlay.pane.downcast_ref::<CopyOverlay>() {
                            let mut r = copy_overlay.render.lock();
                            r.processed_search_chunk(generation, pattern, results.take().unwrap(), range);
                        }
                    }
                })));

                anyhow::Result::<()>::Ok(())
            })
            .detach();
        } else {
            self.searching.take();
            self.replace_results = false;
            self.results.clear();
            self.by_line.clear();
            self.result_pos.take();
            self.clear_selection();
        }
        self.window.invalidate();
    }

    fn processed_search_chunk(
        &mut self,
        generation: u64,
        pattern: Pattern,
        mut results: Vec<SearchResult>,
        range: Range<StableRowIndex>,
    ) {
        self.window.invalidate();
        if generation != self.search_generation || pattern != self.get_pattern() {
            return;
        }
        // Native logical-line search can expand past a physical chunk edge.
        // Assign each match to the chunk containing its start, exactly once.
        results.retain(|result| range.contains(&result.start_y));
        if std::mem::take(&mut self.replace_results) {
            // The first results of this pattern replace the old ones.
            for idx in self.by_line.keys() {
                self.dirty_results.add(*idx);
            }
            self.results.clear();
            self.by_line.clear();
            self.result_pos.take();
            if results.is_empty() {
                self.clear_selection();
            }
        }
        let is_first = self.results.is_empty();
        self.incrementally_recompute_results(results);

        if is_first && self.activate_search_results && self.directed_search.is_none() && self.reflow_result_pos.is_none() {
            if !self.results.is_empty() {
                self.activate_match_number(0);
            } else {
                self.set_viewport(None);
                self.clear_selection();
            }
        }

        // A backwards search can select its closest match as soon as that
        // chunk arrives. Previously selection waited for the entire history,
        // even when the matching text was already in the visible viewport.
        if self.result_pos.is_none() && self.activate_search_results && self.reflow_result_pos.is_none() {
            if let Some(forward) = self.directed_search {
                let origin = self.search_origin;
                let selected = if forward {
                    self.results.iter().rposition(|r| (r.start_y, r.start_x) >= origin)
                } else {
                    self.results.iter().position(|r| (r.start_y, r.start_x) <= origin)
                };
                if let Some(index) = selected { self.activate_match_number(index); }
            }
        }
        let next=self.searching.as_mut().and_then(|search| {
            let next=search.pending.pop_front();
            if let Some(range)=&next {search.remain-=range.end-range.start;}
            next
        });
        if next.is_none() {
            if let Some(index)=self.reflow_result_pos.take() {
                if !self.results.is_empty() {self.activate_match_number(index.min(self.results.len()-1));}
            } else if let Some(forward)=self.directed_search.filter(|_|self.activate_search_results && self.result_pos.is_none()) {
                let origin=self.search_origin;
                let selected=if forward {
                    self.results.iter().rposition(|r|(r.start_y,r.start_x)>=origin)
                } else {
                    self.results.iter().position(|r|(r.start_y,r.start_x)<=origin)
                };
                if let Some(index)=selected {self.activate_match_number(index);}
                else {self.cursor.y=origin.0;self.cursor.x=origin.1;self.clear_selection();}
            }
            self.searching.take();
            return;
        }

        // Search next chunk
        let pane: Arc<dyn Pane> = self.delegate.clone();
        let window = self.window.clone();
        let range = next.unwrap();

        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(1)).await;
            let limit = None;
            log::trace!("Searching for {pattern:?} in {range:?}");
            let results = pane.search(pattern.clone(), range.clone(), limit).await?;

            let pane_id = pane.pane_id();
            let mut results = Some(results);
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let state = term_window.pane_state(pane_id);
                if let Some(overlay) = state.overlay.as_ref() {
                    if let Some(copy_overlay) = overlay.pane.downcast_ref::<CopyOverlay>() {
                        let mut r = copy_overlay.render.lock();
                        r.processed_search_chunk(generation, pattern, results.take().unwrap(), range);
                    }
                }
            })));

            anyhow::Result::<()>::Ok(())
        })
        .detach();
    }

    fn clear_selection(&mut self) {
        let pane_id = self.delegate.pane_id();
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let mut selection = term_window.selection(pane_id);
                selection.origin.take();
                selection.range.take();
            })));
    }

    fn activate_match_number(&mut self, n: usize) {
        self.result_pos.replace(n);
        let result = self.results[n].clone();
        self.cursor.y = result.end_y;
        self.cursor.x = result.end_x.saturating_sub(1);

        let start = SelectionCoordinate::x_y(result.start_x, result.start_y);
        let end = SelectionCoordinate::x_y(result.end_x.saturating_sub(1), result.end_y);
        self.start.replace(start);
        self.adjust_selection(start, SelectionRange { start, end });
    }

    fn clamp_cursor_to_scrollback(&mut self) {
        let dims = self.delegate.get_dimensions();
        if self.cursor.x >= dims.cols {
            self.cursor.x = dims.cols - 1;
        }
        if self.cursor.y < dims.scrollback_top {
            self.cursor.y = dims.scrollback_top;
        }

        let max_row = dims.scrollback_top + dims.scrollback_rows as isize;
        if self.cursor.y >= max_row {
            self.cursor.y = max_row - 1;
        }
    }

    fn select_to_cursor_pos(&mut self) {
        self.dirty_results.add(self.compute_search_row());
        self.clamp_cursor_to_scrollback();
        if let Some(sel_start) = self.start {
            LAST_SELECTION.lock().insert(self.delegate.pane_id(), (sel_start, self.cursor.x, self.cursor.y, self.selection_mode));
            let cursor = SelectionCoordinate::x_y(self.cursor.x, self.cursor.y);

            let (start, end) = match self.selection_mode {
                SelectionMode::Line => {
                    let cursor_is_above_start = self.cursor.y < sel_start.y;

                    let start = SelectionCoordinate::x_y(
                        if cursor_is_above_start {
                            usize::max_value()
                        } else {
                            0
                        },
                        sel_start.y,
                    );
                    let end = SelectionCoordinate::x_y(
                        if cursor_is_above_start {
                            0
                        } else {
                            usize::max_value()
                        },
                        self.cursor.y,
                    );
                    (start, end)
                }
                SelectionMode::SemanticZone => {
                    let zone_range = SelectionRange::zone_around(cursor, &*self.delegate);
                    let start_zone = SelectionRange::zone_around(sel_start, &*self.delegate);

                    let range = zone_range.extend_with(start_zone);

                    (range.start, range.end)
                }
                _ => {
                    let start = SelectionCoordinate {
                        x: sel_start.x,
                        y: sel_start.y,
                    };
                    let end = cursor;
                    (start, end)
                }
            };

            self.adjust_selection(start, SelectionRange { start, end });
        } else {
            self.adjust_viewport_for_cursor_position();
            self.window.invalidate();
        }
    }

    fn adjust_selection(&mut self, start: SelectionCoordinate, range: SelectionRange) {
        let pane_id = self.delegate.pane_id();
        let window = self.window.clone();
        let mode = self.selection_mode;
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let mut selection = term_window.selection(pane_id);
                selection.origin = Some(start);
                selection.range = Some(range);
                selection.rectangular = mode == SelectionMode::Block;
                window.invalidate();
            })));
        self.adjust_viewport_for_cursor_position();
    }

    fn dimensions(&self) -> Dimensions {
        const VERTICAL_GAP: isize = 5;
        let dims = self.delegate.get_dimensions();
        let vertical_gap = if dims.physical_top <= VERTICAL_GAP {
            1
        } else {
            VERTICAL_GAP
        };
        let top = self.viewport.unwrap_or_else(|| dims.physical_top);
        Dimensions {
            vertical_gap,
            top,
            dims,
        }
    }

    fn adjust_viewport_for_cursor_position(&mut self) {
        if self.scrolling { return; }
        let dims = self.dimensions();

        if dims.top > self.cursor.y {
            // Cursor is off the top of the viewport; adjust
            self.set_viewport(Some(self.cursor.y.saturating_sub(dims.vertical_gap)));
            return;
        }

        let top_gap = self.cursor.y - dims.top;
        if top_gap < dims.vertical_gap {
            // Increase the gap so we can "look ahead"
            self.set_viewport(Some(self.cursor.y.saturating_sub(dims.vertical_gap)));
            return;
        }

        // Do not scroll backwards merely because a wheel/Alt scroll left the
        // cursor in the bottom margin. Only move down once it leaves the view.
        let rows = dims.dims.viewport_rows.max(1) as isize;
        if top_gap >= rows {
            self.set_viewport(Some(self.cursor.y - rows + 1));
        }
    }

    fn set_viewport(&mut self, row: Option<StableRowIndex>) {
        let dims = self.delegate.get_dimensions();
        // Never above the scrollback or below the live screen: the smooth
        // scroll would animate to that row and then snap back (k on the
        // top row of a pane without scrollback).
        let row = row.map(|r| r.min(dims.physical_top).max(dims.scrollback_top));
        log::debug!(target: "copy_scroll", "COPY_VIEWPORT {row:?} scrollback_top {}", dims.scrollback_top);
        self.viewport = row;
        let requests = self.viewport_request.clone();
        let request = requests.fetch_add(1, Ordering::Relaxed) + 1;
        let pane_id = self.delegate.pane_id();
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                if requests.load(Ordering::Relaxed) == request {
                    term_window.set_viewport(pane_id, row, dims);
                }
            })));
    }

    fn close(&mut self) {
        remember_search(self.search_line.get_line());
        self.search_generation += 1;
        self.activate_search_results = false;
        SAVED_PATTERN.lock().remove(&self.tab_id);
        TermWindow::schedule_cancel_overlay_for_pane(self.window.clone(), self.delegate.pane_id());
    }

    fn scroll_by_line(&mut self, amount: isize) {
        self.activate_search_results=false;
        self.directed_search=None;
        let dims=self.delegate.get_dimensions();
        let old=self.viewport.unwrap_or(dims.physical_top);
        let top=(old+amount).clamp(dims.scrollback_top,dims.physical_top);
        self.cursor.y+=top-old;
        self.viewport=Some(top);
        self.scrolling=true;
        self.select_to_cursor_pos();
        self.scrolling=false;
        self.set_viewport(Some(top));
    }

    fn scroll_viewport(&mut self, amount: isize) {
        let dims = self.delegate.get_dimensions();
        let old = self.viewport.unwrap_or(dims.physical_top);
        let top = (old + amount).clamp(dims.scrollback_top, dims.physical_top);
        if top == old { return; }
        let rows = dims.viewport_rows.max(1) as isize;
        self.cursor.y = self.cursor.y.clamp(top, top + rows - 1);
        self.viewport = Some(top);
        self.scrolling = true;
        self.select_to_cursor_pos();
        self.scrolling = false;
        self.set_viewport(Some(top));
    }

    fn move_by_page(&mut self, amount: f64) {
        let dims = self.dimensions();
        let rows = (dims.dims.viewport_rows as f64 * amount) as isize;
        self.cursor.y += rows;
        self.select_to_cursor_pos();
    }

    /// Move to next match
    fn next_match(&mut self) {
        if let Some(cur) = self.result_pos.as_ref() {
            let prior = if *cur > 0 {
                cur - 1
            } else {
                if self.delegate.is::<mux::rex::RexPane>() { return; }
                self.results.len() - 1
            };
            self.activate_match_number(prior);
        }
    }

    /// Move to prior match
    fn prior_match(&mut self) {
        if let Some(cur) = self.result_pos.as_ref() {
            let next = if *cur + 1 >= self.results.len() {
                if self.delegate.is::<mux::rex::RexPane>() { return; }
                0
            } else {
                *cur + 1
            };
            self.activate_match_number(next);
        }
    }

    /// Skip this page of matches and move down to the first match from
    /// the next page.
    fn next_match_page(&mut self) {
        let dims = self.delegate.get_dimensions();
        if let Some(cur) = self.result_pos {
            let top = self.viewport.unwrap_or(dims.physical_top);
            let prior = top - dims.viewport_rows as isize;
            if let Some(pos) = self
                .results
                .iter()
                .position(|res| res.start_y > prior && res.start_y < top)
            {
                self.activate_match_number(pos);
            } else {
                self.activate_match_number(cur.saturating_sub(1));
            }
        }
    }

    /// Skip this page of matches and move up to the first match from
    /// the prior page.
    fn prior_match_page(&mut self) {
        let dims = self.delegate.get_dimensions();
        if let Some(cur) = self.result_pos {
            let top = self.viewport.unwrap_or(dims.physical_top);
            let bottom = top + dims.viewport_rows as isize;
            if let Some(pos) = self.results.iter().position(|res| res.start_y >= bottom) {
                self.activate_match_number(pos);
            } else {
                let len = self.results.len().saturating_sub(1);
                self.activate_match_number(cur.min(len));
            }
        }
    }

    fn get_pattern(&self) -> Pattern {
        let pattern = self.search_line.get_line().to_string();
        match self.pattern_type {
            PatternType::CaseSensitiveString => Pattern::CaseSensitiveString(pattern),
            PatternType::CaseInSensitiveString => Pattern::CaseInSensitiveString(pattern),
            PatternType::CaseSmartString => Pattern::CaseSmartString(pattern),
            PatternType::Regex => Pattern::Regex(pattern),
        }
    }

    /// Up (-1) and Down (1) in the search editor walk the search history,
    /// newest first. Down past the newest entry brings back the typed text.
    fn search_history(&mut self, direction: isize) {
        let history = SEARCH_HISTORY.lock().clone();
        if history.is_empty() {
            return;
        }
        let next = match (self.history_pos, direction < 0) {
            (None, true) => {
                self.history_draft = self.search_line.get_line().to_owned();
                // Skip an entry equal to the text in the editor.
                let mut pos = history.len() - 1;
                if history[pos] == self.history_draft && pos > 0 {
                    pos -= 1;
                }
                Some(pos)
            }
            (None, false) => return,
            (Some(pos), true) => Some(pos.saturating_sub(1)),
            (Some(pos), false) if pos + 1 < history.len() => Some(pos + 1),
            (Some(_), false) => None,
        };
        self.history_pos = next;
        let text = match next {
            Some(pos) => history[pos].clone(),
            None => std::mem::take(&mut self.history_draft),
        };
        self.search_line.set_line_and_cursor(&text, text.len());
        self.schedule_update_search();
    }

    fn clear_pattern(&mut self) {
        self.search_line.clear();
        self.update_search();
    }

    fn edit_pattern(&mut self) {
        self.activate_search_results=true;
        self.editing_search = true;
        self.update_key_table();
    }

    fn directed_pattern(&mut self, forward: bool) {
        self.directed_search=Some(forward);
        self.search_origin=(self.cursor.y,self.cursor.x);
        self.edit_pattern();
        self.update_search();
    }

    fn repeat_search(&mut self, reverse: bool) {
        self.activate_search_results=true;
        let forward=self.directed_search.unwrap_or(false)^reverse;
        let origin=(self.cursor.y,self.cursor.x);
        let selected=if forward {
            self.results.iter().rposition(|r|(r.start_y,r.start_x)>origin)
        } else {
            self.results.iter().position(|r|(r.end_y,r.end_x.saturating_sub(1))<origin)
        };
        if let Some(index)=selected {self.activate_match_number(index);}
    }

    fn accept_pattern(&mut self) {
        remember_search(self.search_line.get_line());
        self.history_pos = None;
        self.editing_search = false;
        self.update_key_table();
    }

    fn update_key_table(&mut self) {
        let window = self.window.clone();
        let pane_id = self.delegate.pane_id();

        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
            let mut state = term_window.pane_state(pane_id);
            if let Some(overlay) = state.overlay.as_mut() {
                if let Some(copy_overlay) = overlay.pane.downcast_ref::<CopyOverlay>() {
                    let editing_search = copy_overlay.render.lock().editing_search;

                    overlay.key_table_state.activate(KeyTableArgs {
                        name: if editing_search {
                            "search_mode"
                        } else {
                            "copy_mode"
                        },
                        timeout_milliseconds: None,
                        replace_current: true,
                        one_shot: false,
                        until_unknown: false,
                        prevent_fallback: false,
                        passthrough: false,
                    });
                }
            }
        })));
    }

    fn cycle_match_type(&mut self) {
        let pattern_type = match &self.pattern_type {
            PatternType::CaseSensitiveString => PatternType::CaseInSensitiveString,
            PatternType::CaseInSensitiveString => PatternType::CaseSmartString,
            PatternType::CaseSmartString => PatternType::Regex,
            PatternType::Regex => PatternType::CaseSensitiveString,
        };
        self.pattern_type = pattern_type;
        self.schedule_update_search();
    }

    fn move_to_viewport_middle(&mut self) {
        let dims = self.dimensions();
        self.cursor.y = dims.top + (dims.dims.viewport_rows as isize) / 2;
        self.select_to_cursor_pos();
    }

    fn move_to_viewport_top(&mut self) {
        let dims = self.dimensions();
        self.cursor.y = dims.top + dims.vertical_gap;
        self.select_to_cursor_pos();
    }

    fn move_to_viewport_bottom(&mut self) {
        let dims = self.dimensions();
        self.cursor.y = dims.top + (dims.dims.viewport_rows as isize) - dims.vertical_gap;
        self.select_to_cursor_pos();
    }

    fn move_left_single_cell(&mut self) {
        self.cursor.x = self.cursor.x.saturating_sub(1);
        self.select_to_cursor_pos();
    }

    fn move_right_single_cell(&mut self) {
        self.cursor.x += 1;
        self.select_to_cursor_pos();
    }

    fn move_up_single_row(&mut self) {
        self.cursor.y = self.cursor.y.saturating_sub(1);
        self.select_to_cursor_pos();
    }

    fn move_down_single_row(&mut self) {
        self.cursor.y += 1;
        self.select_to_cursor_pos();
    }
    fn move_to_start_of_line(&mut self) {
        self.cursor.x = 0;
        self.select_to_cursor_pos();
    }

    fn move_to_start_of_next_line(&mut self) {
        self.cursor.x = 0;
        self.cursor.y += 1;
        self.select_to_cursor_pos();
    }

    fn move_to_top(&mut self) {
        // This will get fixed up by clamp_cursor_to_scrollback
        self.cursor.y = 0;
        self.select_to_cursor_pos();
    }

    fn move_to_bottom(&mut self) {
        // This will get fixed up by clamp_cursor_to_scrollback
        self.cursor.y = isize::max_value();
        self.select_to_cursor_pos();
    }

    fn move_to_end_of_line_content(&mut self) {
        let y = self.cursor.y;
        let (top, lines) = self.delegate.get_lines(y..y + 1);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            self.cursor.x = 0;
            for cell in line.visible_cells() {
                if cell.str() != " " {
                    self.cursor.x = cell.cell_index();
                }
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_to_start_of_line_content(&mut self) {
        let y = self.cursor.y;
        let (top, lines) = self.delegate.get_lines(y..y + 1);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            self.cursor.x = 0;
            for cell in line.visible_cells() {
                if cell.str() != " " {
                    self.cursor.x = cell.cell_index();
                    break;
                }
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_to_selection_other_end(&mut self) {
        if let Some(old_start) = self.start {
            // Swap cursor & start of selection
            self.start
                .replace(SelectionCoordinate::x_y(self.cursor.x, self.cursor.y));
            self.cursor.x = match &old_start.x {
                SelectionX::Cell(x) => *x,
                SelectionX::BeforeZero => 0,
            };
            self.cursor.y = old_start.y;
            self.select_to_cursor_pos();
        }
    }

    fn move_to_selection_other_end_horiz(&mut self) {
        if self.selection_mode != SelectionMode::Block {
            return self.move_to_selection_other_end();
        }
        if let Some(old_start) = self.start {
            // Swap X coordinate of cursor & start of selection
            self.start
                .replace(SelectionCoordinate::x_y(self.cursor.x, old_start.y));
            self.cursor.x = match &old_start.x {
                SelectionX::Cell(x) => *x,
                SelectionX::BeforeZero => 0,
            };
            self.select_to_cursor_pos();
        }
    }

    fn move_backward_one_word(&mut self) {
        let y = if self.cursor.x == 0 && self.cursor.y > 0 {
            self.cursor.x = usize::max_value();
            self.cursor.y.saturating_sub(1)
        } else {
            self.cursor.y
        };

        let (top, lines) = self.delegate.get_lines(y..y + 1);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            if self.cursor.x == usize::max_value() {
                self.cursor.x = line.len().saturating_sub(1);
            }
            let s = line.columns_as_str(0..self.cursor.x.saturating_add(1));

            // "hello there you"
            //              |_
            //        |    _
            //  |    _
            //        |     _
            //  |     _

            let mut last_was_whitespace = false;

            for (idx, word) in s.split_word_bounds().rev().enumerate() {
                let width = unicode_column_width(word, None);

                if is_whitespace_word(word) {
                    self.cursor.x = self.cursor.x.saturating_sub(width);
                    last_was_whitespace = true;
                    continue;
                }
                last_was_whitespace = false;

                if idx == 0 && width == 1 {
                    // We were at the start of the initial word
                    self.cursor.x = self.cursor.x.saturating_sub(width);
                    continue;
                }

                self.cursor.x = self.cursor.x.saturating_sub(width.saturating_sub(1));
                break;
            }

            if last_was_whitespace && self.cursor.y > 0 {
                // The line begins with whitespace
                self.cursor.x = usize::max_value();
                self.cursor.y -= 1;
                return self.move_backward_one_word();
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_forward_one_word(&mut self) {
        let y = self.cursor.y;
        let (top, lines) = self.delegate.get_lines(y..y + 1);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            let width = line.len();
            let s = line.columns_as_str(self.cursor.x..width + 1);
            let mut words = s.split_word_bounds();

            if let Some(word) = words.next() {
                self.cursor.x += unicode_column_width(word, None);
                if !is_whitespace_word(word) {
                    if let Some(word) = words.next() {
                        if is_whitespace_word(word) {
                            self.cursor.x += unicode_column_width(word, None);
                        }
                    }
                }
            }

            if self.cursor.x >= width {
                let dims = self.delegate.get_dimensions();
                let max_row = dims.scrollback_top + dims.scrollback_rows as isize;
                if self.cursor.y + 1 < max_row {
                    self.cursor.y += 1;
                    return self.move_to_start_of_line_content();
                }
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_to_end_of_word(&mut self) {
        let y = self.cursor.y;
        let (top, lines) = self.delegate.get_lines(y..y + 1);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            let width = line.len();
            let s = line.columns_as_str(self.cursor.x..width + 1);
            let mut words = s.split_word_bounds();

            if self.cursor.x >= width - 1 {
                let dims = self.delegate.get_dimensions();
                let max_row = dims.scrollback_top + dims.scrollback_rows as isize;
                if self.cursor.y + 1 < max_row {
                    self.cursor.y += 1;
                    self.cursor.x = 0;
                    return self.move_to_end_of_word();
                }
            }

            if let Some(word) = words.next() {
                let mut word_end = self.cursor.x + unicode_column_width(word, None);
                if !is_whitespace_word(word) {
                    if self.cursor.x == word_end - 1 {
                        while let Some(next_word) = words.next() {
                            word_end += unicode_column_width(next_word, None);
                            if !is_whitespace_word(next_word) {
                                break;
                            }
                        }
                    }
                }
                while let Some(next_word) = words.next() {
                    if !is_whitespace_word(next_word) {
                        word_end += unicode_column_width(next_word, None);
                    } else {
                        break;
                    }
                }
                self.cursor.x = word_end - 1;
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_by_zone(&mut self, mut delta: isize, zone_type: Option<SemanticType>) {
        if delta == 0 {
            return;
        }

        let zones = self
            .delegate
            .get_semantic_zones()
            .unwrap_or_else(|_| vec![]);
        let mut idx = match zones.binary_search_by(|zone| {
            if zone.start_y == self.cursor.y {
                zone.start_x.cmp(&self.cursor.x)
            } else if zone.start_y < self.cursor.y {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }) {
            Ok(idx) | Err(idx) => idx,
        };

        let step = if delta > 0 { 1 } else { -1 };

        while delta != 0 {
            if step > 0 {
                idx = match idx.checked_add(1) {
                    Some(n) => n,
                    None => return,
                };
            } else {
                idx = match idx.checked_sub(1) {
                    Some(n) => n,
                    None => return,
                };
            }
            let zone = match zones.get(idx) {
                Some(z) => z,
                None => return,
            };
            if let Some(zone_type) = &zone_type {
                if zone.semantic_type != *zone_type {
                    continue;
                }
            }
            delta = delta.saturating_sub(step);

            self.cursor.x = zone.start_x;
            self.cursor.y = zone.start_y;
        }
        self.select_to_cursor_pos();
    }

    /// The rows that a jump looks at: from the cursor to 99 rows past the
    /// edge of the screen, in the direction of the jump (as vim-sneak does).
    /// vim w, b, e, ge (W, B, E, gE when big) as vim defines them: the end of
    /// a row is a blank, an empty row stops w, b and ge.
    fn vim_word(&mut self, motion: &str, big: bool) {
        let mut text = VimText::new(self.delegate.clone(), big);
        let row = text.row(self.cursor.y);
        let index = row.iter().rposition(|(x, _)| *x <= self.cursor.x).unwrap_or(0);
        let mut pos = (self.cursor.y, index.min(row.len().saturating_sub(1)));
        match motion {
            "w" => 'w: {
                let sclass = text.cls(pos);
                let Some(next) = text.inc(pos) else { break 'w };
                pos = next;
                if sclass != 0 {
                    while text.cls(pos) == sclass {
                        let Some(next) = text.inc(pos) else { break 'w };
                        pos = next;
                    }
                }
                while text.cls(pos) == 0 {
                    if pos.1 == 0 && text.empty(pos.0) { break; }
                    let Some(next) = text.inc(pos) else { break 'w };
                    pos = next;
                }
            }
            "b" => 'b: {
                let Some(prev) = text.dec(pos) else { break 'b };
                pos = prev;
                while text.cls(pos) == 0 {
                    if pos.1 == 0 && text.empty(pos.0) { break 'b; }
                    let Some(prev) = text.dec(pos) else { break 'b };
                    pos = prev;
                }
                let c = text.cls(pos);
                while let Some(prev) = text.dec(pos) {
                    if text.cls(prev) != c { break; }
                    pos = prev;
                }
            }
            "e" => 'e: {
                let sclass = text.cls(pos);
                let Some(next) = text.inc(pos) else { break 'e };
                pos = next;
                if !(text.cls(pos) == sclass && sclass != 0) {
                    while text.cls(pos) == 0 {
                        let Some(next) = text.inc(pos) else { break 'e };
                        pos = next;
                    }
                }
                let c = text.cls(pos);
                while let Some(next) = text.inc(pos) {
                    if text.cls(next) != c { break; }
                    pos = next;
                }
            }
            "ge" => 'g: {
                // vim bckend_word: back to the end of the previous word.
                let sclass = text.cls(pos);
                let Some(prev) = text.dec(pos) else { break 'g };
                pos = prev;
                if sclass != 0 {
                    while text.cls(pos) == sclass {
                        let Some(prev) = text.dec(pos) else { break 'g };
                        pos = prev;
                    }
                }
                while text.cls(pos) == 0 {
                    if pos.1 == 0 && text.empty(pos.0) { break; }
                    let Some(prev) = text.dec(pos) else { break 'g };
                    pos = prev;
                }
            }
            _ => return,
        }
        let x = text.row(pos.0).get(pos.1).map_or(0, |(x, _)| *x);
        self.cursor.y = pos.0;
        self.cursor.x = x;
        self.select_to_cursor_pos();
    }

    fn sneak_scope(&self, forward: bool, from_y: StableRowIndex) -> Range<StableRowIndex> {
        let dims = self.delegate.get_dimensions();
        let top = self.viewport.unwrap_or(dims.physical_top);
        let end = dims.scrollback_top + dims.scrollback_rows as isize;
        if forward {
            from_y..(top + dims.viewport_rows as isize + 99).min(end).max(from_y + 1)
        } else {
            (top - 99).max(dims.scrollback_top).min(from_y)..from_y + 1
        }
    }

    /// The matches of `target` after (forward) or before `from`, nearest
    /// first: (row, first cell, width in cells). Smart case: a target with
    /// an upper case letter matches only that case.
    fn sneak_find(&self, target: &str, forward: bool, from: (StableRowIndex, usize)) -> Vec<(StableRowIndex, usize, usize)> {
        let want: Vec<String> = target.chars().map(|c| c.to_string()).collect();
        if want.is_empty() { return vec![]; }
        let fold = !target.chars().any(|c| c.is_uppercase());
        let same = |cell: &str, want: &str| if fold { cell.to_lowercase() == want.to_lowercase() } else { cell == want };
        let rows = self.sneak_scope(forward, from.0);
        let (first_row, lines) = self.delegate.get_lines(rows);
        let mut found = vec![];
        for (i, line) in lines.iter().enumerate() {
            let y = first_row + i as StableRowIndex;
            let cells: Vec<(usize, String, usize)> = line.visible_cells()
                .map(|c| (c.cell_index(), c.str().to_owned(), c.width().max(1))).collect();
            for k in 0..cells.len() {
                if k + want.len() > cells.len() { break; }
                if !(0..want.len()).all(|j| same(&cells[k + j].1, &want[j])) { continue; }
                let x = cells[k].0;
                let last = &cells[k + want.len() - 1];
                let width = last.0 + last.2 - x;
                let beyond = if forward { (y, x) > from } else { (y, x) < from };
                if beyond { found.push((y, x, width)); }
            }
        }
        if !forward { found.reverse(); }
        found
    }

    fn sneak_mark(&mut self, y: StableRowIndex, cells: Range<usize>, label: Option<char>) {
        self.dirty_results.add(y);
        self.sneak_marks.entry(y).or_default().push((cells, label));
    }

    /// Take away the jump highlights and labels.
    fn sneak_clear(&mut self) {
        if self.sneak_marks.is_empty() && self.sneak_labels.is_empty() { return; }
        for y in self.sneak_marks.keys() { self.dirty_results.add(*y); }
        self.sneak_marks.clear();
        self.sneak_labels.clear();
        self.sneak_rest.clear();
        self.window.invalidate();
    }

    /// Label mode: a label on each match on the screen after the cursor.
    fn sneak_place_labels(&mut self) {
        let dims = self.delegate.get_dimensions();
        let top = self.viewport.unwrap_or(dims.physical_top);
        let bottom = top + dims.viewport_rows as isize;
        let labels: Vec<char> = SNEAK_LABELS.chars().collect();
        let rest = std::mem::take(&mut self.sneak_rest);
        let mut placed = 0;
        for (n, &(y, x, _width)) in rest.iter().enumerate() {
            if y < top || y >= bottom { break; }
            if n >= labels.len() { break; }
            self.sneak_labels.push((labels[n], y, x));
            self.sneak_mark(y, x..x + 1, Some(labels[n]));
            placed = n + 1;
        }
        // Tab labels the matches after these ones.
        self.sneak_rest = rest[placed..].to_vec();
        self.window.invalidate();
    }

    /// Move the cursor to a jump target; in visual mode the selection follows.
    fn sneak_move(&mut self, y: StableRowIndex, x: usize) {
        self.cursor.y = y;
        self.cursor.x = x;
        self.select_to_cursor_pos();
    }

    fn perform_jump(&mut self, jump: &Jump, forward: bool, repeat: bool) {
        let mut from = (self.cursor.y, self.cursor.x);
        // A repeated t-like jump does not stop again before the same character.
        if jump.prev_char && repeat {
            if forward { from.1 += 1; } else { from.1 = from.1.saturating_sub(1); }
        }
        let found = self.sneak_find(&jump.target, forward, from);
        self.sneak_clear();
        self.sneak_active = true;
        let Some(&(y, x, width)) = found.first() else {
            self.delegate.perform_actions(vec![termwiz::escape::Action::Control(termwiz::escape::ControlCode::Bell)]);
            return;
        };
        let x = match (jump.prev_char, forward) {
            (false, _) => x,
            (true, true) => x.saturating_sub(1),
            (true, false) => x + width,
        };
        self.sneak_move(y, x);
        // A match of two characters shows where the cursor is.
        if jump.target.chars().count() > 1 { self.sneak_mark(y, x..x + width, None); }
        self.sneak_rest = found[1..].to_vec();
        let dims = self.delegate.get_dimensions();
        let top = self.viewport.unwrap_or(dims.physical_top);
        let on_screen = |y: StableRowIndex| y >= top && y < top + dims.viewport_rows as isize;
        // A first two-character jump with more matches on the screen: labels.
        if jump.label && !repeat && self.sneak_rest.first().is_some_and(|m| on_screen(m.0)) {
            self.sneak_place_labels();
        } else {
            for (y, x, width) in std::mem::take(&mut self.sneak_rest) { self.sneak_mark(y, x..x + width, None); }
        }
        self.window.invalidate();
    }

    /// f, F, t, T: ask for the target; right after a jump of the same kind, go
    /// to the next match instead (clever-f). As in vim-sneak with
    /// sneak#s_next, the key of the first jump goes on in its direction and
    /// the other key goes back.
    fn jump(&mut self, forward: bool, prev_char: bool, label: bool) {
        if self.sneak_active {
            if let Some(last) = self.last_jump.clone() {
                if last.prev_char == prev_char && last.label == label {
                    self.perform_jump(&last, last.forward == forward, true);
                    return;
                }
            }
        }
        self.sneak_clear();
        self.sneak_active = false;
        self.pending_jump.replace(PendingJump { forward, prev_char, label, typed: String::new() });
    }

    fn jump_again(&mut self, reverse: bool) {
        if let Some(jump) = self.last_jump.clone() {
            self.perform_jump(&jump, jump.forward != reverse, true);
        }
    }

    /// A key while the labels show: a label jumps there; Tab labels the
    /// next matches; Escape or Space ends; any other key ends and does
    /// what it does. Returns the assignment of that other key.
    fn sneak_label_key(&mut self, key: KeyCode, mods: KeyModifiers) -> Option<KeyAssignment> {
        let deferred = self.sneak_deferred.take();
        let plain = mods == KeyModifiers::NONE || mods == KeyModifiers::SHIFT;
        match key {
            KeyCode::Char(c) if plain && (c == ':' || self.sneak_labels.iter().any(|l| l.0 == c)) => {
                let target = if c == ':' { self.sneak_labels.first() } else { self.sneak_labels.iter().find(|l| l.0 == c) };
                if let Some(&(_, y, x)) = target {
                    self.sneak_clear();
                    self.sneak_active = false;
                    self.sneak_move(y, x);
                }
                None
            }
            KeyCode::Tab | KeyCode::Char('\t') if mods == KeyModifiers::NONE => {
                match self.sneak_rest.first().copied() {
                    Some((y, x, _)) => {
                        let rest = self.sneak_rest[1..].to_vec();
                        self.sneak_clear();
                        self.sneak_move(y, x);
                        self.sneak_rest = rest;
                        self.sneak_place_labels();
                    }
                    None => { self.sneak_clear(); self.sneak_active = false; }
                }
                None
            }
            KeyCode::Escape | KeyCode::Char(' ') | KeyCode::Char('\u{1b}') => {
                self.sneak_clear();
                self.sneak_active = false;
                None
            }
            _ => {
                self.sneak_clear();
                self.sneak_active = false;
                deferred
            }
        }
    }

    /// The pane takes the next keys itself (the target of f/t, a label).
    pub fn captures_keys(&self) -> bool {
        self.pending_jump.is_some() || self.pending_g || !self.sneak_labels.is_empty()
    }

    fn set_selection_mode(&mut self, mode: &Option<SelectionMode>) {
        match mode {
            None => self.clear_selection_mode(),
            Some(mode) => {
                if self.start.is_none() {
                    let coord = SelectionCoordinate::x_y(self.cursor.x, self.cursor.y);
                    self.start.replace(coord);
                } else if self.selection_mode == *mode {
                    // We have a selection and we're trying to set the same mode
                    // again; consider this to be a toggle that clears the selection
                    self.clear_selection_mode();
                    return;
                }
                self.selection_mode = *mode;
                self.select_to_cursor_pos();
            }
        }
    }

    fn clear_selection_mode(&mut self) {
        self.start.take();
        self.clear_selection();
    }

    /// vim gv: the last visual selection of this pane again (also from an
    /// earlier copy mode), the cursor at its end.
    fn reselect_last(&mut self) {
        let Some((start, x, y, mode)) = LAST_SELECTION.lock().get(&self.delegate.pane_id()).copied() else { return };
        self.start = Some(start);
        self.selection_mode = mode;
        self.cursor.x = x;
        self.cursor.y = y;
        self.select_to_cursor_pos();
    }
}

impl Pane for CopyOverlay {
    fn copy_mode_scroll_position(&self) -> Option<(isize, usize)> {
        let render=self.render.lock();
        let dims=self.delegate.get_dimensions();
        let search=!config::configuration().copy_mode_pane_status && (render.editing_search || !render.get_pattern().is_empty());
        Some((-render.viewport.unwrap_or(dims.physical_top),dims.viewport_rows.saturating_sub(usize::from(search))))
    }

    fn popup_frame(&self) -> Option<mux::pane::PopupFrame> {
        self.delegate.popup_frame()
    }

    fn pane_id(&self) -> PaneId {
        self.delegate.pane_id()
    }

    fn get_title(&self) -> String {
        // The tab keeps its title: the mode bar shows copy mode.
        self.delegate.get_title()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        // paste into the search bar
        let mut r = self.render.lock();
        r.search_line.insert_text(text);
        r.schedule_update_search();
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.delegate.resize(size)
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let mut render = self.render.lock();
        let mods = mods.remove_positional_mods();
        // Shift (for F, T or an upper case target) comes as a key of its own:
        // it must not end a jump that waits for its target or a label.
        if key.is_modifier() && (render.pending_jump.is_some() || render.pending_g || !render.sneak_labels.is_empty()) {
            return Ok(());
        }
        if std::mem::take(&mut render.pending_g) {
            match (key, mods) {
                (KeyCode::Char('g'), KeyModifiers::NONE) => render.move_to_top(),
                (KeyCode::Char('v'), KeyModifiers::NONE) => render.reselect_last(),
                (KeyCode::Char('e'), KeyModifiers::NONE) => render.vim_word("ge", false),
                (KeyCode::Char('E'), KeyModifiers::NONE) | (KeyCode::Char('E'), KeyModifiers::SHIFT) => render.vim_word("ge", true),
                // Any other key (Escape) ends the g.
                _ => {}
            }
            return Ok(());
        }
        if !render.sneak_labels.is_empty() {
            if let Some(assignment) = render.sneak_label_key(key, mods) {
                // The key that ended label mode does what it does.
                let pane_id = self.delegate.pane_id();
                render.window.notify(TermWindowNotif::PerformAssignment { pane_id, assignment, tx: None });
            }
            return Ok(());
        }
        if let Some(mut pending) = render.pending_jump.take() {
            let mut target = None;
            match (key, mods) {
                (KeyCode::Char(c), KeyModifiers::NONE)
                | (KeyCode::Char(c), KeyModifiers::SHIFT) if c != '\u{1b}' && c != '\r' && c != '\t' && c != '\u{8}' && c != '\u{7f}' => {
                    pending.typed.push(c);
                    if pending.typed.chars().count() >= if pending.label { 2 } else { 1 } {
                        target = Some(pending.typed.clone());
                    } else {
                        render.pending_jump = Some(pending.clone());
                    }
                }
                // Enter: the characters typed so far, or the last target.
                (KeyCode::Enter, _) | (KeyCode::Char('\r'), _) => {
                    target = if pending.typed.is_empty() { render.last_jump.as_ref().map(|j| j.target.clone()) } else { Some(pending.typed.clone()) };
                }
                (KeyCode::Escape, _) | (KeyCode::Char('\u{1b}'), _) | (KeyCode::Backspace, _) | (KeyCode::Char('C'), KeyModifiers::CTRL) | (KeyCode::Char('c'), KeyModifiers::CTRL) => {}
                _ => {
                    self.delegate
                        .perform_actions(vec![termwiz::escape::Action::Control(
                            termwiz::escape::ControlCode::Bell,
                        )]);
                }
            }
            if let Some(target) = target {
                let jump = Jump { forward: pending.forward, prev_char: pending.prev_char, label: pending.label, target };
                render.last_jump.replace(jump.clone());
                render.perform_jump(&jump, jump.forward, false);
            }
            return Ok(());
        }

        if render.editing_search {
            let key = match key { KeyCode::Char(c) if mods == KeyModifiers::CTRL => KeyCode::Char(c.to_ascii_uppercase()), key => key };
            match (key, mods) {
                (KeyCode::Char(c), KeyModifiers::NONE)
                | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                    // Type to add to the pattern
                    render.search_line.insert_char(c);

                    render.schedule_update_search();
                }
                (KeyCode::Char('H'), KeyModifiers::CTRL)
                | (KeyCode::Backspace, KeyModifiers::NONE) => {
                    render
                        .search_line
                        .kill_text(Movement::BackwardChar(1), Movement::BackwardChar(1));

                    render.schedule_update_search();
                }
                (KeyCode::Delete, KeyModifiers::NONE) => {
                    render
                        .search_line
                        .kill_text(Movement::ForwardChar(1), Movement::None);

                    render.schedule_update_search();
                }
                (KeyCode::Backspace, KeyModifiers::ALT)
                | (KeyCode::Char('W'), KeyModifiers::CTRL) => {
                    render
                        .search_line
                        .kill_text(Movement::BackwardWord(1), Movement::BackwardWord(1));

                    render.schedule_update_search();
                }
                (KeyCode::Backspace, KeyModifiers::SUPER) => {
                    render
                        .search_line
                        .kill_text(Movement::StartOfLine, Movement::StartOfLine);

                    render.schedule_update_search();
                }
                (KeyCode::Char('K'), KeyModifiers::CTRL) => {
                    render
                        .search_line
                        .kill_text(Movement::EndOfLine, Movement::EndOfLine);

                    render.schedule_update_search();
                }
                (KeyCode::Char('B'), KeyModifiers::CTRL)
                | (KeyCode::ApplicationLeftArrow, KeyModifiers::NONE)
                | (KeyCode::LeftArrow, KeyModifiers::NONE) => {
                    render.search_line.exec_movement(Movement::BackwardChar(1));
                }
                (KeyCode::Char('F'), KeyModifiers::CTRL)
                | (KeyCode::ApplicationRightArrow, KeyModifiers::NONE)
                | (KeyCode::RightArrow, KeyModifiers::NONE) => {
                    render.search_line.exec_movement(Movement::ForwardChar(1));
                }
                (KeyCode::ApplicationLeftArrow, KeyModifiers::CTRL)
                | (KeyCode::LeftArrow, KeyModifiers::CTRL) => {
                    render.search_line.exec_movement(Movement::BackwardWord(1));
                }
                (KeyCode::ApplicationRightArrow, KeyModifiers::CTRL)
                | (KeyCode::RightArrow, KeyModifiers::CTRL) => {
                    render.search_line.exec_movement(Movement::ForwardWord(1));
                }
                (KeyCode::Char('A'), KeyModifiers::CTRL) | (KeyCode::Home, KeyModifiers::NONE) => {
                    render.search_line.exec_movement(Movement::StartOfLine);
                }
                (KeyCode::Char('E'), KeyModifiers::CTRL) | (KeyCode::End, KeyModifiers::NONE) => {
                    render.search_line.exec_movement(Movement::EndOfLine);
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn perform_assignment(&self, assignment: &KeyAssignment) -> PerformAssignmentResult {
        use CopyModeAssignment::*;
        let mut render = self.render.lock();
        if render.editing_search && !matches!(assignment, KeyAssignment::CopyMode(_) | KeyAssignment::SendKey(_)) {
            return PerformAssignmentResult::BlockAssignmentAndRouteToKeyDown;
        }
        if render.pending_jump.is_some() || render.pending_g {
            // Block key assignments until key_down is called
            // and resolves the next state
            return PerformAssignmentResult::BlockAssignmentAndRouteToKeyDown;
        }
        if !render.sneak_labels.is_empty() {
            // A label or another key: key_down decides, and does this
            // assignment when the key is not a label.
            render.sneak_deferred = Some(assignment.clone());
            return PerformAssignmentResult::BlockAssignmentAndRouteToKeyDown;
        }
        match assignment {
            KeyAssignment::CopyMode(assignment) => {
                if !matches!(assignment, JumpForward{..} | JumpBackward{..} | JumpAgain | JumpReverse | Sneak{..}) {
                    // Another motion ends the jump: its highlights go.
                    render.sneak_clear();
                    render.sneak_active = false;
                }
                if !matches!(assignment, HighlightCommands(_) | CommandSelection(_) | SelectCommand{..} | SetSelectionMode(_) | ClearSelectionMode) {
                    render.set_command_highlight(false);
                }
                let was_command_selection = render.command_selection;
                if !matches!(assignment, HighlightCommands(_) | CommandSelection(_) | SelectCommand{..}) { render.command_selection=false; }
                match assignment {
                    // v / V right after a command jump: the command becomes
                    // the start of a visual selection (no toggle).
                    SetSelectionMode(Some(mode)) if was_command_selection && render.start.is_some() => {
                        render.selection_mode = *mode;
                        render.select_to_cursor_pos();
                    }
                    SelectCommand{direction,prompt_marker,select} => render.select_command(*direction,prompt_marker.as_deref(),*select),
                    CommandSelection(enabled) => { render.command_selection=*enabled; render.window.invalidate(); }
                    MoveToCoordinates{x,y} => {
                        let dims=render.delegate.get_dimensions();
                        render.cursor.x=(*x).min(dims.cols.saturating_sub(1));
                        render.cursor.y=(*y).clamp(dims.scrollback_top,dims.physical_top+dims.viewport_rows as isize-1);
                        render.select_to_cursor_pos();
                    }
                    MoveToViewportBottom => render.move_to_viewport_bottom(),
                    MoveToViewportTop => render.move_to_viewport_top(),
                    MoveToViewportMiddle => render.move_to_viewport_middle(),
                    MoveToScrollbackTop => render.move_to_top(),
                    MoveToScrollbackBottom => render.move_to_bottom(),
                    MoveToStartOfLineContent => render.move_to_start_of_line_content(),
                    MoveToEndOfLineContent => render.move_to_end_of_line_content(),
                    MoveToStartOfLine => render.move_to_start_of_line(),
                    MoveToStartOfNextLine => render.move_to_start_of_next_line(),
                    MoveToSelectionOtherEnd => render.move_to_selection_other_end(),
                    MoveToSelectionOtherEndHoriz => render.move_to_selection_other_end_horiz(),
                    MoveBackwardWord => render.move_backward_one_word(),
                    MoveForwardWord => render.move_forward_one_word(),
                    MoveForwardWordEnd => render.move_to_end_of_word(),
                    MoveRight => render.move_right_single_cell(),
                    MoveLeft => render.move_left_single_cell(),
                    MoveUp => render.move_up_single_row(),
                    MoveDown => render.move_down_single_row(),
                    MoveByPage(n) => render.move_by_page(**n),
                    PageUp => render.move_by_page(-1.0),
                    PageDown => render.move_by_page(1.0),
                    Close => render.close(),
                    PriorMatch => render.prior_match(),
                    NextMatch => render.next_match(),
                    PriorMatchPage => render.prior_match_page(),
                    NextMatchPage => render.next_match_page(),
                    CycleMatchType => render.cycle_match_type(),
                    ClearPattern => render.clear_pattern(),
                    EditPattern => render.edit_pattern(),
                    SearchForward => render.directed_pattern(true),
                    SearchBackward => render.directed_pattern(false),
                    SearchAgain => render.repeat_search(false),
                    SearchReverse => render.repeat_search(true),
                    HighlightCommands(enabled) => render.set_command_highlight(*enabled),
                    SearchHistory(direction) => render.search_history(*direction),
                    ScrollByLine(amount) => render.scroll_by_line(*amount),
                    ScrollViewport(amount) => render.scroll_viewport(*amount),
                    AcceptPattern => render.accept_pattern(),
                    SetSelectionMode(mode) => render.set_selection_mode(mode),
                    ClearSelectionMode => render.clear_selection_mode(),
                    MoveBackwardSemanticZone => render.move_by_zone(-1, None),
                    MoveForwardSemanticZone => render.move_by_zone(1, None),
                    MoveBackwardZoneOfType(zone_type) => render.move_by_zone(-1, Some(*zone_type)),
                    MoveForwardZoneOfType(zone_type) => render.move_by_zone(1, Some(*zone_type)),
                    JumpForward { prev_char } => render.jump(true, *prev_char, false),
                    JumpBackward { prev_char } => render.jump(false, *prev_char, false),
                    Sneak { forward } => render.jump(*forward, false, true),
                    VimWord { motion, big } => render.vim_word(motion, *big),
                    ReselectLast => render.reselect_last(),
                    GPrefix => render.pending_g = true,
                    JumpAgain => render.jump_again(false),
                    JumpReverse => render.jump_again(true),
                }
                PerformAssignmentResult::Handled
            }
            _ => PerformAssignmentResult::Unhandled,
        }
    }

    fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
        anyhow::bail!("ignoring mouse while copying");
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        self.delegate.perform_actions(actions)
    }

    fn is_dead(&self) -> bool {
        self.delegate.is_dead()
    }

    fn palette(&self) -> ColorPalette {
        self.delegate.palette()
    }

    fn copy_user_vars(&self) -> std::collections::HashMap<String,String> {
        self.delegate.copy_user_vars()
    }

    fn domain_id(&self) -> DomainId {
        self.delegate.domain_id()
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        self.delegate.erase_scrollback(erase_mode)
    }

    fn is_mouse_grabbed(&self) -> bool {
        // Force grabbing off while we're searching
        false
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.delegate.set_clipboard(clipboard)
    }

    fn get_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        self.delegate.get_current_working_dir(policy)
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let renderer = self.render.lock();
        if renderer.editing_search {
            // place in the search box
            // Padding between the start of the editable line and the left side of the terminal
            const SEARCH_CURSOR_PADDING: usize = 8;
            let cursor = unicode_column_width(
                &renderer.search_line.get_line()[0..renderer.search_line.get_cursor()],
                None,
            );
            StableCursorPosition {
                x: SEARCH_CURSOR_PADDING + cursor,
                y: renderer.compute_search_row(),
                shape: termwiz::surface::CursorShape::SteadyBlock,
                visibility: if config::configuration().copy_mode_pane_status { CursorVisibility::Hidden } else { CursorVisibility::Visible },
            }
        } else {
            renderer.cursor
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.delegate.get_current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let mut dirty = self.delegate.get_changed_since(lines.clone(), seqno);
        dirty.add_set(&self.render.lock().dirty_results);
        dirty.intersection_with_range(lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        self.delegate.get_logical_lines(lines)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        self.delegate
            .for_each_logical_line_in_stable_range_mut(lines, for_line);
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        // Take care to access self.delegate methods here before we get into
        // calling into its own with_lines_mut to avoid a runtime
        // lock erro!
        let mut renderer = self.render.lock();
        if self.delegate.get_current_seqno() > renderer.last_result_seqno {
            renderer.update_search();
        }
        renderer.check_for_resize();
        let dims = self.get_dimensions();
        let search_row = renderer.compute_search_row();

        struct OverlayLines<'a> {
            with_lines: &'a mut dyn WithPaneLines,
            dims: RenderableDimensions,
            search_row: StableRowIndex,
            renderer: &'a mut CopyRenderable,
        }

        self.delegate.with_lines_mut(
            lines,
            &mut OverlayLines {
                with_lines,
                dims,
                search_row,
                renderer: &mut *renderer,
            },
        );

        impl<'a> WithPaneLines for OverlayLines<'a> {
            fn with_lines_mut(&mut self, first_row: StableRowIndex, lines: &mut [&mut Line]) {
                let mut overlay_lines = vec![];
                let config = config::configuration();
                let colors = &config.resolved_palette;

                for (idx, line) in lines.iter_mut().enumerate() {
                    let mut line: Line = line.clone();

                    let stable_idx = idx as StableRowIndex + first_row;
                    if self.renderer.highlight_commands { highlight_commands(&mut line,self.dims.cols,&config); }
                    self.renderer.dirty_results.remove(stable_idx);
                    let pattern = self.renderer.get_pattern();
                    if !config.copy_mode_pane_status && stable_idx == self.search_row
                        && (self.renderer.editing_search || !pattern.is_empty())
                    {
                        // Replace with search UI
                        let rev = CellAttributes::default().set_reverse(true).clone();
                        line.fill_range(0..self.dims.cols, &Cell::new(' ', rev.clone()), SEQ_ZERO);
                        let mode = &match pattern {
                            Pattern::CaseSensitiveString(_) => "case-sensitive",
                            Pattern::CaseInSensitiveString(_) => "ignore-case",
                            Pattern::CaseSmartString(_) => "smart-case",
                            Pattern::Regex(_) => "regex",
                        };

                        let remain = match &self.renderer.searching {
                            Some(Searching { remain, .. }) => {
                                format!(" searching {remain} lines")
                            }
                            None => String::new(),
                        };

                        line.overlay_text_with_attribute(
                            0,
                            &format!(
                                "Search: {} ({}/{} matches. {}{remain})",
                                *pattern,
                                self.renderer.result_pos.map(|x| x + 1).unwrap_or(0),
                                self.renderer.results.len(),
                                mode
                            ),
                            rev,
                            SEQ_ZERO,
                        );
                        self.renderer.last_bar_pos = Some(self.search_row);
                        line.clear_appdata();
                    } else if let Some(matches) = self.renderer.by_line.get(&stable_idx) {
                        for m in matches {
                            // highlight
                            for cell_idx in m.range.clone() {
                                if let Some(cell) =
                                    line.cells_mut_for_attr_changes_only().get_mut(cell_idx)
                                {
                                    cell.attrs_mut()
                                            .set_background(
                                                colors
                                                    .copy_mode_inactive_highlight_bg
                                                    .unwrap_or(AnsiColor::Fuchsia.into()),
                                            )
                                            .set_foreground(
                                                colors
                                                    .copy_mode_inactive_highlight_fg
                                                    .unwrap_or(AnsiColor::Black.into()),
                                            )
                                            .set_reverse(false);
                                }
                            }
                        }
                        line.clear_appdata();
                    }
                    if let Some(marks) = self.renderer.sneak_marks.get(&stable_idx) {
                        draw_sneak_marks(&mut line, marks);
                    }
                    overlay_lines.push(line);
                }

                let mut overlay_refs: Vec<&mut Line> = overlay_lines.iter_mut().collect();
                self.with_lines.with_lines_mut(first_row, &mut overlay_refs);
            }
        }
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut renderer = self.render.lock();
        if self.delegate.get_current_seqno() > renderer.last_result_seqno {
            renderer.update_search();
        }

        renderer.check_for_resize();
        let dims = self.get_dimensions();

        let (top, mut lines) = self.delegate.get_lines(lines);

        let config = config::configuration();
        let colors = &config.resolved_palette;

        // Process the lines; for the search row we want to render instead
        // the search UI.
        // For rows with search results, we want to highlight the matching ranges
        let search_row = renderer.compute_search_row();
        for (idx, line) in lines.iter_mut().enumerate() {
            let stable_idx = idx as StableRowIndex + top;
            if renderer.highlight_commands { highlight_commands(line,dims.cols,&config); }
            renderer.dirty_results.remove(stable_idx);
            let pattern = renderer.get_pattern();
            if !config.copy_mode_pane_status && stable_idx == search_row && (renderer.editing_search || !pattern.is_empty()) {
                // Replace with search UI
                let rev = CellAttributes::default().set_reverse(true).clone();
                line.fill_range(0..dims.cols, &Cell::new(' ', rev.clone()), SEQ_ZERO);
                let mode = &match pattern {
                    Pattern::CaseSensitiveString(_) => "case-sensitive",
                    Pattern::CaseInSensitiveString(_) => "ignore-case",
                    Pattern::CaseSmartString(_) => "smart-case",
                    Pattern::Regex(_) => "regex",
                };
                line.overlay_text_with_attribute(
                    0,
                    &format!(
                        "Search: {} ({}/{} matches. {})",
                        *pattern,
                        renderer.result_pos.map(|x| x + 1).unwrap_or(0),
                        renderer.results.len(),
                        mode
                    ),
                    rev,
                    SEQ_ZERO,
                );
                renderer.last_bar_pos = Some(search_row);
            } else if let Some(matches) = renderer.by_line.get(&stable_idx) {
                for m in matches {
                    // highlight
                    for cell_idx in m.range.clone() {
                        if let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(cell_idx)
                        {
                            cell.attrs_mut()
                                    .set_background(
                                        colors
                                            .copy_mode_inactive_highlight_bg
                                            .unwrap_or(AnsiColor::Fuchsia.into()),
                                    )
                                    .set_foreground(
                                        colors
                                            .copy_mode_inactive_highlight_fg
                                            .unwrap_or(AnsiColor::Black.into()),
                                    )
                                    .set_reverse(false);
                        }
                    }
                }
            }
            if let Some(marks) = renderer.sneak_marks.get(&stable_idx) {
                draw_sneak_marks(line, marks);
            }
        }

        (top, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.delegate.get_dimensions()
    }
}

/// The jump highlights of vim-sneak (SneakLabel in red, bold, underlined),
/// and the labels in place of the first character of their match.
/// The text of a pane as vim sees it for its word motions: each row is its
/// cells (column, first char), then '\n' after the text of a row that does
/// not wrap (trailing blanks are padding: a blank row is an empty line).
struct VimText {
    pane: Arc<dyn Pane>,
    rows: HashMap<StableRowIndex, Vec<(usize, char)>>,
    top: StableRowIndex,
    bottom: StableRowIndex,
    big: bool,
}

impl VimText {
    fn new(pane: Arc<dyn Pane>, big: bool) -> Self {
        let dims = pane.get_dimensions();
        let top = dims.scrollback_top;
        let bottom = dims.scrollback_top + dims.scrollback_rows as StableRowIndex;
        Self { pane, rows: HashMap::new(), top, bottom, big }
    }
    fn row(&mut self, y: StableRowIndex) -> &Vec<(usize, char)> {
        let pane = &self.pane;
        self.rows.entry(y).or_insert_with(|| {
            let (_, lines) = pane.get_lines(y..y + 1);
            let Some(line) = lines.get(0) else { return vec![(0, '\n')] };
            let mut row: Vec<(usize, char)> = line.visible_cells()
                .map(|cell| (cell.cell_index(), cell.str().chars().next().unwrap_or(' ')))
                .collect();
            if line.last_cell_was_wrapped() && !row.is_empty() { return row; }
            while row.last().is_some_and(|(_, c)| c.is_whitespace()) { row.pop(); }
            let end = row.last().map_or(0, |(x, _)| x + 1);
            row.push((end, '\n'));
            row
        })
    }
    /// 0 for a blank (and the end of a row), 1 for punctuation (any
    /// non-blank for a WORD), 2 for letters, digits and _.
    fn cls(&mut self, pos: (StableRowIndex, usize)) -> u8 {
        let big = self.big;
        let c = self.row(pos.0).get(pos.1).map_or('\n', |(_, c)| *c);
        if c.is_whitespace() || c == '\n' { 0 }
        else if big { 1 }
        else if c.is_alphanumeric() || c == '_' { 2 }
        else { 1 }
    }
    fn empty(&mut self, y: StableRowIndex) -> bool {
        let row = self.row(y);
        row.len() == 1 && row[0].1 == '\n'
    }
    /// The next character; None at the end of the history.
    fn inc(&mut self, pos: (StableRowIndex, usize)) -> Option<(StableRowIndex, usize)> {
        if pos.1 + 1 < self.row(pos.0).len() { Some((pos.0, pos.1 + 1)) }
        else if pos.0 + 1 < self.bottom { Some((pos.0 + 1, 0)) } else { None }
    }
    /// The previous character; None at the top of the history.
    fn dec(&mut self, pos: (StableRowIndex, usize)) -> Option<(StableRowIndex, usize)> {
        if pos.1 > 0 { Some((pos.0, pos.1 - 1)) }
        else if pos.0 > self.top { let len = self.row(pos.0 - 1).len(); Some((pos.0 - 1, len.saturating_sub(1))) }
        else { None }
    }
}

fn draw_sneak_marks(line: &mut Line, marks: &[(Range<usize>, Option<char>)]) {
    for (cells, label) in marks {
        for idx in cells.clone() {
            let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(idx) else { continue };
            cell.attrs_mut()
                .set_foreground(AnsiColor::Maroon)
                .set_intensity(termwiz::cell::Intensity::Bold)
                .set_underline(termwiz::cell::Underline::Single)
                .set_reverse(false);
        }
        if let Some(label) = label {
            if let Some(attrs) = line.get_cell(cells.start).map(|c| c.attrs().clone()) {
                line.set_cell(cells.start, Cell::new(*label, attrs), SEQ_ZERO);
            }
        }
    }
    line.clear_appdata();
}

pub struct SearchOverlayPatternWriter {
    render: Arc<Mutex<CopyRenderable>>,
}

impl std::io::Write for SearchOverlayPatternWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut render = self.render.lock();
        let s = std::str::from_utf8(buf).map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("invalid UTF-8: {err:#}"))
        })?;
        render.search_line.insert_text(s);
        render.schedule_update_search();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn is_whitespace_word(word: &str) -> bool {
    if let Some(c) = word.chars().next() {
        c.is_whitespace()
    } else {
        false
    }
}

pub fn search_key_table() -> KeyTable {
    let mut table = KeyTable::default();
    for (key, mods, action) in [
        (
            WKeyCode::Char('\x1b'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::Close),
        ),
        (
            WKeyCode::UpArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PriorMatch),
        ),
        (
            WKeyCode::Char('\r'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PriorMatch),
        ),
        (
            WKeyCode::Char('p'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::PriorMatch),
        ),
        (
            WKeyCode::PageUp,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PriorMatchPage),
        ),
        (
            WKeyCode::PageDown,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::NextMatchPage),
        ),
        (
            WKeyCode::Char('n'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::NextMatch),
        ),
        (
            WKeyCode::DownArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::NextMatch),
        ),
        (
            WKeyCode::Char('r'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::CycleMatchType),
        ),
        (
            WKeyCode::Char('u'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::ClearPattern),
        ),
    ] {
        table.insert((key, mods), KeyTableEntry { action });
    }
    table
}

fn scroll_to_bottom_and_close() -> KeyAssignment {
    KeyAssignment::Multiple(vec![
        KeyAssignment::ScrollToBottom,
        KeyAssignment::CopyMode(CopyModeAssignment::Close),
    ])
}

pub fn copy_key_table() -> KeyTable {
    let mut table = KeyTable::default();
    for (key, mods, action) in [
        (
            WKeyCode::Char('c'),
            Modifiers::CTRL,
            scroll_to_bottom_and_close(),
        ),
        (
            WKeyCode::Char('g'),
            Modifiers::CTRL,
            scroll_to_bottom_and_close(),
        ),
        (
            WKeyCode::Char('q'),
            Modifiers::NONE,
            scroll_to_bottom_and_close(),
        ),
        (
            WKeyCode::Char('\x1b'),
            Modifiers::NONE,
            scroll_to_bottom_and_close(),
        ),
        (
            WKeyCode::Char('h'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveLeft),
        ),
        (
            WKeyCode::LeftArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveLeft),
        ),
        (
            WKeyCode::Char('j'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveDown),
        ),
        (
            WKeyCode::DownArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveDown),
        ),
        (
            WKeyCode::Char('k'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveUp),
        ),
        (
            WKeyCode::UpArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveUp),
        ),
        (
            WKeyCode::Char('l'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveRight),
        ),
        (
            WKeyCode::RightArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveRight),
        ),
        (
            WKeyCode::RightArrow,
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWord),
        ),
        (
            WKeyCode::Char('f'),
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWord),
        ),
        (
            WKeyCode::Char('\t'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWord),
        ),
        (
            WKeyCode::Char('w'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWord),
        ),
        (
            WKeyCode::Char('e'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWordEnd),
        ),
        (
            WKeyCode::LeftArrow,
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveBackwardWord),
        ),
        (
            WKeyCode::Char('b'),
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveBackwardWord),
        ),
        (
            WKeyCode::Char('\t'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveBackwardWord),
        ),
        (
            WKeyCode::Char('b'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveBackwardWord),
        ),
        (
            WKeyCode::Char('0'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLine),
        ),
        (
            WKeyCode::Char('\r'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfNextLine),
        ),
        (
            WKeyCode::Char('$'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToEndOfLineContent),
        ),
        (
            WKeyCode::Char('$'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToEndOfLineContent),
        ),
        (
            WKeyCode::Char('m'),
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLineContent),
        ),
        (
            WKeyCode::Char('^'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLineContent),
        ),
        (
            WKeyCode::Char('^'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLineContent),
        ),
        (
            WKeyCode::Char(' '),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Cell,
            ))),
        ),
        (
            WKeyCode::Char('v'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Cell,
            ))),
        ),
        (
            WKeyCode::Char('V'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Line,
            ))),
        ),
        (
            WKeyCode::Char('V'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Line,
            ))),
        ),
        (
            WKeyCode::Char('v'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Block,
            ))),
        ),
        (
            WKeyCode::Char('G'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToScrollbackBottom),
        ),
        (
            WKeyCode::Char('G'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToScrollbackBottom),
        ),
        (
            WKeyCode::Char('g'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToScrollbackTop),
        ),
        (
            WKeyCode::Char('H'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportTop),
        ),
        (
            WKeyCode::Char('H'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportTop),
        ),
        (
            WKeyCode::Char('M'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportMiddle),
        ),
        (
            WKeyCode::Char('M'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportMiddle),
        ),
        (
            WKeyCode::Char('L'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportBottom),
        ),
        (
            WKeyCode::Char('L'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportBottom),
        ),
        (
            WKeyCode::PageUp,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PageUp),
        ),
        (
            WKeyCode::PageDown,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PageDown),
        ),
        (
            WKeyCode::Char('b'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::PageUp),
        ),
        (
            WKeyCode::Char('f'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::PageDown),
        ),
        (
            WKeyCode::Char('u'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveByPage(NotNan::new(-0.5).unwrap())),
        ),
        (
            WKeyCode::Char('d'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveByPage(NotNan::new(0.5).unwrap())),
        ),
        (
            WKeyCode::Char('o'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToSelectionOtherEnd),
        ),
        (
            WKeyCode::Char('O'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToSelectionOtherEndHoriz),
        ),
        (
            WKeyCode::Char('O'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToSelectionOtherEndHoriz),
        ),
        (
            WKeyCode::Char('y'),
            Modifiers::NONE,
            KeyAssignment::Multiple(vec![
                KeyAssignment::CopyTo(ClipboardCopyDestination::ClipboardAndPrimarySelection),
                scroll_to_bottom_and_close(),
            ]),
        ),
        (
            WKeyCode::Char(';'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpAgain),
        ),
        (
            WKeyCode::Char(','),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpReverse),
        ),
        (
            WKeyCode::Char('F'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpBackward { prev_char: false }),
        ),
        (
            WKeyCode::Char('F'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpBackward { prev_char: false }),
        ),
        (
            WKeyCode::Char('T'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpBackward { prev_char: true }),
        ),
        (
            WKeyCode::Char('T'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpBackward { prev_char: true }),
        ),
        (
            WKeyCode::Char('f'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpForward { prev_char: false }),
        ),
        (
            WKeyCode::Char('t'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpForward { prev_char: true }),
        ),
        (
            WKeyCode::Home,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLine),
        ),
        (
            WKeyCode::End,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToEndOfLineContent),
        ),
    ] {
        table.insert((key, mods), KeyTableEntry { action });
    }
    table
}

impl Drop for CopyOverlay {
    fn drop(&mut self) {
        if config::configuration().copy_mode_pane_status {
            let mut r = self.render.lock();
            r.search_generation += 1;
            r.activate_search_results = false;
            SAVED_PATTERN.lock().remove(&r.tab_id);
        }
    }
}
