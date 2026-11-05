//! Merge conflicts in files, as Git marks them:
//!
//! ```text
//! <<<<<<< HEAD
//! the current change
//! ||||||| base (only with merge.conflictStyle diff3 or zdiff3)
//! what both changed from
//! =======
//! the incoming change
//! >>>>>>> feature
//! ```
//!
//! Open documents of conflicted files show each conflict's sides in color,
//! with links above it to accept the current change, the incoming one, or
//! both, to compare them, or to resolve the file in the merge editor, as VS
//! Code does. The links run commands with the conflict's first line; the
//! document is read again then, so an edit made meanwhile is accounted for.

use std::ops::Range;

use flareite_extension_api::host;
use flareite_extension_api::types::{
  ApiError, CommandContext, DocumentChange, DocumentId, DocumentInfo, LogLevel, Position,
  Range as WitRange, TextEdit,
};
use flareite_extension_api::workbench::{self, CodeLens};

/// One conflict's lines (zero-based) in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
  /// The `<<<<<<<` line.
  pub start: usize,
  pub current: Range<usize>,
  /// The `|||||||` line and the base's lines, when Git wrote them.
  pub base: Option<(usize, Range<usize>)>,
  /// The `=======` line.
  pub splitter: usize,
  pub incoming: Range<usize>,
  /// The `>>>>>>>` line.
  pub end: usize,
}

/// What to keep of a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
  Current,
  Incoming,
  /// The current change, then the incoming one.
  Both,
}

/// Whether `line` is a marker: seven of `mark`, then the end of the line or
/// a space (and, for the splitter, only white space).
fn marker(line: &str, mark: char, label: bool) -> bool {
  let line = line.strip_suffix('\r').unwrap_or(line);
  let Some(rest) = line.strip_prefix(&mark.to_string().repeat(7)) else {
    return false;
  };
  if label {
    rest.is_empty() || rest.starts_with(' ')
  } else {
    rest.trim().is_empty()
  }
}

/// The conflicts in `lines`, in order. A start marker inside a conflict
/// starts it again, as the one before wasn't complete.
pub fn parse<S: AsRef<str>>(lines: &[S]) -> Vec<Conflict> {
  enum At {
    Outside,
    Current {
      start: usize,
    },
    Base {
      start: usize,
      base: usize,
    },
    Incoming {
      start: usize,
      base: Option<usize>,
      splitter: usize,
    },
  }
  let mut found = Vec::new();
  let mut at = At::Outside;
  for (index, line) in lines.iter().enumerate() {
    let line = line.as_ref();
    if marker(line, '<', true) {
      at = At::Current { start: index };
      continue;
    }
    at = match at {
      At::Outside => At::Outside,
      At::Current { start } if marker(line, '|', true) => At::Base { start, base: index },
      At::Current { start } if marker(line, '=', false) => At::Incoming {
        start,
        base: None,
        splitter: index,
      },
      At::Base { start, base } if marker(line, '=', false) => At::Incoming {
        start,
        base: Some(base),
        splitter: index,
      },
      At::Incoming {
        start,
        base,
        splitter,
      } if marker(line, '>', true) => {
        found.push(Conflict {
          start,
          current: start + 1..base.unwrap_or(splitter),
          base: base.map(|base| (base, base + 1..splitter)),
          splitter,
          incoming: splitter + 1..index,
          end: index,
        });
        At::Outside
      }
      other => other,
    };
  }
  found
}

/// Whether `text` has a conflict.
pub fn has_conflicts(text: &str) -> bool {
  let lines: Vec<&str> = text.split('\n').collect();
  !parse(&lines).is_empty()
}

/// The lines a conflict becomes.
pub fn resolve<'a, S: AsRef<str>>(
  lines: &'a [S],
  conflict: &Conflict,
  choice: Choice,
) -> Vec<&'a str> {
  let side = |range: &Range<usize>| lines[range.clone()].iter().map(AsRef::as_ref);
  match choice {
    Choice::Current => side(&conflict.current).collect(),
    Choice::Incoming => side(&conflict.incoming).collect(),
    Choice::Both => side(&conflict.current)
      .chain(side(&conflict.incoming))
      .collect(),
  }
}

/// `text` with every conflict resolved one way, for comparing the sides.
pub fn resolve_all(text: &str, choice: Choice) -> String {
  let lines: Vec<&str> = text.split('\n').collect();
  let mut out: Vec<&str> = Vec::with_capacity(lines.len());
  let mut at = 0;
  for conflict in parse(&lines) {
    out.extend_from_slice(&lines[at..conflict.start]);
    out.extend(resolve(&lines, &conflict, choice));
    at = conflict.end + 1;
  }
  out.extend_from_slice(&lines[at.min(lines.len())..]);
  out.join("\n")
}

/// The conflict `line` is in (from its start marker to its end marker).
pub fn at_line(conflicts: &[Conflict], line: usize) -> Option<&Conflict> {
  conflicts
    .iter()
    .find(|conflict| conflict.start <= line && line <= conflict.end)
}

// Documents of conflicted files.

/// The decoration types, as the manifest declares them.
const CURRENT_HEADER: &str = "conflict-current-header";
const CURRENT: &str = "conflict-current";
const BASE_HEADER: &str = "conflict-base-header";
const BASE: &str = "conflict-base";
const INCOMING: &str = "conflict-incoming";
const INCOMING_HEADER: &str = "conflict-incoming-header";
const DECORATIONS: [&str; 6] = [
  CURRENT_HEADER,
  CURRENT,
  BASE_HEADER,
  BASE,
  INCOMING,
  INCOMING_HEADER,
];
/// Larger documents aren't searched for conflicts as they're edited.
const MAX_LINES: u32 = 100_000;

fn position(line: usize, column: usize) -> Position {
  Position {
    line: u32::try_from(line).unwrap_or(u32::MAX),
    column: u32::try_from(column).unwrap_or(u32::MAX),
  }
}

/// Whole lines `lines`, if there are any.
fn whole_lines(text: &[String], lines: Range<usize>) -> Option<WitRange> {
  (!lines.is_empty()).then(|| WitRange {
    start: position(lines.start, 0),
    end: position(lines.end - 1, text[lines.end - 1].len()),
  })
}

/// A document's lines and their version.
fn read(id: DocumentId) -> Option<(u64, Vec<String>)> {
  let info = host::document(id).ok()?;
  if info.line_count > MAX_LINES {
    return None;
  }
  let slice = host::read_lines(id, 0, info.line_count).ok()?;
  Some((slice.version, slice.lines))
}

/// Shows a document's conflicts: their sides in color, and links above
/// each. Returns whether it has any.
pub fn show(id: DocumentId) -> bool {
  let Some((version, lines)) = read(id) else {
    clear(id);
    return false;
  };
  let found = parse(&lines);
  let mut ranges: [Vec<WitRange>; 6] = Default::default();
  let mut lenses = Vec::new();
  for conflict in &found {
    let header = |line: usize| whole_lines(&lines, line..line + 1);
    ranges[0].extend(header(conflict.start));
    ranges[1].extend(whole_lines(&lines, conflict.current.clone()));
    if let Some((marker, base)) = &conflict.base {
      ranges[2].extend(header(*marker));
      ranges[3].extend(whole_lines(&lines, base.clone()));
    }
    ranges[4].extend(whole_lines(&lines, conflict.incoming.clone()));
    ranges[5].extend(header(conflict.end));
    let line = u32::try_from(conflict.start).unwrap_or(u32::MAX);
    let argument = Some(conflict.start.to_string());
    for (title, command) in [
      ("Accept Current Change", "acceptCurrent"),
      ("Accept Incoming Change", "acceptIncoming"),
      ("Accept Both Changes", "acceptBoth"),
      ("Compare Changes", "compareConflict"),
      ("Resolve in Merge Editor", "openMergeEditor"),
    ] {
      lenses.push(CodeLens {
        line,
        title: title.into(),
        tooltip: None,
        command: format!("flareite.git.{command}"),
        argument: argument.clone(),
      });
    }
  }
  for (decoration, ranges) in DECORATIONS.iter().zip(&ranges) {
    if let Err(problem) = workbench::set_decorations(id, version, decoration, ranges) {
      crate::log(
        LogLevel::Debug,
        &format!("can't mark conflicts: {problem:?}"),
      );
      return false;
    }
  }
  let _ = workbench::set_code_lenses(id, version, &lenses);
  let any = !found.is_empty();
  crate::with_state(|state| {
    if any {
      state.conflicted_documents.insert(id);
    } else {
      state.conflicted_documents.remove(&id);
    }
  });
  any
}

/// Stops showing a document's conflicts.
fn clear(id: DocumentId) {
  if crate::with_state(|state| state.conflicted_documents.remove(&id))
    && let Ok(info) = host::document(id)
  {
    for decoration in DECORATIONS {
      let _ = workbench::set_decorations(id, info.version, decoration, &[]);
    }
    let _ = workbench::set_code_lenses(id, info.version, &[]);
  }
}

/// Whether a document's conflicts are shown, or its file has some.
fn wanted(id: DocumentId, path: Option<&str>) -> bool {
  crate::with_state(|state| state.conflicted_documents.contains(&id))
    || path.is_some_and(crate::is_conflicted)
}

pub fn opened(document: &DocumentInfo) {
  if wanted(document.id, document.path.as_deref()) {
    show(document.id);
  }
}

pub fn changed(change: &DocumentChange) {
  if crate::with_state(|state| state.conflicted_documents.contains(&change.document)) {
    show(change.document);
  }
}

pub fn closed(id: DocumentId) {
  crate::with_state(|state| state.conflicted_documents.remove(&id));
}

/// After a status: shows the conflicts of open documents of conflicted
/// files, and of those that had some.
pub fn refresh_open() {
  let Ok(documents) = host::documents() else {
    return;
  };
  for document in documents {
    if wanted(document.id, document.path.as_deref()) {
      show(document.id);
    }
  }
}

/// The edit that resolves `conflict`.
fn replacement(lines: &[String], conflict: &Conflict, choice: Choice) -> TextEdit {
  let kept = resolve(lines, conflict, choice);
  let end_of = |line: usize| position(line, lines[line].len());
  if conflict.end + 1 < lines.len() {
    return TextEdit {
      range: WitRange {
        start: position(conflict.start, 0),
        end: position(conflict.end + 1, 0),
      },
      text: kept.iter().map(|line| format!("{line}\n")).collect(),
    };
  }
  // The conflict ends the text, without a line break after it.
  if kept.is_empty() && conflict.start > 0 {
    return TextEdit {
      range: WitRange {
        start: end_of(conflict.start - 1),
        end: end_of(conflict.end),
      },
      text: String::new(),
    };
  }
  TextEdit {
    range: WitRange {
      start: position(conflict.start, 0),
      end: end_of(conflict.end),
    },
    text: kept.join("\n"),
  }
}

/// The document a command is for, and the line: the code lens's, or the
/// cursor's.
fn target(context: &CommandContext) -> Option<(DocumentId, usize)> {
  let id = context.document?;
  let line = workbench::command_argument()
    .and_then(|argument| argument.parse().ok())
    .or_else(|| {
      context
        .selections
        .first()
        .map(|range| range.start.line as usize)
    })?;
  Some((id, line))
}

/// Keeps one side of the conflict at the cursor (or the code lens's), or
/// both.
pub fn accept(context: &CommandContext, choice: Choice) {
  let Some((id, line)) = target(context) else {
    return crate::info("Open a file with merge conflicts first.");
  };
  let Some((version, lines)) = read(id) else {
    return;
  };
  let found = parse(&lines);
  let Some(conflict) = at_line(&found, line) else {
    return crate::info("There's no merge conflict there.");
  };
  apply(id, version, &[replacement(&lines, conflict, choice)]);
}

/// Keeps one side of every conflict in the document, or both.
pub fn accept_all(context: &CommandContext, choice: Choice) {
  let Some(id) = context.document else {
    return crate::info("Open a file with merge conflicts first.");
  };
  let Some((version, lines)) = read(id) else {
    return;
  };
  let edits: Vec<TextEdit> = parse(&lines)
    .iter()
    .map(|conflict| replacement(&lines, conflict, choice))
    .collect();
  if edits.is_empty() {
    return crate::info("The file has no merge conflicts.");
  }
  apply(id, version, &edits);
}

fn apply(id: DocumentId, version: u64, edits: &[TextEdit]) {
  match host::apply_edits(id, version, edits) {
    Ok(_) => {}
    Err(ApiError::StaleVersion(_)) => {
      crate::info("The file changed meanwhile; try again.");
    }
    Err(ApiError::PermissionDenied(_)) => crate::error(
      "Resolving conflicts needs Git's permission to edit documents (in the Extensions view).",
    ),
    Err(problem) => crate::error(&format!("Resolving the conflict failed: {problem:?}")),
  }
}

/// Moves the cursor to the next conflict (or the previous one), around the
/// end.
pub fn go_to(context: &CommandContext, backward: bool) {
  let Some(id) = context.document else {
    return;
  };
  let line = context
    .selections
    .first()
    .map_or(0, |range| range.start.line as usize);
  let Some((_, lines)) = read(id) else {
    return;
  };
  let found = parse(&lines);
  let next = if backward {
    found
      .iter()
      .rev()
      .find(|conflict| conflict.start < line)
      .or(found.last())
  } else {
    found
      .iter()
      .find(|conflict| conflict.start > line)
      .or(found.first())
  };
  match next {
    Some(conflict) => {
      let at = position(conflict.start, 0);
      let _ = workbench::reveal(id, WitRange { start: at, end: at });
    }
    None => crate::info("The file has no merge conflicts."),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resolving_replaces_the_conflict_lines() {
    let text: Vec<String> = TEXT.split('\n').map(Into::into).collect();
    let found = parse(&text);
    let edit = replacement(&text, &found[0], Choice::Both);
    assert_eq!((edit.range.start.line, edit.range.end.line), (1, 6));
    assert_eq!(edit.range.end.column, 0);
    assert_eq!(edit.text, "mine\ntheirs\n");
    // The last conflict, kept empty: the line break before it goes too.
    let edit = replacement(&text, &found[1], Choice::Current);
    assert_eq!((edit.range.start.line, edit.range.start.column), (6, 3));
    assert_eq!(edit.text, "");
    let edit = replacement(&text, &found[1], Choice::Incoming);
    assert_eq!((edit.range.start.line, edit.range.start.column), (7, 0));
    assert_eq!(edit.text, "new");
  }

  const TEXT: &str = "one
<<<<<<< HEAD
mine
=======
theirs
>>>>>>> feature
two
<<<<<<< HEAD
||||||| base
old
=======
new
>>>>>>> feature";

  fn lines(text: &str) -> Vec<&str> {
    text.split('\n').collect()
  }

  #[test]
  fn conflicts_are_found_with_their_parts() {
    let found = parse(&lines(TEXT));
    assert_eq!(
      found,
      vec![
        Conflict {
          start: 1,
          current: 2..3,
          base: None,
          splitter: 3,
          incoming: 4..5,
          end: 5,
        },
        Conflict {
          start: 7,
          current: 8..8,
          base: Some((8, 9..10)),
          splitter: 10,
          incoming: 11..12,
          end: 12,
        },
      ]
    );
    assert_eq!(at_line(&found, 4), Some(&found[0]));
    assert_eq!(at_line(&found, 6), None);
  }

  #[test]
  fn markers_need_their_seven_characters_alone() {
    assert!(parse(&lines("<<<<<<<< x\n=======\n>>>>>>>")).is_empty());
    assert!(parse(&lines("<<<<<<<\n======= no\n>>>>>>>")).is_empty());
    assert_eq!(parse(&lines("<<<<<<<\r\n=======\r\n>>>>>>> b\r")).len(), 1);
    // Incomplete: started again.
    let found = parse(&lines("<<<<<<< a\nx\n<<<<<<< b\ny\n=======\nz\n>>>>>>> c"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].start, 2);
  }

  #[test]
  fn conflicts_resolve_to_a_side_or_both() {
    let text = lines(TEXT);
    let found = parse(&text);
    assert_eq!(resolve(&text, &found[0], Choice::Current), ["mine"]);
    assert_eq!(resolve(&text, &found[0], Choice::Incoming), ["theirs"]);
    assert_eq!(resolve(&text, &found[0], Choice::Both), ["mine", "theirs"]);
    assert_eq!(resolve_all(TEXT, Choice::Current), "one\nmine\ntwo");
    assert_eq!(resolve_all(TEXT, Choice::Incoming), "one\ntheirs\ntwo\nnew");
    assert!(has_conflicts(TEXT));
    assert!(!has_conflicts("one\n=======\n"));
  }
}
