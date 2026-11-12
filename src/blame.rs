//! Blame, as VS Code shows it: who last changed the line the cursor is on,
//! in faint text after the line and in the status bar, with the commit's
//! details on hover.
//!
//! A document's blame comes from `git blame --porcelain --contents -` with
//! its text as it is, so lines changed and not committed say so. It's kept
//! for the document's version and read again, in the background, when the
//! cursor moves to another line of a newer version. The extension has no
//! clock, so "3 days ago" is counted from the time Git gives for a new
//! commit (`git var GIT_COMMITTER_IDENT`).

use std::collections::HashMap;

use flareite_extension_api::events::SelectionChange;
use flareite_extension_api::host;
use flareite_extension_api::processes::{ProcessError, ProcessOutput};
use flareite_extension_api::types::{ConfigValue, DocumentId, StatusItem};
use flareite_extension_api::workbench::{self, LineAnnotation};

use crate::{Job, args, options, relative_path, run_git, spawn, with_state};

/// Larger documents aren't blamed.
const MAX_LINES: u32 = 50_000;
/// A commit not made yet: lines changed since the last one.
const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

/// The commit that last changed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
  pub hash: String,
  pub author: String,
  pub mail: String,
  /// Seconds since 1970, and the author's offset from UTC (`+0100`).
  pub time: i64,
  pub zone: String,
  pub summary: String,
}

impl BlameCommit {
  fn uncommitted(&self) -> bool {
    self.hash == UNCOMMITTED
  }
}

/// A document's blame at one version.
pub struct Blame {
  version: u64,
  /// Each line's commit, by index into `commits`.
  lines: Vec<Option<usize>>,
  commits: Vec<BlameCommit>,
  /// Now, by Git's clock.
  now: i64,
  /// The user's email, whose commits are "You".
  me: Option<String>,
}

/// Blame for documents, and what's shown.
#[derive(Default)]
pub struct BlameState {
  documents: HashMap<DocumentId, Blame>,
  /// Documents being blamed.
  running: HashMap<DocumentId, u64>,
  /// The document and line the cursor is on.
  cursor: Option<(DocumentId, usize)>,
  /// The document annotated, and the commit shown.
  shown: Option<DocumentId>,
  pub hash: Option<String>,
}

/// Reads `git blame --porcelain`: each line's commit (lines are numbered
/// from 1 in it).
pub fn parse(stdout: &str) -> (Vec<Option<usize>>, Vec<BlameCommit>) {
  let mut commits: Vec<BlameCommit> = Vec::new();
  let mut by_hash: HashMap<String, usize> = HashMap::new();
  let mut lines: Vec<Option<usize>> = Vec::new();
  let mut current: Option<(usize, usize)> = None;
  for line in stdout.lines() {
    if line.starts_with('\t') {
      if let Some((commit, final_line)) = current.take() {
        if lines.len() < final_line {
          lines.resize(final_line, None);
        }
        lines[final_line - 1] = Some(commit);
      }
      continue;
    }
    let mut words = line.split(' ');
    let first = words.next().unwrap_or_default();
    if first.len() == 40 && first.bytes().all(|byte| byte.is_ascii_hexdigit()) {
      let final_line: usize = words.nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
      let index = *by_hash.entry(first.to_owned()).or_insert_with(|| {
        commits.push(BlameCommit {
          hash: first.to_owned(),
          author: String::new(),
          mail: String::new(),
          time: 0,
          zone: "+0000".into(),
          summary: String::new(),
        });
        commits.len() - 1
      });
      current = (final_line > 0).then_some((index, final_line));
      continue;
    }
    let Some((index, _)) = current else {
      continue;
    };
    let commit = &mut commits[index];
    let value = line.split_once(' ').map_or("", |(_, value)| value);
    match first {
      "author" => commit.author = value.into(),
      "author-mail" => {
        commit.mail = value.trim_start_matches('<').trim_end_matches('>').into();
      }
      "author-time" => commit.time = value.parse().unwrap_or(0),
      "author-tz" => commit.zone = value.into(),
      "summary" => commit.summary = value.into(),
      _ => {}
    }
  }
  (lines, commits)
}

/// How long ago `then` was, as Git says it.
pub fn ago(then: i64, now: i64) -> String {
  let seconds = (now - then).max(0);
  let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
  match seconds {
    0..45 => "now".into(),
    45..5_400 => plural((seconds + 30) / 60, "minute"),
    5_400..129_600 => plural((seconds + 1_800) / 3_600, "hour"),
    129_600..1_209_600 => plural((seconds + 43_200) / 86_400, "day"),
    1_209_600..5_184_000 => plural((seconds + 302_400) / 604_800, "week"),
    5_184_000..31_536_000 => plural((seconds + 1_296_000) / 2_592_000, "month"),
    _ => plural(seconds / 31_536_000, "year"),
  }
}

/// A time as the author saw it: `2026-11-04 14:03`.
pub fn date(time: i64, zone: &str) -> String {
  let sign = if zone.starts_with('-') { -1 } else { 1 };
  let digits = zone.trim_start_matches(['+', '-']);
  let offset = digits
    .get(..2)
    .and_then(|hours| hours.parse::<i64>().ok())
    .unwrap_or(0)
    * 3_600
    + digits
      .get(2..4)
      .and_then(|minutes| minutes.parse::<i64>().ok())
      .unwrap_or(0)
      * 60;
  let local = time + sign * offset;
  let days = local.div_euclid(86_400);
  let seconds = local.rem_euclid(86_400);
  // Howard Hinnant's days-to-civil.
  let z = days + 719_468;
  let era = z.div_euclid(146_097);
  let day_of_era = z.rem_euclid(146_097);
  let year_of_era =
    (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
  let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
  let mp = (5 * day_of_year + 2) / 153;
  let day = day_of_year - (153 * mp + 2) / 5 + 1;
  let month = if mp < 10 { mp + 3 } else { mp - 9 };
  let year = year_of_era + era * 400 + i64::from(month <= 2);
  format!(
    "{year:04}-{month:02}-{day:02} {:02}:{:02}",
    seconds / 3_600,
    seconds % 3_600 / 60
  )
}

fn enabled(key: &str) -> bool {
  !matches!(crate::setting(key), Some(ConfigValue::Boolean(false)))
}

/// The cursor moved to another line, or another document is in use.
pub fn selection_changed(change: &SelectionChange) {
  let cursor = change.document.and_then(|document| {
    let line = change.selections.first()?.start.line as usize;
    Some((document, line))
  });
  with_state(|state| state.blame.cursor = cursor);
  update();
}

/// A document changed: what's shown of it is out of date until the cursor
/// moves again.
pub fn changed(document: DocumentId) {
  if with_state(|state| state.blame.shown == Some(document)) {
    clear();
  }
}

pub fn closed(document: DocumentId) {
  with_state(|state| {
    state.blame.documents.remove(&document);
    state.blame.running.remove(&document);
  });
}

/// Hides the annotation and the status item.
fn clear() {
  if let Some(document) = with_state(|state| state.blame.shown.take())
    && let Ok(info) = host::document(document)
  {
    let _ = workbench::set_line_annotations(document, info.version, &[]);
  }
  with_state(|state| state.blame.hash = None);
  let _ = workbench::set_status_item("blame", None);
}

/// Shows the blame of the cursor's line, reading it first if it's out of
/// date.
fn update() {
  let line_on = enabled("blame_line");
  let status_on = enabled("blame_status");
  let Some((document, line)) = with_state(|state| state.blame.cursor) else {
    return clear();
  };
  if !line_on && !status_on {
    return clear();
  }
  let Ok(info) = host::document(document) else {
    return clear();
  };
  let Some(path) = info.path.clone() else {
    return clear();
  };
  if info.line_count > MAX_LINES {
    return clear();
  }
  let fresh = with_state(|state| {
    state
      .blame
      .documents
      .get(&document)
      .is_some_and(|blame| blame.version == info.version)
  });
  if !fresh {
    if with_state(|state| state.blame.shown != Some(document)) {
      clear();
    }
    return start(document, info.version, info.line_count, &path);
  }
  let found = with_state(|state| {
    let blame = state.blame.documents.get(&document)?;
    let commit = blame.commits.get((*blame.lines.get(line)?)?)?.clone();
    Some((commit, blame.now, blame.me.clone()))
  });
  let Some((commit, now, me)) = found else {
    return clear();
  };
  let you = me
    .as_deref()
    .is_some_and(|me| me.eq_ignore_ascii_case(&commit.mail));
  let author = if you {
    "You".to_owned()
  } else {
    commit.author.clone()
  };
  let (text, status, tooltip) = if commit.uncommitted() {
    (
      "You, Uncommitted changes".to_owned(),
      "You (Uncommitted changes)".to_owned(),
      "Uncommitted changes".to_owned(),
    )
  } else {
    let when = ago(commit.time, now);
    (
      format!("{}, {author} ({when})", commit.summary),
      format!("{author} ({when})"),
      format!(
        "{} <{}>\n{} ({when})\n\n{}\n\nCommit {}",
        commit.author,
        commit.mail,
        date(commit.time, &commit.zone),
        commit.summary,
        &commit.hash[..12]
      ),
    )
  };
  // The annotation moves to this document's line.
  let previous = with_state(|state| state.blame.shown.replace(document));
  if let Some(previous) = previous.filter(|previous| *previous != document)
    && let Ok(old) = host::document(previous)
  {
    let _ = workbench::set_line_annotations(previous, old.version, &[]);
  }
  let annotations = if line_on {
    vec![LineAnnotation {
      line: u32::try_from(line).unwrap_or(u32::MAX),
      text,
      tooltip: Some(tooltip.clone()),
    }]
  } else {
    Vec::new()
  };
  let _ = workbench::set_line_annotations(document, info.version, &annotations);
  let hash = (!commit.uncommitted()).then(|| commit.hash.clone());
  with_state(|state| state.blame.hash = hash.clone());
  let item = status_on.then(|| StatusItem {
    text: status,
    tooltip: Some(tooltip),
    command: hash.map(|_| "flareite.git.blameDetails".into()),
  });
  let _ = workbench::set_status_item("blame", item.as_ref());
}

/// Reads a document's blame in the background.
fn start(document: DocumentId, version: u64, line_count: u32, path: &str) {
  if with_state(|state| state.blame.running.get(&document) == Some(&version)) {
    return;
  }
  let Some(repo) = crate::repos::repo_of(path) else {
    return;
  };
  let Some(relative) = relative_path(&repo.root, path) else {
    return;
  };
  let Ok(slice) = host::read_lines(document, 0, line_count) else {
    return;
  };
  let text = slice.lines.join("\n");
  let started = spawn(
    Job::Blame {
      document,
      version: slice.version,
      root: repo.root.clone(),
    },
    &args(&[
      "--no-optional-locks",
      "blame",
      "--porcelain",
      "--contents",
      "-",
      "--",
      &relative,
    ]),
    &repo.root,
    Some(text.into_bytes()),
  );
  if started {
    with_state(|state| state.blame.running.insert(document, version));
  }
}

/// Git's time now, from the identity a new commit would get.
fn now(root: &str) -> i64 {
  run_git(&args(&["var", "GIT_COMMITTER_IDENT"]), &options(root))
    .ok()
    .and_then(|output| {
      let text = String::from_utf8_lossy(&output.stdout).into_owned();
      text.trim().rsplit(' ').nth(1)?.parse().ok()
    })
    .unwrap_or(0)
}

fn me(root: &str) -> Option<String> {
  let output = run_git(&args(&["config", "user.email"]), &options(root)).ok()?;
  let email = String::from_utf8_lossy(&output.stdout).trim().to_owned();
  (output.exit_code == Some(0) && !email.is_empty()).then_some(email)
}

pub fn finished(
  document: DocumentId,
  version: u64,
  root: &str,
  outcome: Result<ProcessOutput, ProcessError>,
) {
  with_state(|state| state.blame.running.remove(&document));
  // A file Git doesn't track has none: kept empty, so it isn't asked again.
  let (lines, commits) = match outcome {
    Ok(output) if output.exit_code == Some(0) => parse(&String::from_utf8_lossy(&output.stdout)),
    _ => (Vec::new(), Vec::new()),
  };
  let blame = Blame {
    version,
    lines,
    commits,
    now: now(root),
    me: me(root),
  };
  with_state(|state| state.blame.documents.insert(document, blame));
  let current = with_state(|state| state.blame.cursor.is_some_and(|(at, _)| at == document));
  if current {
    update();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn porcelain_blame_gives_each_line_its_commit() {
    let hash = "a".repeat(40);
    let text = format!(
      "{hash} 1 1 2\nauthor Ada\nauthor-mail <ada@example.com>\nauthor-time 1700000000\n\
author-tz -0500\nsummary First\nfilename f.txt\n\tone\n{hash} 2 2\n\ttwo\n\
{UNCOMMITTED} 3 3 1\nauthor Not Committed Yet\nauthor-mail <not.committed.yet>\n\
author-time 1700000100\nauthor-tz +0000\nsummary Version of f.txt from f.txt\n\
filename f.txt\n\tthree\n"
    );
    let (lines, commits) = parse(&text);
    assert_eq!(lines, [Some(0), Some(0), Some(1)]);
    assert_eq!(commits[0].author, "Ada");
    assert_eq!(commits[0].mail, "ada@example.com");
    assert_eq!(commits[0].time, 1_700_000_000);
    assert_eq!(commits[0].summary, "First");
    assert!(commits[1].uncommitted());
  }

  #[test]
  fn times_read_as_people_say_them() {
    assert_eq!(ago(100, 110), "now");
    assert_eq!(ago(0, 60), "1 minute ago");
    assert_eq!(ago(0, 3 * 3_600), "3 hours ago");
    assert_eq!(ago(0, 2 * 86_400), "2 days ago");
    assert_eq!(ago(0, 21 * 86_400), "3 weeks ago");
    assert_eq!(ago(0, 400 * 86_400), "1 year ago");
    // 2023-11-14 22:13:20 UTC, five hours behind.
    assert_eq!(date(1_700_000_000, "-0500"), "2023-11-14 17:13");
    assert_eq!(date(0, "+0000"), "1970-01-01 00:00");
  }
}
