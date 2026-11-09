//! Diff tabs: two versions of a file compared, as the editor's `diff`
//! layout of this extension (API 1.15).
//!
//! A tab's key names its sides (`[original, modified, title]`), each a
//! file's absolute path or a `git:` address. Side by side, each side is an
//! editor pane and the two scroll together; inline (by choice, or when the
//! tab is narrower than 900 pixels), the modified side is the only editor
//! and the removed lines show above the lines that replaced them. The
//! changes are found here with `flareite-diff` whenever either side's text
//! changes, and drawn as pane decorations: changed lines and characters
//! colored, filler where a change has fewer lines on one side so the lines
//! after it stay level, unchanged lines away from changes folded behind a
//! bar that shows them, and marks beside the scroll bar.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use flareite_diff::{DiffError, DiffOptions, LineChange};
use flareite_extension_api::layouts::{
  self, Button, Decorated, Direction, EditorPane, FillReason, Layout, LayoutNode, LayoutTab,
  LineRange, NodeKind, Pane, PaneContent, PaneDecorations, RulerMark, Zone, ZoneContent, ZoneLines,
  ZoneText,
};
use flareite_extension_api::types::{ConfigValue, LogLevel, Position, Range as ApiRange};
use flareite_extension_api::{documents, host, quick_diff, workspace};

use crate::diff::Sides;
use crate::text::{self, decode_key, encode_key, whole_lines};

pub const LAYOUT: &str = "diff";
/// Narrower than this, a diff shows inline, as in VS Code.
const INLINE_BELOW: u32 = 900;
/// Unchanged lines kept around a change when the others collapse.
const CONTEXT_LINES: usize = 3;
/// Fewer unchanged lines than this don't collapse.
const MIN_COLLAPSED: usize = 3;
/// The panes, by id.
const ORIGINAL: &str = "original";
const MODIFIED: &str = "modified";

/// An open diff tab.
struct Tab {
  sides: Sides,
  width: u32,
  /// Whether it's laid out inline.
  inline: bool,
  /// The sides' documents, once open.
  documents: Option<(u64, u64)>,
  /// The texts last compared, and their changes.
  texts: Option<(String, String)>,
  changes: Vec<LineChange>,
  /// Why the texts couldn't be compared.
  problem: Option<String>,
  /// Unchanged stretches the user opened, by their first original line.
  revealed: BTreeSet<usize>,
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

fn flag(key: &str, default: bool) -> bool {
  match host::configuration(key) {
    Some(ConfigValue::Boolean(value)) => value,
    _ => default,
  }
}

/// Both sides in one editor, by choice.
fn inline_chosen() -> bool {
  flag("diff_inline", false)
}

fn ignore_whitespace() -> bool {
  flag("diff_ignore_whitespace", true)
}

fn collapse_unchanged() -> bool {
  flag("diff_collapse_unchanged", true)
}

fn is_file(uri: &str) -> bool {
  crate::diff::parse(uri).is_none()
}

/// Shows two versions of a file compared, in a tab of their own.
pub fn open(sides: Sides) {
  let key = encode_key(&[&sides.original, &sides.modified, &sides.title]);
  if let Err(problem) = layouts::open(&tab(&key), false) {
    crate::error(&format!("Showing the changes failed: {problem:?}"));
  }
}

/// The layout's command: fills a tab in, or hears that it changed.
pub fn fill() {
  let Some(request) = layouts::filling() else {
    return;
  };
  let key = request.tab.key;
  match request.reason {
    FillReason::Opened | FillReason::Restored | FillReason::Resized => {
      let Some([original, modified, title]) =
        decode_key(&key).and_then(|parts| <[String; 3]>::try_from(parts).ok())
      else {
        return crate::log(LogLevel::Warn, &format!("not a diff tab: {key}"));
      };
      TABS.with_borrow_mut(|tabs| {
        let tab = tabs.entry(key.clone()).or_insert_with(|| Tab {
          sides: Sides {
            original,
            modified,
            title,
          },
          width: 0,
          inline: false,
          documents: None,
          texts: None,
          changes: Vec::new(),
          problem: None,
          revealed: BTreeSet::new(),
        });
        if request.width > 0 {
          tab.width = request.width;
        }
      });
      lay_out(&key);
    }
    FillReason::Ready => {
      let documents = layouts::documents(&tab(&key)).unwrap_or_default();
      let ready = TABS.with_borrow_mut(|tabs| {
        let tab = tabs.get_mut(&key)?;
        let find = |uri: &str| {
          documents
            .iter()
            .find(|document| document.uri == uri)
            .map(|document| document.document)
        };
        tab.documents = Some((find(&tab.sides.original)?, find(&tab.sides.modified)?));
        Some(())
      });
      if ready.is_some() {
        compare(&key);
      }
    }
    FillReason::Closed => {
      TABS.with_borrow_mut(|tabs| tabs.remove(&key));
    }
  }
}

/// Sets a tab's layout: side by side or inline, with its buttons.
fn lay_out(key: &str) {
  let Some((sides, inline)) = TABS.with_borrow_mut(|tabs| {
    let tab = tabs.get_mut(key)?;
    tab.inline = inline_chosen() || (tab.width > 0 && tab.width < INLINE_BELOW);
    Some((tab.sides.clone(), tab.inline))
  }) else {
    return;
  };
  let editor = |uri: &str| {
    NodeKind::Pane(Pane {
      content: PaneContent::Editor(EditorPane {
        uri: uri.to_owned(),
        minimap: false,
        folding: false,
        quick_diff: false,
      }),
      header: None,
      scroll_group: Some("sides".into()),
    })
  };
  let nodes = if inline {
    vec![LayoutNode {
      id: MODIFIED.into(),
      parent: None,
      size: 1.0,
      kind: editor(&sides.modified),
    }]
  } else {
    vec![
      LayoutNode {
        id: "sides".into(),
        parent: None,
        size: 1.0,
        kind: NodeKind::Split(Direction::Row),
      },
      LayoutNode {
        id: ORIGINAL.into(),
        parent: Some("sides".into()),
        size: 1.0,
        kind: editor(&sides.original),
      },
      LayoutNode {
        id: MODIFIED.into(),
        parent: Some("sides".into()),
        size: 1.0,
        kind: editor(&sides.modified),
      },
    ]
  };
  let layout = Layout {
    title: sides.title.clone(),
    nodes,
    // Inline, the original's lines show in zones: it stays open.
    documents: if inline {
      vec![sides.original.clone()]
    } else {
      Vec::new()
    },
    buttons: buttons(&sides),
    breakpoints: vec![INLINE_BELOW],
    main: Some(MODIFIED.into()),
  };
  match layouts::set_layout(&tab(key), &layout) {
    Ok(_) => decorate(key),
    Err(problem) => crate::error(&format!("Showing the changes failed: {problem:?}")),
  }
}

fn button(id: &str, title: &str, icon: &str, command: &str, pressed: Option<bool>) -> Button {
  Button {
    id: id.into(),
    title: title.into(),
    icon: Some(icon.into()),
    command: format!("flareite.git.{command}"),
    argument: None,
    primary: false,
    pressed,
    disabled: false,
  }
}

/// The buttons beside a diff tab.
fn buttons(sides: &Sides) -> Vec<Button> {
  let mut buttons = Vec::new();
  if is_file(&sides.modified) {
    // Changes not staged: the original is the index.
    if matches!(crate::diff::parse(&sides.original), Some(("", _))) {
      buttons.push(button(
        "stage",
        "Stage Selected Ranges",
        "stage",
        "diffStageSelected",
        None,
      ));
    }
    buttons.push(button(
      "revert",
      "Revert Selected Ranges",
      "undo",
      "diffRevertSelected",
      None,
    ));
  }
  buttons.extend([
    button(
      "previous",
      "Previous Change (Shift+F7)",
      "arrow-up",
      "previousChange",
      None,
    ),
    button("next", "Next Change (F7)", "arrow-down", "nextChange", None),
    button(
      "inline",
      "Side by Side",
      "split",
      "diffToggleInline",
      Some(!inline_chosen()),
    ),
    button(
      "whitespace",
      "Show Leading and Trailing Whitespace Differences",
      "pilcrow",
      "diffToggleWhitespace",
      Some(!ignore_whitespace()),
    ),
    button(
      "collapse",
      "Collapse Unchanged Lines",
      "fold",
      "diffToggleUnchanged",
      Some(collapse_unchanged()),
    ),
    button("open", "Open File", "file-text", "diffOpenFile", None),
  ]);
  buttons
}

/// Compares a tab's sides again, as they are now, and decorates its panes.
fn compare(key: &str) {
  let Some((original, modified)) =
    TABS.with_borrow(|tabs| tabs.get(key).and_then(|tab| tab.documents))
  else {
    return;
  };
  let (Some((before, _)), Some((after, _))) =
    (text::document_text(original), text::document_text(modified))
  else {
    return;
  };
  let options = DiffOptions {
    ignore_trim_whitespace: ignore_whitespace(),
  };
  let outcome = flareite_diff::diff(&before, &after, options);
  TABS.with_borrow_mut(|tabs| {
    let Some(tab) = tabs.get_mut(key) else {
      return;
    };
    match outcome {
      Ok(changes) => {
        tab.changes = changes;
        tab.problem = None;
      }
      Err(DiffError::TooLarge) => {
        tab.changes = Vec::new();
        tab.problem = Some(format!(
          "Files with more than {} lines aren't compared.",
          flareite_diff::MAX_LINES
        ));
      }
    }
    tab.texts = Some((before, after));
  });
  decorate(key);
}

/// Unchanged lines hidden on both sides: as many on each.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Collapsed {
  /// The unchanged stretch's first original line, which names it.
  key: usize,
  original: Range<usize>,
  modified: Range<usize>,
}

/// The unchanged lines to collapse, leaving `CONTEXT_LINES` next to each
/// change, the first line of the file, and the stretches in `revealed`.
fn collapsed_regions(
  changes: &[LineChange],
  line_count: usize,
  revealed: &BTreeSet<usize>,
) -> Vec<Collapsed> {
  let mut regions = Vec::new();
  let mut add = |original: usize, modified: usize, length: usize, first: bool, last: bool| {
    // The line before hidden lines stays, so the first line always does.
    let from = (if first { 0 } else { CONTEXT_LINES }).max(usize::from(original == 0));
    let to = length.saturating_sub(if last { 0 } else { CONTEXT_LINES });
    if to >= from + MIN_COLLAPSED && !revealed.contains(&original) {
      regions.push(Collapsed {
        key: original,
        original: original + from..original + to,
        modified: modified + from..modified + to,
      });
    }
  };
  let (mut original, mut modified) = (0, 0);
  for (index, change) in changes.iter().enumerate() {
    add(
      original,
      modified,
      change.original.start.saturating_sub(original),
      index == 0,
      false,
    );
    original = change.original.end;
    modified = change.modified.end;
  }
  add(
    original,
    modified,
    line_count.saturating_sub(modified),
    changes.is_empty(),
    true,
  );
  regions
}

fn u32_of(value: usize) -> u32 {
  u32::try_from(value).unwrap_or(u32::MAX)
}

fn line_range(lines: &Range<usize>) -> LineRange {
  LineRange {
    start: u32_of(lines.start),
    end: u32_of(lines.end),
  }
}

/// The bar standing for collapsed lines, above the first of them.
fn collapsed_zone(region: &Collapsed, line: usize) -> Zone {
  let count = region.original.end - region.original.start;
  Zone {
    line: u32_of(line),
    content: ZoneContent::Text(ZoneText {
      text: format!(
        "\u{22ef} {count} unchanged {}",
        if count == 1 { "line" } else { "lines" }
      ),
      tooltip: Some("Show the unchanged lines".into()),
      command: Some("flareite.git.diffReveal".into()),
      argument: Some(region.key.to_string()),
    }),
    decoration: None,
  }
}

/// What a tab's panes show besides their text, from its last comparison.
fn decorate(key: &str) {
  let Some(shown) = TABS.with_borrow(|tabs| {
    let tab = tabs.get(key)?;
    let (before, after) = tab.texts.as_ref()?;
    Some(panes(tab, before, after))
  }) else {
    return;
  };
  for (pane, decorations) in shown {
    if let Err(problem) = layouts::set_pane_decorations(&tab(key), pane, &decorations) {
      crate::log(
        LogLevel::Warn,
        &format!("can't show the changes in {key}: {problem:?}"),
      );
    }
  }
}

fn ranges(decoration: &str, ranges: Vec<ApiRange>) -> Decorated {
  Decorated {
    decoration: decoration.into(),
    ranges,
  }
}

/// The decorations of a tab's panes (the original's only side by side).
fn panes(tab: &Tab, before: &str, after: &str) -> Vec<(&'static str, PaneDecorations)> {
  let original_lines = text::lines(before);
  let modified_lines = text::lines(after);
  let changes = &tab.changes;
  let collapsed = if collapse_unchanged() && tab.problem.is_none() {
    collapsed_regions(changes, modified_lines.len(), &tab.revealed)
  } else {
    Vec::new()
  };
  let chars = |lines: &[&str], original: bool| -> Vec<ApiRange> {
    changes
      .iter()
      .flat_map(|change| &change.chars)
      .map(|chars| {
        let range = if original {
          &chars.original
        } else {
          &chars.modified
        };
        ApiRange {
          start: text::position(lines, range.start.line, range.start.column),
          end: text::position(lines, range.end.line, range.end.column),
        }
      })
      .collect()
  };
  let changed_lines = |original: bool| -> Vec<ApiRange> {
    changes
      .iter()
      .map(|change| {
        if original {
          &change.original
        } else {
          &change.modified
        }
      })
      .filter(|lines| !lines.is_empty())
      .map(whole_lines)
      .collect()
  };
  let ruler = changes
    .iter()
    .map(|change| {
      let inserted = !change.modified.is_empty();
      let removed = !change.original.is_empty();
      RulerMark {
        lines: line_range(
          &(change.modified.start..change.modified.end.max(change.modified.start + 1)),
        ),
        decoration: match (removed, inserted) {
          (true, true) => "ruler-changed",
          (false, true) => "ruler-inserted",
          _ => "ruler-removed",
        }
        .into(),
      }
    })
    .collect::<Vec<_>>();
  let problem_zone = tab.problem.as_ref().map(|problem| Zone {
    line: 0,
    content: ZoneContent::Text(ZoneText {
      text: problem.clone(),
      tooltip: None,
      command: None,
      argument: None,
    }),
    decoration: None,
  });

  let mut modified = PaneDecorations {
    decorations: vec![
      ranges("inserted-line", changed_lines(false)),
      ranges("inserted-text", chars(&modified_lines, false)),
    ],
    zones: problem_zone.clone().into_iter().collect(),
    folds: collapsed
      .iter()
      .map(|region| line_range(&region.modified))
      .collect(),
    ruler,
  };
  modified.zones.extend(
    collapsed
      .iter()
      .map(|region| collapsed_zone(region, region.modified.start)),
  );
  if tab.inline {
    // The removed lines, above the lines that replaced them.
    let original_chars = chars(&original_lines, true);
    for change in changes.iter().filter(|change| !change.original.is_empty()) {
      let inside = |range: &ApiRange| {
        (range.start.line as usize) < change.original.end
          && (range.end.line as usize) >= change.original.start
      };
      modified.zones.push(Zone {
        line: u32_of(change.modified.start),
        content: ZoneContent::Lines(ZoneLines {
          uri: tab.sides.original.clone(),
          lines: line_range(&change.original),
          decorations: vec![ranges(
            "removed-text",
            original_chars
              .iter()
              .filter(|range| inside(range))
              .copied()
              .collect(),
          )],
          numbers: true,
        }),
        decoration: Some("removed-line".into()),
      });
    }
    return vec![(MODIFIED, modified)];
  }
  let mut original = PaneDecorations {
    decorations: vec![
      ranges("removed-line", changed_lines(true)),
      ranges("removed-text", chars(&original_lines, true)),
    ],
    zones: problem_zone.into_iter().collect(),
    folds: collapsed
      .iter()
      .map(|region| line_range(&region.original))
      .collect(),
    ruler: Vec::new(),
  };
  original.zones.extend(
    collapsed
      .iter()
      .map(|region| collapsed_zone(region, region.original.start)),
  );
  // Filler below a change on the side with fewer lines.
  for change in changes {
    let removed = change.original.len();
    let inserted = change.modified.len();
    let filler = |line: usize, rows: usize| Zone {
      line: u32_of(line),
      content: ZoneContent::Filler(u32_of(rows)),
      decoration: None,
    };
    if removed < inserted {
      original
        .zones
        .push(filler(change.original.end, inserted - removed));
    } else if inserted < removed {
      modified
        .zones
        .push(filler(change.modified.end, removed - inserted));
    }
  }
  vec![(ORIGINAL, original), (MODIFIED, modified)]
}

/// The diff tab a command is for: the clicked one's, else the one in use.
fn tab_in_use() -> Option<(String, Option<String>, Vec<ApiRange>)> {
  let active = layouts::active_pane().filter(|active| active.tab.layout == LAYOUT);
  if let Some(from) = layouts::invoked().filter(|from| from.tab.layout == LAYOUT) {
    let selections = active
      .filter(|active| active.tab.key == from.tab.key)
      .map(|active| (Some(active.pane), active.selections))
      .unwrap_or((None, Vec::new()));
    return Some((from.tab.key, selections.0, selections.1));
  }
  active.map(|active| (active.tab.key, Some(active.pane), active.selections))
}

/// Moves to a diff tab's next change after the cursor (or the one before
/// it), wrapping around.
pub fn go_to_change(backward: bool) {
  let Some((key, pane, selections)) = tab_in_use() else {
    return;
  };
  let pane = pane.unwrap_or_else(|| MODIFIED.into());
  let line = selections
    .first()
    .map_or(0, |range| range.start.line as usize);
  let target = TABS.with_borrow(|tabs| {
    let tab = tabs.get(&key)?;
    let side = |change: &LineChange| {
      if pane == ORIGINAL {
        change.original.start
      } else {
        change.modified.start
      }
    };
    let changes = &tab.changes;
    let found = if backward {
      changes
        .iter()
        .rev()
        .find(|change| side(change) < line)
        .or_else(|| changes.last())
    } else {
      changes
        .iter()
        .find(|change| side(change) > line)
        .or_else(|| changes.first())
    };
    found.map(side)
  });
  if let Some(target) = target {
    let at = Position {
      line: u32_of(target),
      column: 0,
    };
    let _ = layouts::reveal(&tab(&key), &pane, ApiRange { start: at, end: at });
  }
}

/// Shows an unchanged stretch the user clicked.
pub fn reveal() {
  let (Some(from), Some(region)) = (
    layouts::invoked().filter(|from| from.tab.layout == LAYOUT),
    flareite_extension_api::workbench::command_argument().and_then(|key| key.parse().ok()),
  ) else {
    return;
  };
  TABS.with_borrow_mut(|tabs| {
    if let Some(tab) = tabs.get_mut(&from.tab.key) {
      tab.revealed.insert(region);
    }
  });
  decorate(&from.tab.key);
}

/// Turns one of the diff settings on or off, and shows every diff tab again.
pub fn toggle(setting: &str) {
  let value = match setting {
    "diff_inline" => !inline_chosen(),
    "diff_ignore_whitespace" => !ignore_whitespace(),
    _ => !collapse_unchanged(),
  };
  if let Err(problem) = host::set_configuration(setting, Some(&ConfigValue::Boolean(value))) {
    return crate::error(&format!("Changing the setting failed: {problem:?}"));
  }
  settings_changed(setting);
}

/// A diff setting changed: every diff tab shows it.
fn settings_changed(setting: &str) {
  let keys: Vec<String> = TABS.with_borrow(|tabs| tabs.keys().cloned().collect());
  for key in keys {
    match setting {
      "diff_inline" => lay_out(&key),
      "diff_ignore_whitespace" => {
        lay_out(&key);
        compare(&key);
      }
      _ => {
        TABS.with_borrow_mut(|tabs| {
          if let Some(tab) = tabs.get_mut(&key) {
            tab.revealed.clear();
          }
        });
        lay_out(&key);
      }
    }
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
          .is_some_and(|(original, modified)| original == document || modified == document)
      })
      .map(|(key, _)| key.clone())
      .collect()
  });
  for key in keys {
    compare(&key);
  }
}

/// Stages the changes the selections in the tab's file touch: the
/// editor applies them to the file's original, the index.
pub fn stage_selected() {
  if let Err(problem) = quick_diff::apply_selected_ranges() {
    crate::error(&format!("Staging failed: {problem:?}"));
  }
}

/// Reverts the changes the selections in the tab's file touch, putting
/// back the original's lines.
pub fn revert_selected() {
  let Some((key, pane, selections)) = tab_in_use() else {
    return;
  };
  if pane.as_deref() != Some(MODIFIED) || selections.is_empty() {
    return crate::info("Select lines in the file's side first.");
  }
  let Some(((_, modified), (before, after), changes)) = TABS.with_borrow(|tabs| {
    let tab = tabs.get(&key)?;
    Some((tab.documents?, tab.texts.clone()?, tab.changes.clone()))
  }) else {
    return;
  };
  let lines: Vec<std::ops::RangeInclusive<usize>> = selections
    .iter()
    .map(|range| {
      let (start, end) = (range.start.line as usize, range.end.line as usize);
      // A selection ending at the start of a line doesn't take that line.
      let last = if end > start && range.end.column == 0 {
        end - 1
      } else {
        end
      };
      start..=last
    })
    .collect();
  let chosen = flareite_diff::touching(&changes, &lines);
  if chosen.is_empty() {
    return;
  }
  let splices = flareite_diff::revert(&before, &after, &chosen);
  let Some(info) = host::document(modified).ok() else {
    return;
  };
  if let Err(problem) = host::apply_edits(modified, info.version, &text::edits(&after, &splices)) {
    crate::error(&format!("Reverting failed: {problem:?}"));
  }
}

/// Opens the tab's modified side by itself.
pub fn open_file() {
  let Some((key, _, _)) = tab_in_use() else {
    return;
  };
  let Some(modified) = TABS.with_borrow(|tabs| Some(tabs.get(&key)?.sides.modified.clone())) else {
    return;
  };
  let opened = if is_file(&modified) {
    workspace::open_file(&modified).map_err(|problem| format!("{problem:?}"))
  } else {
    documents::open(&modified).map_err(|problem| format!("{problem:?}"))
  };
  if let Err(problem) = opened {
    crate::error(&format!("Opening the file failed: {problem}"));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn change(original: Range<usize>, modified: Range<usize>) -> LineChange {
    LineChange {
      original,
      modified,
      chars: Vec::new(),
    }
  }

  #[test]
  fn unchanged_lines_collapse_around_changes() {
    // 20 lines; line 10 changed.
    let changes = [change(10..11, 10..11)];
    let regions = collapsed_regions(&changes, 20, &BTreeSet::new());
    assert_eq!(
      regions,
      vec![
        Collapsed {
          key: 0,
          original: 1..7,
          modified: 1..7,
        },
        Collapsed {
          key: 11,
          original: 14..20,
          modified: 14..20,
        },
      ]
    );
    // Opened stretches stay open.
    let revealed = BTreeSet::from([0]);
    assert_eq!(collapsed_regions(&changes, 20, &revealed).len(), 1);
  }

  #[test]
  fn short_stretches_stay() {
    let changes = [change(4..5, 4..5)];
    assert!(collapsed_regions(&changes, 9, &BTreeSet::new()).is_empty());
  }

  #[test]
  fn an_unchanged_file_collapses_but_its_first_line() {
    let regions = collapsed_regions(&[], 10, &BTreeSet::new());
    assert_eq!(
      regions,
      vec![Collapsed {
        key: 0,
        original: 1..10,
        modified: 1..10,
      }]
    );
  }
}
