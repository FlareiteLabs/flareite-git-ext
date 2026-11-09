//! Three-way merges, as the merge editor shows them: the lines each side
//! changed from their common base, which of those changes conflict, and
//! what the result made of each. (Moved from the editor's `flareite-diff`
//! when the merge editor became this extension's layout.)
//!
//! Each side is compared with the base. Changes from either side that
//! overlap or touch in the base form one range; a range that both sides
//! changed, differently, is a conflict (as Git counts them). Where each
//! range is in the result is found by comparing the result with the base,
//! so the result can be edited freely: a range's lines in it are compared
//! with what accepting either side, both, or neither would give.

use std::ops::Range;

use flareite_diff::{DiffError, DiffOptions, LineChange, diff};

/// A text's lines, without their line breaks.
fn lines(text: &str) -> Vec<&str> {
  text
    .split('\n')
    .map(|line| line.strip_suffix('\r').unwrap_or(line))
    .collect()
}

/// Lines changed from the base by one side or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRange {
  pub base: Range<usize>,
  pub incoming: Range<usize>,
  pub current: Range<usize>,
  /// Both sides changed these lines, to different text.
  pub conflicting: bool,
}

impl MergeRange {
  /// Whether `side` changed these lines.
  pub fn changes(&self, side: Side, text: &MergeTexts) -> bool {
    let lines = self.side(side);
    text.side(side)[lines.clone()] != text.base[self.base.clone()]
  }

  pub fn side(&self, side: Side) -> &Range<usize> {
    match side {
      Side::Incoming => &self.incoming,
      Side::Current => &self.current,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
  Incoming,
  Current,
}

/// What a range's lines in the result are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeState {
  /// The base's: neither side's change is in.
  Base,
  Incoming,
  Current,
  /// The current side's lines, then the incoming side's.
  CurrentIncoming,
  IncomingCurrent,
  /// Something else: edited by hand.
  Manual,
}

impl RangeState {
  /// Whether `side`'s change is in.
  pub fn has(self, side: Side) -> bool {
    matches!(
      (self, side),
      (
        Self::Incoming | Self::CurrentIncoming | Self::IncomingCurrent,
        Side::Incoming
      ) | (
        Self::Current | Self::CurrentIncoming | Self::IncomingCurrent,
        Side::Current
      )
    )
  }
}

/// The lines of the three texts a merge compares.
pub struct MergeTexts<'a> {
  pub base: Vec<&'a str>,
  pub incoming: Vec<&'a str>,
  pub current: Vec<&'a str>,
}

impl<'a> MergeTexts<'a> {
  pub fn new(base: &'a str, incoming: &'a str, current: &'a str) -> Self {
    Self {
      base: lines(base),
      incoming: lines(incoming),
      current: lines(current),
    }
  }

  fn side(&self, side: Side) -> &[&'a str] {
    match side {
      Side::Incoming => &self.incoming,
      Side::Current => &self.current,
    }
  }

  /// The lines a range's result has in `state` (`Manual` gives the base's).
  pub fn lines_for(&self, range: &MergeRange, state: RangeState) -> Vec<&'a str> {
    let incoming = &self.incoming[range.incoming.clone()];
    let current = &self.current[range.current.clone()];
    match state {
      RangeState::Base | RangeState::Manual => self.base[range.base.clone()].to_vec(),
      RangeState::Incoming => incoming.to_vec(),
      RangeState::Current => current.to_vec(),
      RangeState::CurrentIncoming => [current, incoming].concat(),
      RangeState::IncomingCurrent => [incoming, current].concat(),
    }
  }
}

const OPTIONS: DiffOptions = DiffOptions {
  ignore_trim_whitespace: false,
};

/// The lines a change covers in `side` of the base, and in the side.
struct Hunk {
  base: Range<usize>,
  side: Side,
  lines: Range<usize>,
}

/// The ranges the sides changed, in order.
pub fn ranges(base: &str, incoming: &str, current: &str) -> Result<Vec<MergeRange>, DiffError> {
  let texts = MergeTexts::new(base, incoming, current);
  let mut hunks: Vec<Hunk> = Vec::new();
  for (side, text) in [(Side::Incoming, incoming), (Side::Current, current)] {
    hunks.extend(diff(base, text, OPTIONS)?.into_iter().map(|change| Hunk {
      base: change.original,
      side,
      lines: change.modified,
    }));
  }
  hunks.sort_by_key(|hunk| (hunk.base.start, hunk.base.end));

  // Each side's lines move by what its changes before added or removed.
  let mut delta = [0isize; 2];
  let index = |side: Side| match side {
    Side::Incoming => 0,
    Side::Current => 1,
  };
  let mut ranges = Vec::new();
  let mut hunks = hunks.into_iter().peekable();
  while let Some(first) = hunks.next() {
    let mut group = vec![first];
    let mut end = group[0].base.end;
    while let Some(next) = hunks.next_if(|hunk| hunk.base.start <= end) {
      end = end.max(next.base.end);
      group.push(next);
    }
    let start = group[0].base.start;
    let mut sides = [start..end, start..end];
    let mut changed = [false; 2];
    for side in [Side::Incoming, Side::Current] {
      let i = index(side);
      let before = delta[i];
      let inside: isize = group
        .iter()
        .filter(|hunk| hunk.side == side)
        .map(|hunk| {
          changed[i] = true;
          hunk.lines.len() as isize - hunk.base.len() as isize
        })
        .sum();
      let at = |line: usize, by: isize| (line as isize + by).max(0) as usize;
      sides[i] = at(start, before)..at(end, before + inside);
      delta[i] += inside;
    }
    let [incoming_lines, current_lines] = sides;
    let conflicting = changed == [true, true]
      && texts.incoming[incoming_lines.clone()] != texts.current[current_lines.clone()];
    ranges.push(MergeRange {
      base: start..end,
      incoming: incoming_lines,
      current: current_lines,
      conflicting,
    });
  }
  Ok(ranges)
}

/// Where a range is in the result, and what it is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRange {
  pub lines: Range<usize>,
  pub state: RangeState,
}

/// Where the result was changed from the base away from any range: its
/// lines in the base and in the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultEdit {
  pub base: Range<usize>,
  pub result: Range<usize>,
}

/// Where each range is in `result`, and what it is there; and the result's
/// other changes from the base.
pub fn in_result(
  ranges: &[MergeRange],
  texts: &MergeTexts,
  base: &str,
  result: &str,
) -> Result<(Vec<ResultRange>, Vec<ResultEdit>), DiffError> {
  let changes = diff(base, result, OPTIONS)?;
  let result_lines = lines(result);
  let mut found: Vec<ResultRange> = Vec::with_capacity(ranges.len());
  let mut previous_end = 0;
  for range in ranges {
    let start = map_start(&changes, range.base.start).max(previous_end);
    let end = map_end(&changes, range.base.end).max(start);
    previous_end = end;
    let shown = &result_lines[start.min(result_lines.len())..end.min(result_lines.len())];
    let state = [
      RangeState::Base,
      RangeState::Incoming,
      RangeState::Current,
      RangeState::CurrentIncoming,
      RangeState::IncomingCurrent,
    ]
    .into_iter()
    .find(|state| texts.lines_for(range, *state) == shown)
    .unwrap_or(RangeState::Manual);
    found.push(ResultRange {
      lines: start..end,
      state,
    });
  }
  let edits = changes
    .iter()
    .filter(|change| {
      !ranges.iter().any(|range| {
        change.original.start <= range.base.end && change.original.end >= range.base.start
      })
    })
    .map(|change| ResultEdit {
      base: change.original.clone(),
      result: change.modified.clone(),
    })
    .collect();
  Ok((found, edits))
}

/// Where the base's line `start` begins in the result: a change around it
/// counts as the range's, as does one inserting lines there.
fn map_start(changes: &[LineChange], start: usize) -> usize {
  let mut delta = 0isize;
  for change in changes {
    let original = &change.original;
    if original.end < start || (original.end == start && original.start < start) {
      delta += change.modified.len() as isize - original.len() as isize;
    } else if original.start < start {
      return change.modified.start;
    } else {
      break;
    }
  }
  (start as isize + delta).max(0) as usize
}

/// Where the base's lines up to `end` end in the result, with a change
/// across it, or inserting lines there, counted in.
fn map_end(changes: &[LineChange], end: usize) -> usize {
  let mut delta = 0isize;
  for change in changes {
    let original = &change.original;
    if original.end <= end {
      delta += change.modified.len() as isize - original.len() as isize;
    } else if original.start < end {
      return change.modified.end;
    } else {
      break;
    }
  }
  (end as isize + delta).max(0) as usize
}

/// The result a merge starts from: the base with every change that doesn't
/// conflict made, and each conflict's lines as the base has them. Line
/// breaks are the base's (`\r\n` if it has any).
pub fn initial(base: &str, ranges: &[MergeRange], texts: &MergeTexts) -> String {
  let mut out: Vec<&str> = Vec::with_capacity(texts.base.len());
  let mut at = 0;
  for range in ranges {
    out.extend_from_slice(&texts.base[at..range.base.start]);
    let state = if range.conflicting {
      RangeState::Base
    } else if range.changes(Side::Incoming, texts) {
      RangeState::Incoming
    } else {
      RangeState::Current
    };
    out.extend(texts.lines_for(range, state));
    at = range.base.end;
  }
  out.extend_from_slice(&texts.base[at..]);
  out.join(if base.contains("\r\n") { "\r\n" } else { "\n" })
}

#[cfg(test)]
mod tests {
  use super::*;

  const BASE: &str = "a\nb\nc\nd\ne\nf\ng\n";

  #[test]
  fn changes_far_apart_do_not_conflict() {
    let incoming = "a\nB\nc\nd\ne\nf\ng\n";
    let current = "a\nb\nc\nd\ne\nF\ng\n";
    let found = ranges(BASE, incoming, current).expect("small");
    assert_eq!(
      found,
      vec![
        MergeRange {
          base: 1..2,
          incoming: 1..2,
          current: 1..2,
          conflicting: false
        },
        MergeRange {
          base: 5..6,
          incoming: 5..6,
          current: 5..6,
          conflicting: false
        },
      ]
    );
    let texts = MergeTexts::new(BASE, incoming, current);
    assert_eq!(initial(BASE, &found, &texts), "a\nB\nc\nd\ne\nF\ng\n");
  }

  #[test]
  fn overlapping_and_touching_changes_conflict() {
    // Incoming changes b and adds a line; current changes c, touching it.
    let incoming = "a\nB\nnew\nc\nd\ne\nf\ng\n";
    let current = "a\nb\nC\nd\ne\nf\ng\n";
    let found = ranges(BASE, incoming, current).expect("small");
    assert_eq!(found.len(), 1);
    let range = &found[0];
    assert!(range.conflicting);
    assert_eq!(range.base, 1..3);
    assert_eq!(range.incoming, 1..4);
    assert_eq!(range.current, 1..3);
    let texts = MergeTexts::new(BASE, incoming, current);
    // The conflict keeps the base's lines to begin with.
    assert_eq!(initial(BASE, &found, &texts), BASE);
  }

  #[test]
  fn the_same_change_on_both_sides_does_not_conflict() {
    let both = "a\nb\nX\nd\ne\nf\ng\n";
    let found = ranges(BASE, both, both).expect("small");
    assert_eq!(found.len(), 1);
    assert!(!found[0].conflicting);
  }

  #[test]
  fn later_ranges_account_for_lines_added_before() {
    let incoming = "a\nnew 1\nnew 2\nb\nc\nd\ne\nf\nG\n";
    let current = "a\nb\nc\nd\ne\nf\nH\n";
    let found = ranges(BASE, incoming, current).expect("small");
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].base, 1..1);
    assert_eq!(found[0].incoming, 1..3);
    assert_eq!(found[0].current, 1..1);
    assert_eq!(found[1].base, 6..7);
    assert_eq!(found[1].incoming, 8..9);
    assert_eq!(found[1].current, 6..7);
    assert!(found[1].conflicting);
  }

  #[test]
  fn the_result_tells_what_each_range_became() {
    let incoming = "a\nB\nc\nd\ne\nF1\ng\n";
    let current = "a\nb\nc\nd\ne\nF2\ng\n";
    let found = ranges(BASE, incoming, current).expect("small");
    let texts = MergeTexts::new(BASE, incoming, current);
    let start = initial(BASE, &found, &texts);
    let (shown, edits) = in_result(&found, &texts, BASE, &start).expect("small");
    assert_eq!(shown[0].state, RangeState::Incoming);
    assert_eq!(shown[1].state, RangeState::Base);
    assert!(edits.is_empty());

    // Both sides of the conflict, current first, and an edit elsewhere.
    let result = "a\nB\nc\nD\ne\nF2\nF1\ng\n";
    let (shown, edits) = in_result(&found, &texts, BASE, result).expect("small");
    assert_eq!(shown[1].lines, 5..7);
    assert_eq!(shown[1].state, RangeState::CurrentIncoming);
    assert_eq!(
      edits,
      vec![ResultEdit {
        base: 3..4,
        result: 3..4
      }]
    );

    // Edited by hand, and removed altogether.
    let (shown, _) = in_result(&found, &texts, BASE, "a\nB\nc\nd\ne\nmine\ng\n").expect("small");
    assert_eq!(shown[1].state, RangeState::Manual);
    let (shown, _) = in_result(&found, &texts, BASE, "a\nB\nc\nd\ne\ng\n").expect("small");
    assert_eq!(shown[1].lines, 5..5);
    assert_eq!(shown[1].state, RangeState::Manual);
  }

  #[test]
  fn insertions_at_the_same_place_are_found_in_the_result() {
    let incoming = "a\nb\nc\nd\ne\nf\ng\nin\n";
    let current = "a\nb\nc\nd\ne\nf\ng\ncur\n";
    let found = ranges(BASE, incoming, current).expect("small");
    assert_eq!(found.len(), 1);
    assert!(found[0].conflicting);
    let texts = MergeTexts::new(BASE, incoming, current);
    let (shown, _) = in_result(&found, &texts, BASE, incoming).expect("small");
    assert_eq!(shown[0].state, RangeState::Incoming);
    let (shown, _) = in_result(&found, &texts, BASE, BASE).expect("small");
    assert_eq!(shown[0].state, RangeState::Base);
    assert!(shown[0].lines.is_empty());
  }

  #[test]
  fn crlf_bases_keep_their_line_breaks() {
    let base = "a\r\nb\r\n";
    let incoming = "a\r\nB\r\n";
    let found = ranges(base, incoming, base).expect("small");
    let texts = MergeTexts::new(base, incoming, base);
    assert_eq!(initial(base, &found, &texts), "a\r\nB\r\n");
  }
}
