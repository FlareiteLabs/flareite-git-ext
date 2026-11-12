//! The merge editor: a conflicted file's incoming and current versions
//! above the result (the file), as VS Code's, as the editor's `merge`
//! layout of this extension.
//!
//! A tab's key is the result's absolute path. The versions are the index
//! stages (`git:~3:` incoming, `git:~2:` current, `git:~1:` their base,
//! which stays open unseen). Each version is compared with the base
//! (`merge_ranges`); changes that overlap or touch make one range, a
//! conflict if both sides changed it differently. Where each range is in
//! the result comes from comparing the result with the base, so the result
//! can be edited freely. Above each range, every editor has a row of links
//! (accept a side, both, or neither); every range, and every change made to
//! the result elsewhere, takes the height of its tallest side in all three
//! editors, which scroll together. Complete Merge saves the result, stages
//! it, and closes the tab.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use flareite_extension_api::layouts::{
  self, Button, Decorated, Direction, EditorPane, FillReason, FillRequest, Layout, LayoutNode,
  LayoutTab, NodeKind, Pane, PaneContent, PaneDecorations, PaneHeader, Zone, ZoneContent, ZoneLink,
  ZoneLinks,
};
use flareite_extension_api::types::{CommandContext, LogLevel, Position, Range as ApiRange};
use flareite_extension_api::{documents, host};

use crate::merge_ranges::{
  self, MergeRange, MergeTexts, RangeState, ResultEdit, ResultRange, Side,
};
use crate::text::{self, whole_lines};

pub const LAYOUT: &str = "merge";
const INCOMING: &str = "incoming";
const CURRENT: &str = "current";
const RESULT: &str = "result";
const PANES: [&str; 3] = [INCOMING, CURRENT, RESULT];

/// What a merge found, as of the result's `version`.
#[derive(Clone)]
struct Found {
  ranges: Vec<MergeRange>,
  shown: Vec<ResultRange>,
  edits: Vec<ResultEdit>,
  /// Per range: whether each side changed it.
  changed: Vec<(bool, bool)>,
  version: u64,
}

/// An open merge tab.
struct Tab {
  /// The result's path relative to its repository's top folder.
  relative: String,
  root: String,
  incoming: Option<String>,
  current: Option<String>,
  /// The result starts again from the base once its documents are open:
  /// the file had conflict markers when the tab opened.
  reset: bool,
  /// The documents: base, incoming, current, result.
  documents: Option<[u64; 4]>,
  found: Option<Found>,
  /// Why the versions couldn't be compared.
  problem: Option<String>,
  /// The unresolved and conflicting counts the result's header shows.
  counts: (usize, usize),
}

thread_local! {
  static TABS: RefCell<HashMap<String, Tab>> = RefCell::new(HashMap::new());
}

fn tab(key: &str) -> LayoutTab {
  LayoutTab {
    layout: LAYOUT.into(),
    key: key.into(),
  }
}

fn u32_of(value: usize) -> u32 {
  u32::try_from(value).unwrap_or(u32::MAX)
}

/// The versions' addresses, by the stage each is.
fn version(tab: &Tab, stage: &str) -> String {
  crate::repos::address_in(&tab.root, stage, &tab.relative)
}

/// Opens a conflicted file in the merge editor (or shows its tab): theirs
/// and ours above the result, which starts again from their base if the
/// file has conflict markers (`marked`).
pub fn open(
  root: &str,
  path: &str,
  relative: &str,
  marked: bool,
  incoming: Option<String>,
  current: Option<String>,
) {
  TABS.with_borrow_mut(|tabs| {
    let tab = tabs.entry(path.to_owned()).or_insert_with(|| Tab {
      relative: relative.to_owned(),
      root: root.to_owned(),
      incoming: None,
      current: None,
      reset: false,
      documents: None,
      found: None,
      problem: None,
      counts: (0, 0),
    });
    tab.reset = marked;
    tab.incoming = incoming;
    tab.current = current;
  });
  if let Err(problem) = layouts::open(&tab(path), false) {
    crate::error(&format!("Opening the merge editor failed: {problem:?}"));
  }
}

/// The layout's command: fills a tab in, or hears that it changed.
pub fn fill(request: FillRequest) {
  let key = request.tab.key;
  match request.reason {
    FillReason::Opened | FillReason::Restored => {
      let known = TABS.with_borrow(|tabs| tabs.contains_key(&key));
      if !known {
        // Restored with the window: the file is merged again as it is.
        let Some(repo) = crate::repos::repo_of(&key) else {
          return crate::info(&format!("{key} isn't in a repository any more."));
        };
        let Some(relative) = crate::relative_path(&repo.root, &key) else {
          return;
        };
        let marked = crate::conflicts::has_conflicts(&crate::current_text(&key));
        TABS.with_borrow_mut(|tabs| {
          tabs.insert(
            key.clone(),
            Tab {
              relative,
              root: repo.root,
              incoming: None,
              current: None,
              reset: marked,
              documents: None,
              found: None,
              problem: None,
              counts: (0, 0),
            },
          );
        });
      }
      lay_out(&key);
    }
    FillReason::Resized => {}
    FillReason::Ready => {
      let documents = layouts::documents(&tab(&key)).unwrap_or_default();
      let reset = TABS.with_borrow_mut(|tabs| {
        let tab = tabs.get_mut(&key)?;
        let find = |uri: &str| {
          documents
            .iter()
            .find(|document| document.uri == uri)
            .map(|document| document.document)
        };
        tab.documents = Some([
          find(&version(tab, "~1"))?,
          find(&version(tab, "~3"))?,
          find(&version(tab, "~2"))?,
          find(&key)?,
        ]);
        Some(std::mem::take(&mut tab.reset))
      });
      match reset {
        Some(true) => reset_result(&key),
        Some(false) => compare(&key),
        None => {}
      }
    }
    FillReason::Closed => {
      TABS.with_borrow_mut(|tabs| tabs.remove(&key));
    }
  }
}

fn icon_button(id: &str, title: &str, icon: &str, command: &str, disabled: bool) -> Button {
  Button {
    id: id.into(),
    title: title.into(),
    icon: Some(icon.into()),
    command: format!("flareite.git.{command}"),
    argument: None,
    primary: false,
    pressed: None,
    disabled,
  }
}

/// Sets a tab's layout: the versions side by side above the result.
fn lay_out(key: &str) {
  let Some((uris, incoming, current, relative, problem, (remaining, conflicts), found)) = TABS
    .with_borrow(|tabs| {
      let tab = tabs.get(key)?;
      Some((
        [version(tab, "~1"), version(tab, "~3"), version(tab, "~2")],
        tab.incoming.clone(),
        tab.current.clone(),
        tab.relative.clone(),
        tab.problem.clone(),
        tab.counts,
        tab.found.is_some(),
      ))
    })
  else {
    return;
  };
  let [base, incoming_uri, current_uri] = uris;
  let editor = |uri: &str| {
    PaneContent::Editor(EditorPane {
      uri: uri.to_owned(),
      minimap: false,
      folding: false,
      quick_diff: false,
    })
  };
  let pane = |id: &str, parent: &str, content: PaneContent, header: PaneHeader| LayoutNode {
    id: id.into(),
    parent: Some(parent.into()),
    size: 1.0,
    kind: NodeKind::Pane(Pane {
      content,
      header: Some(header),
      scroll_group: Some("merge".into()),
    }),
  };
  let status = if let Some(problem) = problem {
    problem
  } else if !found {
    String::new()
  } else if conflicts == 0 {
    "No conflicts".into()
  } else if remaining == 0 {
    "All conflicts resolved".into()
  } else {
    format!(
      "{remaining} of {conflicts} {} remaining",
      if conflicts == 1 {
        "conflict"
      } else {
        "conflicts"
      }
    )
  };
  let name = relative.rsplit('/').next().unwrap_or(&relative).to_owned();
  let layout = Layout {
    title: format!("Merging: {name}"),
    nodes: vec![
      LayoutNode {
        id: "all".into(),
        parent: None,
        size: 1.0,
        kind: NodeKind::Split(Direction::Column),
      },
      LayoutNode {
        id: "versions".into(),
        parent: Some("all".into()),
        size: 1.0,
        kind: NodeKind::Split(Direction::Row),
      },
      pane(
        INCOMING,
        "versions",
        editor(&incoming_uri),
        PaneHeader {
          title: "Incoming".into(),
          description: incoming,
          buttons: Vec::new(),
        },
      ),
      pane(
        CURRENT,
        "versions",
        editor(&current_uri),
        PaneHeader {
          title: "Current".into(),
          description: current,
          buttons: Vec::new(),
        },
      ),
      pane(
        RESULT,
        "all",
        editor(key),
        PaneHeader {
          title: "Result".into(),
          description: Some(if status.is_empty() {
            relative.clone()
          } else {
            format!("{relative} \u{2014} {status}")
          }),
          buttons: vec![
            icon_button(
              "previous",
              "Previous Conflict (Shift+F7)",
              "arrow-up",
              "previousChange",
              conflicts == 0,
            ),
            icon_button(
              "next",
              "Next Conflict (F7)",
              "arrow-down",
              "nextChange",
              conflicts == 0,
            ),
            Button {
              id: "complete".into(),
              title: "Complete Merge".into(),
              icon: None,
              command: "flareite.git.mergeComplete".into(),
              argument: None,
              primary: true,
              pressed: None,
              disabled: !found,
            },
          ],
        },
      ),
    ],
    // The versions' base, to compare them with.
    documents: vec![base],
    buttons: vec![icon_button(
      "open",
      "Open File",
      "file-text",
      "mergeOpenFile",
      false,
    )],
    breakpoints: Vec::new(),
    main: Some(RESULT.into()),
  };
  if let Err(problem) = layouts::set_layout(&tab(key), &layout) {
    crate::error(&format!("Showing the merge failed: {problem:?}"));
  }
}

/// The four texts of a tab, and the result's version.
fn texts(documents: [u64; 4]) -> Option<([String; 4], u64)> {
  let [base, incoming, current, result] = documents.map(text::document_text);
  let (result, version) = result?;
  Some(([base?.0, incoming?.0, current?.0, result], version))
}

/// Compares a tab's versions again, as they are now, and decorates it.
fn compare(key: &str) {
  let Some(documents) = TABS.with_borrow(|tabs| tabs.get(key)?.documents) else {
    return;
  };
  let Some(([base, incoming, current, result], version)) = texts(documents) else {
    return;
  };
  let found = find(&base, &incoming, &current, &result, version);
  let counts = found
    .as_ref()
    .map(|found| {
      (
        found
          .ranges
          .iter()
          .zip(&found.shown)
          .filter(|(range, shown)| unresolved(range, shown))
          .count(),
        found
          .ranges
          .iter()
          .filter(|range| range.conflicting)
          .count(),
      )
    })
    .unwrap_or_default();
  let relayout = TABS.with_borrow_mut(|tabs| {
    let tab = tabs.get_mut(key)?;
    let before = (tab.counts, tab.found.is_some(), tab.problem.clone());
    match found {
      Ok(found) => {
        tab.found = Some(found);
        tab.problem = None;
      }
      Err(problem) => {
        tab.found = None;
        tab.problem = Some(problem);
      }
    }
    tab.counts = counts;
    Some(before != (tab.counts, tab.found.is_some(), tab.problem.clone()))
  });
  if relayout == Some(true) {
    lay_out(key);
  }
  decorate(key);
}

fn find(
  base: &str,
  incoming: &str,
  current: &str,
  result: &str,
  version: u64,
) -> Result<Found, String> {
  let too_large = |_| {
    format!(
      "Files with more than {} lines aren't merged.",
      flareite_diff::MAX_LINES
    )
  };
  let lines = MergeTexts::new(base, incoming, current);
  let ranges = merge_ranges::ranges(base, incoming, current).map_err(too_large)?;
  let (shown, edits) = merge_ranges::in_result(&ranges, &lines, base, result).map_err(too_large)?;
  let changed = ranges
    .iter()
    .map(|range| {
      (
        range.changes(Side::Incoming, &lines),
        range.changes(Side::Current, &lines),
      )
    })
    .collect();
  Ok(Found {
    ranges,
    shown,
    edits,
    changed,
    version,
  })
}

/// A conflict neither side's change was accepted into.
fn unresolved(range: &MergeRange, shown: &ResultRange) -> bool {
  range.conflicting && shown.state == RangeState::Base
}

/// One range (`range`), or a change made to the result elsewhere (`None`):
/// its lines in each editor.
struct Block {
  range: Option<usize>,
  lines: [Range<usize>; 3],
}

/// The blocks, in order.
fn blocks(found: &Found) -> Vec<Block> {
  let mut blocks: Vec<(usize, Block)> = found
    .ranges
    .iter()
    .zip(&found.shown)
    .enumerate()
    .map(|(index, (range, shown))| {
      (
        range.base.start,
        Block {
          range: Some(index),
          lines: [
            range.incoming.clone(),
            range.current.clone(),
            shown.lines.clone(),
          ],
        },
      )
    })
    .collect();
  // A change elsewhere is at its base lines in the versions, which moved by
  // what the ranges before it added or removed.
  for edit in &found.edits {
    let shift = |side: Side| -> isize {
      found
        .ranges
        .iter()
        .filter(|range| range.base.end <= edit.base.start)
        .map(|range| range.side(side).len() as isize - range.base.len() as isize)
        .sum()
    };
    let at = |side: Side| {
      let by = shift(side);
      let move_by = |line: usize| line.saturating_add_signed(by);
      move_by(edit.base.start)..move_by(edit.base.end)
    };
    blocks.push((
      edit.base.start,
      Block {
        range: None,
        lines: [at(Side::Incoming), at(Side::Current), edit.result.clone()],
      },
    ));
  }
  blocks.sort_by_key(|(base, block)| (*base, block.range.is_none()));
  blocks.into_iter().map(|(_, block)| block).collect()
}

/// What accepting `side` does from the state a range is in: toggling that
/// side's change.
fn toggled(range: &MergeRange, state: RangeState, side: Side) -> RangeState {
  let other = match side {
    Side::Incoming => Side::Current,
    Side::Current => Side::Incoming,
  };
  let has = state.has(side);
  let has_other = state != RangeState::Manual && state.has(other);
  let alone = |side: Side| match side {
    Side::Incoming => RangeState::Incoming,
    Side::Current => RangeState::Current,
  };
  if has {
    return if has_other {
      alone(other)
    } else {
      RangeState::Base
    };
  }
  if !range.conflicting || !has_other {
    return alone(side);
  }
  // Accepting the second side keeps both: the one accepted first, first.
  match other {
    Side::Current => RangeState::CurrentIncoming,
    Side::Incoming => RangeState::IncomingCurrent,
  }
}

fn state_name(state: RangeState) -> &'static str {
  match state {
    RangeState::Base => "base",
    RangeState::Incoming => "incoming",
    RangeState::Current => "current",
    RangeState::CurrentIncoming => "currentIncoming",
    RangeState::IncomingCurrent => "incomingCurrent",
    RangeState::Manual => "manual",
  }
}

fn state_of(name: &str) -> Option<RangeState> {
  Some(match name {
    "base" => RangeState::Base,
    "incoming" => RangeState::Incoming,
    "current" => RangeState::Current,
    "currentIncoming" => RangeState::CurrentIncoming,
    "incomingCurrent" => RangeState::IncomingCurrent,
    _ => return None,
  })
}

/// A range's result, in words.
fn state_label(range: &MergeRange, state: RangeState) -> &'static str {
  match state {
    RangeState::Base if range.conflicting => "Unresolved conflict",
    RangeState::Base => "Change not accepted",
    RangeState::Incoming => "Incoming accepted",
    RangeState::Current => "Current accepted",
    RangeState::CurrentIncoming => "Both accepted (current first)",
    RangeState::IncomingCurrent => "Both accepted (incoming first)",
    RangeState::Manual => "Edited",
  }
}

/// The links above a range in one of the editors.
fn links(found: &Found, index: usize, pane: &str) -> ZoneLinks {
  let range = &found.ranges[index];
  let state = found.shown[index].state;
  let (incoming_changed, current_changed) = found.changed[index];
  let link = |title: &str, to: RangeState| ZoneLink {
    title: title.into(),
    tooltip: None,
    command: "flareite.git.mergeAccept".into(),
    argument: Some(format!("{index}:{}:{}", state_name(to), found.version)),
  };
  let mut label = None;
  let mut links = Vec::new();
  if pane == RESULT {
    label = Some(state_label(range, state).to_owned());
    if range.conflicting {
      links.extend([
        link("Accept Incoming", RangeState::Incoming),
        link("Accept Current", RangeState::Current),
        link("Accept Both", RangeState::CurrentIncoming),
      ]);
    }
    if state != RangeState::Base {
      links.push(link("Reset", RangeState::Base));
    }
  } else {
    let (side, changed, name) = if pane == INCOMING {
      (Side::Incoming, incoming_changed, "Incoming")
    } else {
      (Side::Current, current_changed, "Current")
    };
    if changed {
      let to = toggled(range, state, side);
      if state.has(side) {
        label = Some("\u{2713} Accepted".to_owned());
        links.push(link("Undo", to));
      } else {
        let alone = if side == Side::Incoming {
          RangeState::Incoming
        } else {
          RangeState::Current
        };
        links.push(link(
          &format!("Accept {name}"),
          if range.conflicting { alone } else { to },
        ));
        if range.conflicting {
          links.push(link(
            "Accept Combination",
            if side == Side::Incoming {
              RangeState::IncomingCurrent
            } else {
              RangeState::CurrentIncoming
            },
          ));
        }
      }
    }
  }
  ZoneLinks { label, links }
}

/// The decoration of a range's lines in one of the editors, if any.
fn line_decoration(found: &Found, index: usize, pane: &str) -> Option<&'static str> {
  let range = &found.ranges[index];
  let state = found.shown[index].state;
  if pane == RESULT {
    if unresolved(range, &found.shown[index]) {
      return Some("merge-unresolved");
    }
    return match state {
      RangeState::Base => None,
      RangeState::Manual => Some("merge-manual"),
      RangeState::Incoming => Some("merge-incoming"),
      RangeState::Current => Some("merge-current"),
      _ => Some("merge-both"),
    };
  }
  let (incoming_changed, current_changed) = found.changed[index];
  let side = if pane == INCOMING {
    Side::Incoming
  } else {
    Side::Current
  };
  if !(if side == Side::Incoming {
    incoming_changed
  } else {
    current_changed
  }) {
    return None;
  }
  Some(match (side, state.has(side)) {
    (Side::Incoming, true) => "merge-incoming-accepted",
    (Side::Incoming, false) => "merge-incoming",
    (Side::Current, true) => "merge-current-accepted",
    (Side::Current, false) => "merge-current",
  })
}

/// What each editor shows besides its text.
fn decorate(key: &str) {
  let Some(found) = TABS.with_borrow(|tabs| tabs.get(key)?.found.clone()) else {
    return;
  };
  let mut panes: [PaneDecorations; 3] = std::array::from_fn(|_| PaneDecorations {
    decorations: Vec::new(),
    zones: Vec::new(),
    folds: Vec::new(),
    ruler: Vec::new(),
  });
  let mut lines: [HashMap<&'static str, Vec<ApiRange>>; 3] = Default::default();
  for block in blocks(&found) {
    let tallest = block.lines.iter().map(Range::len).max().unwrap_or(0);
    for (which, pane) in PANES.iter().enumerate() {
      let range = &block.lines[which];
      if let Some(index) = block.range {
        panes[which].zones.push(Zone {
          line: u32_of(range.start),
          content: ZoneContent::Links(links(&found, index, pane)),
          decoration: None,
        });
        if let Some(decoration) = line_decoration(&found, index, pane)
          && !range.is_empty()
        {
          lines[which]
            .entry(decoration)
            .or_default()
            .push(whole_lines(range));
        }
      }
      let missing = tallest - range.len();
      if missing > 0 {
        panes[which].zones.push(Zone {
          line: u32_of(range.end),
          content: ZoneContent::Filler(u32_of(missing)),
          decoration: None,
        });
      }
    }
  }
  for ((pane, mut decorations), lines) in PANES.iter().zip(panes).zip(lines) {
    decorations.decorations = lines
      .into_iter()
      .map(|(decoration, ranges)| Decorated {
        decoration: decoration.into(),
        ranges,
      })
      .collect();
    if let Err(problem) = layouts::set_pane_decorations(&tab(key), pane, &decorations) {
      crate::log(
        LogLevel::Warn,
        &format!("can't show the merge of {key}: {problem:?}"),
      );
    }
  }
}

/// Starts a tab's result again: the base with the changes that don't
/// conflict made, and the conflicts' lines as the base has them.
fn reset_result(key: &str) {
  let Some(documents) = TABS.with_borrow(|tabs| tabs.get(key)?.documents) else {
    return;
  };
  let Some(([base, incoming, current, result], version)) = texts(documents) else {
    return;
  };
  let lines = MergeTexts::new(&base, &incoming, &current);
  let Ok(ranges) = merge_ranges::ranges(&base, &incoming, &current) else {
    return compare(key);
  };
  let initial = merge_ranges::initial(&base, &ranges, &lines);
  if initial != result {
    let end = text::offset_position(&result, result.len());
    let edit = flareite_extension_api::types::TextEdit {
      range: ApiRange {
        start: Position { line: 0, column: 0 },
        end,
      },
      text: initial,
    };
    if let Err(problem) = host::apply_edits(documents[3], version, &[edit]) {
      crate::error(&format!("Starting the merge failed: {problem:?}"));
    }
  }
  // The edit's notification compares it; compare now too, in case nothing
  // changed.
  compare(key);
}

/// The merge tab a command is for: the clicked one's, else the one in use;
/// and the result's cursor line, if the result is the pane in use.
fn tab_in_use(context: &CommandContext) -> Option<(String, Option<usize>)> {
  let active = layouts::active_pane().filter(|active| active.tab.layout == LAYOUT);
  let line = |key: &str| {
    active
      .as_ref()
      .filter(|active| active.tab.key == key && active.pane == RESULT)
      .and_then(|active| active.selections.first())
      .map(|range| range.start.line as usize)
  };
  if let Some(from) = context
    .invoked_from
    .clone()
    .filter(|from| from.tab.layout == LAYOUT)
  {
    let at = line(&from.tab.key);
    return Some((from.tab.key, at));
  }
  let key = active.as_ref()?.tab.key.clone();
  let at = line(&key);
  Some((key, at))
}

/// Moves to the next unresolved conflict after the result's cursor (or the
/// one before it), wrapping around; when all are resolved, between them all.
pub fn go_to_conflict(context: &CommandContext, backward: bool) {
  let Some((key, line)) = tab_in_use(context) else {
    return;
  };
  let line = line.unwrap_or(0);
  let target = TABS.with_borrow(|tabs| {
    let found = tabs.get(&key)?.found.as_ref()?;
    let pairs: Vec<(&MergeRange, &ResultRange)> = found.ranges.iter().zip(&found.shown).collect();
    let open: Vec<usize> = pairs
      .iter()
      .filter(|(range, shown)| unresolved(range, shown))
      .map(|(_, shown)| shown.lines.start)
      .collect();
    let conflicts = if open.is_empty() {
      pairs
        .iter()
        .filter(|(range, _)| range.conflicting)
        .map(|(_, shown)| shown.lines.start)
        .collect()
    } else {
      open
    };
    if backward {
      conflicts
        .iter()
        .rev()
        .find(|start| **start < line)
        .or_else(|| conflicts.last())
        .copied()
    } else {
      conflicts
        .iter()
        .find(|start| **start > line)
        .or_else(|| conflicts.first())
        .copied()
    }
  });
  if let Some(target) = target {
    let at = Position {
      line: u32_of(target),
      column: 0,
    };
    let _ = layouts::reveal(&tab(&key), RESULT, ApiRange { start: at, end: at });
  }
}

/// Makes a range of the result what a link chose (`<range>:<state>:<the
/// result's version>`), if the result hasn't changed since.
pub fn accept(context: &CommandContext) {
  let (Some(from), Some(argument)) = (
    context
      .invoked_from
      .as_ref()
      .filter(|from| from.tab.layout == LAYOUT),
    context.argument.as_deref(),
  ) else {
    return;
  };
  let mut parts = argument.split(':');
  let (Some(index), Some(to), Some(version)) = (
    parts.next().and_then(|index| index.parse::<usize>().ok()),
    parts.next().and_then(state_of),
    parts.next().and_then(|version| version.parse::<u64>().ok()),
  ) else {
    return;
  };
  let key = from.tab.key.clone();
  let Some(documents) = TABS.with_borrow(|tabs| tabs.get(&key)?.documents) else {
    return;
  };
  let Some(([base, incoming, current, result], now)) = texts(documents) else {
    return;
  };
  if now != version {
    // Edited since the links were made: they're about to be made again.
    return;
  }
  let lines = MergeTexts::new(&base, &incoming, &current);
  let Ok(found) = find(&base, &incoming, &current, &result, now) else {
    return;
  };
  let (Some(range), Some(shown)) = (found.ranges.get(index), found.shown.get(index)) else {
    return;
  };
  let with = lines.lines_for(range, to);
  let source = with.join("\n");
  let splices = flareite_diff::splices(&result, &source, &[(shown.lines.clone(), 0..with.len())]);
  if let Err(problem) = host::apply_edits(documents[3], now, &text::edits(&result, &splices)) {
    crate::error(&format!("Accepting the change failed: {problem:?}"));
  }
}

/// Complete Merge: asks first while conflicts are unresolved.
pub fn complete(context: &CommandContext) {
  let Some((key, _)) = tab_in_use(context) else {
    return;
  };
  let remaining = TABS.with_borrow(|tabs| tabs.get(&key).map_or(0, |tab| tab.counts.0));
  if remaining == 0 {
    return finish(&key);
  }
  crate::ask_confirm(
    &format!(
      "{remaining} {} not resolved yet.",
      if remaining == 1 {
        "conflict is"
      } else {
        "conflicts are"
      }
    ),
    "The result keeps the base's lines where a conflict is unresolved.",
    &["Complete Anyway"],
    false,
    crate::Pending::CompleteMerge(key),
  );
}

/// Saves the result, stages it, and closes the tab.
pub fn finish(key: &str) {
  let Some(documents) = TABS.with_borrow(|tabs| tabs.get(key)?.documents) else {
    return;
  };
  if let Err(problem) = documents::save(documents[3]) {
    return crate::error(&format!("Saving the result failed: {problem:?}"));
  }
  crate::stage_merged(key);
  layouts::close(&tab(key));
}

/// Opens the result file by itself.
pub fn open_file(context: &CommandContext) {
  if let Some((key, _)) = tab_in_use(context)
    && let Err(problem) = flareite_extension_api::workspace::open_file(&key)
  {
    crate::error(&format!("Opening the file failed: {problem:?}"));
  }
}

/// A document changed: tabs showing it compare again.
pub fn changed(document: u64) {
  let keys: Vec<String> = TABS.with_borrow(|tabs| {
    tabs
      .iter()
      .filter(|(_, tab)| {
        tab
          .documents
          .is_some_and(|documents| documents.contains(&document))
      })
      .map(|(key, _)| key.clone())
      .collect()
  });
  for key in keys {
    compare(&key);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn range(conflicting: bool) -> MergeRange {
    MergeRange {
      base: 1..2,
      incoming: 1..2,
      current: 1..2,
      conflicting,
    }
  }

  #[test]
  fn accepting_toggles_a_side() {
    let conflict = range(true);
    assert_eq!(
      toggled(&conflict, RangeState::Base, Side::Incoming),
      RangeState::Incoming
    );
    // The second side accepted keeps both, the first one first.
    assert_eq!(
      toggled(&conflict, RangeState::Current, Side::Incoming),
      RangeState::CurrentIncoming
    );
    assert_eq!(
      toggled(&conflict, RangeState::CurrentIncoming, Side::Current),
      RangeState::Incoming
    );
    assert_eq!(
      toggled(&range(false), RangeState::Incoming, Side::Incoming),
      RangeState::Base
    );
  }

  #[test]
  fn blocks_follow_the_base() {
    let found = find(
      "a\nb\nc\nd\n",
      "a\nB\nc\nd\n",
      "a\nb\nc\nD\nE\n",
      "a\nb\nc\nd\n",
      1,
    )
    .expect("small");
    let blocks = blocks(&found);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].lines[0], 1..2);
    assert_eq!(blocks[1].lines[1], 3..5);
    assert_eq!(
      links(&found, 0, INCOMING).links[0].argument.as_deref(),
      Some("0:incoming:1")
    );
  }
}
