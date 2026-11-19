//! Shell commands on screen, found from OSC 133 input cells.
//!
//! A command is the text that the user typed at a prompt. It can cover more
//! than one input zone (a continuation prompt starts a new zone) and more
//! than one row (a long command wraps). The range of a command starts at its
//! first non-blank cell and stops at its last non-blank cell.
use crate::{Line, SemanticType, SemanticZone, StableRowIndex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandRange {
    pub start_x: usize,
    pub start_y: StableRowIndex,
    /// The last column of the command (inclusive).
    pub end_x: usize,
    pub end_y: StableRowIndex,
}

/// Which command to find, relative to a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandTarget {
    /// The nearest command that ends above the row.
    Previous,
    /// The nearest command that starts below the row.
    Next,
    /// The command on the row, or else the nearest command above it.
    Current,
}

type Rows<'a> = &'a mut dyn FnMut(StableRowIndex) -> Option<Line>;

/// The first and the last non-blank column of `line` inside `from..=to`.
fn text_span(line: &Line, from: usize, to: usize) -> Option<(usize, usize)> {
    let mut span: Option<(usize, usize)> = None;
    for cell in line.visible_cells() {
        let x = cell.cell_index();
        if x < from || x > to || cell.str().trim().is_empty() {
            continue;
        }
        let last = x + cell.width().max(1) - 1;
        span = Some(match span {
            None => (x, last),
            Some((first, _)) => (first, last),
        });
    }
    span
}

/// The text of the cells of `line` that are left of column `end`.
fn text_before(line: &Line, end: usize) -> String {
    line.visible_cells()
        .take_while(|cell| cell.cell_index() < end)
        .map(|cell| cell.str().to_owned())
        .collect()
}

/// A zone that follows a continuation prompt belongs to the command above.
fn continues(inputs: &[SemanticZone], index: usize, rows: Rows) -> bool {
    let Some(previous) = index.checked_sub(1).and_then(|i| inputs.get(i)) else {
        return false;
    };
    let zone = &inputs[index];
    if zone.start_y > previous.end_y + 1 || zone.start_x == 0 {
        return false;
    }
    let Some(line) = rows(zone.start_y) else {
        return false;
    };
    let prompt = text_before(&line, zone.start_x);
    prompt.starts_with('…') || prompt.starts_with('>')
}

/// The indexes of the first and the last zone of the command of `index`.
fn group(inputs: &[SemanticZone], index: usize, rows: Rows) -> (usize, usize) {
    let (mut first, mut last) = (index, index);
    while first > 0 && continues(inputs, first, rows) {
        first -= 1;
    }
    while last + 1 < inputs.len() && continues(inputs, last + 1, rows) {
        last += 1;
    }
    (first, last)
}

/// The command of the zones `first..=last`, without the blank cells around it.
fn trimmed(inputs: &[SemanticZone], first: usize, last: usize, rows: Rows) -> Option<CommandRange> {
    let (head, tail) = (&inputs[first], &inputs[last]);
    let mut start = None;
    for y in head.start_y..=tail.end_y {
        let from = if y == head.start_y { head.start_x } else { 0 };
        let to = if y == tail.end_y { tail.end_x } else { usize::MAX };
        if let Some((x, _)) = rows(y).and_then(|line| text_span(&line, from, to)) {
            start = Some((x, y));
            break;
        }
    }
    let (start_x, start_y) = start?;
    for y in (start_y..=tail.end_y).rev() {
        let from = if y == start_y { start_x } else { 0 };
        let to = if y == tail.end_y { tail.end_x } else { usize::MAX };
        if let Some((_, x)) = rows(y).and_then(|line| text_span(&line, from, to)) {
            return Some(CommandRange { start_x, start_y, end_x: x, end_y: y });
        }
    }
    None
}

/// Find a command relative to row `y`.
///
/// `prompt_marker` is for rows without OSC 133 marks: for
/// `CommandTarget::Current`, the text after the marker on row `y` is the
/// command.
pub fn find_command(
    zones: &[SemanticZone],
    y: StableRowIndex,
    target: CommandTarget,
    prompt_marker: Option<&str>,
    rows: Rows,
) -> Option<CommandRange> {
    let inputs: Vec<SemanticZone> = zones
        .iter()
        .filter(|zone| zone.semantic_type == SemanticType::Input)
        .copied()
        .collect();
    let command = |index: usize, rows: Rows| {
        let (first, last) = group(&inputs, index, rows);
        (first, last, trimmed(&inputs, first, last, rows))
    };
    match target {
        CommandTarget::Next => {
            let mut index = inputs.partition_point(|zone| zone.start_y <= y);
            while index < inputs.len() {
                let (_, last, range) = command(index, rows);
                if let Some(range) = range.filter(|range| range.start_y > y) {
                    return Some(range);
                }
                index = last + 1;
            }
        }
        CommandTarget::Previous => {
            let mut index = inputs.partition_point(|zone| zone.end_y < y);
            while index > 0 {
                let (first, _, range) = command(index - 1, rows);
                if let Some(range) = range.filter(|range| range.end_y < y) {
                    return Some(range);
                }
                index = first;
            }
        }
        CommandTarget::Current => {}
    }
    // The command on this row.
    if let Some(index) = inputs
        .iter()
        .position(|zone| zone.start_y <= y && zone.end_y >= y)
    {
        if let Some(range) = command(index, rows).2 {
            return Some(range);
        }
    } else if let Some(marker) = prompt_marker.filter(|marker| !marker.is_empty()) {
        if let Some(line) = rows(y) {
            let text = line.as_str();
            if let Some(found) = text.find(marker) {
                // The column of the first cell after the marker.
                let (mut bytes, mut from) = (0, usize::MAX);
                for cell in line.visible_cells() {
                    if bytes >= found + marker.len() {
                        from = cell.cell_index();
                        break;
                    }
                    bytes += cell.str().len();
                }
                if let Some((start_x, end_x)) = text_span(&line, from, usize::MAX) {
                    return Some(CommandRange { start_x, start_y: y, end_x, end_y: y });
                }
            }
        }
    }
    if target != CommandTarget::Current {
        return None;
    }
    let mut index = inputs.partition_point(|zone| zone.start_y <= y);
    while index > 0 {
        let (first, _, range) = command(index - 1, rows);
        if range.is_some() {
            return range;
        }
        index = first;
    }
    None
}

/// Every command on screen with the rows of its block: from its first row
/// to the row before the next prompt (its output and the empty rows after
/// it). The next prompt is the first row below the command that has prompt
/// text. Without one, the block ends at `last_row`.
pub fn command_blocks(
    zones: &[SemanticZone],
    last_row: StableRowIndex,
    rows: Rows,
) -> Vec<(CommandRange, StableRowIndex)> {
    let inputs: Vec<SemanticZone> = zones
        .iter()
        .filter(|zone| zone.semantic_type == SemanticType::Input)
        .copied()
        .collect();
    let prompt_rows: Vec<StableRowIndex> = {
        let mut found = vec![];
        for zone in zones.iter().filter(|zone| zone.semantic_type == SemanticType::Prompt) {
            for y in zone.start_y..=zone.end_y {
                let has_text = rows(y).is_some_and(|line| {
                    line.visible_cells().any(|cell| {
                        cell.attrs().semantic_type() == SemanticType::Prompt && !cell.str().trim().is_empty()
                    })
                });
                if has_text && found.last() != Some(&y) {
                    found.push(y);
                }
            }
        }
        found
    };
    let mut blocks = vec![];
    let mut index = 0;
    while index < inputs.len() {
        let (first, last) = group(&inputs, index, rows);
        if let Some(range) = trimmed(&inputs, first, last, rows) {
            let next_prompt = prompt_rows.iter().copied().find(|y| *y > range.end_y);
            let end = next_prompt.map(|y| y - 1).unwrap_or(last_row).max(range.end_y);
            blocks.push((range, end));
        }
        index = last + 1;
    }
    blocks
}
