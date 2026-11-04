//! Reading `git status --porcelain=v2 -z --branch`: the branch, and each
//! changed path's state, and turning states into explorer marks.
//!
//! The format is documented in `git help status` ("Porcelain Format
//! Version 2"). With `-z`, every record ends with NUL, paths are relative to
//! the repository's top folder with `/` separators and never quoted, and a
//! renamed or copied entry is followed by its original path as one more
//! record.

/// The branch headers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Branch {
  /// The commit checked out; `None` before the first commit.
  pub oid: Option<String>,
  /// The branch's name; `None` when HEAD is detached.
  pub head: Option<String>,
  pub upstream: Option<String>,
  pub ahead: u32,
  pub behind: u32,
}

/// What changed about a path, as the explorer shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
  /// Both sides of a merge changed it, or another conflict.
  Conflict,
  Deleted,
  Modified,
  /// New in the index (staged).
  Added,
  /// Renamed in the index.
  Renamed,
  /// Not tracked by Git.
  Untracked,
}

impl Kind {
  /// The name of the manifest's color for it.
  pub fn color(self) -> &'static str {
    match self {
      Self::Conflict => "conflict",
      Self::Deleted => "deleted",
      Self::Modified => "modified",
      Self::Added => "added",
      Self::Renamed => "renamed",
      Self::Untracked => "untracked",
    }
  }

  pub fn badge(self) -> &'static str {
    match self {
      Self::Conflict => "!",
      Self::Deleted => "D",
      Self::Modified => "M",
      Self::Added => "A",
      Self::Renamed => "R",
      Self::Untracked => "U",
    }
  }

  /// Folders take the color of the most important change inside them.
  pub fn priority(self) -> u8 {
    match self {
      Self::Conflict => 6,
      Self::Deleted => 5,
      Self::Modified => 4,
      Self::Added | Self::Renamed => 3,
      Self::Untracked => 2,
    }
  }
}

/// A changed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
  /// Relative to the repository's top folder, with `/` separators. An
  /// untracked folder ends with `/`.
  pub path: String,
  pub kind: Kind,
  /// The change is staged (in the index) and the file has no other
  /// changes.
  pub staged: bool,
  /// For a rename, where it was.
  pub original: Option<String>,
  /// The staged change, if any: what the index has against `HEAD`.
  pub index: Option<Kind>,
  /// The change not staged, if any: what the working tree has against the
  /// index (`Untracked` for a file Git doesn't track).
  pub worktree: Option<Kind>,
}

impl Entry {
  /// The tooltip the explorer shows.
  pub fn tooltip(&self) -> String {
    let what = match self.kind {
      Kind::Conflict => "Conflict",
      Kind::Deleted => "Deleted",
      Kind::Modified => "Modified",
      Kind::Added => "Added",
      Kind::Renamed => "Renamed",
      Kind::Untracked => "Untracked",
    };
    match (
      self
        .original
        .as_ref()
        .filter(|_| self.kind == Kind::Renamed),
      self.staged,
    ) {
      (Some(original), _) => format!("{what} from {original}"),
      (None, true) => format!("{what} (staged)"),
      (None, false) => what.to_owned(),
    }
  }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Status {
  pub branch: Branch,
  pub entries: Vec<Entry>,
}

impl Status {
  /// Whether anything is changed.
  pub fn dirty(&self) -> bool {
    !self.entries.is_empty()
  }

  /// The changes the explorer marks: those to files on disk. A deleted
  /// file isn't in the explorer, so it isn't marked, and doesn't color the
  /// folders it was in either (as in VS Code); the Source Control view
  /// lists it.
  pub fn on_disk(&self) -> impl Iterator<Item = &Entry> {
    self
      .entries
      .iter()
      .filter(|entry| entry.kind != Kind::Deleted)
  }
}

/// The kind of an ordinary or renamed entry from its `XY` field: `X` is
/// the index (staged) side, `Y` the working tree's.
fn kind_of(xy: &[u8]) -> Option<(Kind, bool)> {
  let (&x, &y) = (xy.first()?, xy.get(1)?);
  let staged = y == b'.';
  let kind = if x == b'D' || y == b'D' {
    Kind::Deleted
  } else if x == b'A' {
    Kind::Added
  } else if x == b'R' || x == b'C' {
    if matches!(y, b'M' | b'T') {
      Kind::Modified
    } else {
      Kind::Renamed
    }
  } else if matches!(x, b'M' | b'T') || matches!(y, b'M' | b'T') {
    Kind::Modified
  } else {
    return None;
  };
  Some((kind, staged))
}

/// The staged and unstaged sides of an `XY` field.
fn sides(xy: &[u8]) -> (Option<Kind>, Option<Kind>) {
  let side = |c: Option<&u8>| match c {
    Some(b'M' | b'T') => Some(Kind::Modified),
    Some(b'A') => Some(Kind::Added),
    Some(b'D') => Some(Kind::Deleted),
    Some(b'R') => Some(Kind::Renamed),
    Some(b'C') => Some(Kind::Added),
    _ => None,
  };
  (side(xy.first()), side(xy.get(1)))
}

/// The `n`th space-separated field of `record`, and what follows it: paths
/// are the last field, and may contain spaces.
fn after_fields(record: &str, n: usize) -> Option<(Vec<&str>, &str)> {
  let mut fields = Vec::with_capacity(n);
  let mut rest = record;
  for _ in 0..n {
    let (field, after) = rest.split_once(' ')?;
    fields.push(field);
    rest = after;
  }
  Some((fields, rest))
}

/// Parses the output of `git status --porcelain=v2 -z --branch`. Records it
/// doesn't understand are skipped.
pub fn parse(output: &[u8]) -> Status {
  let text = String::from_utf8_lossy(output);
  let mut records = text.split('\0').filter(|record| !record.is_empty());
  let mut status = Status::default();
  while let Some(record) = records.next() {
    let Some((tag, rest)) = record.split_once(' ') else {
      continue;
    };
    match tag {
      "#" => branch_header(&mut status.branch, rest),
      // 1 XY sub mH mI mW hH hI path
      "1" => {
        if let Some((fields, path)) = after_fields(rest, 7)
          && let Some((kind, staged)) = kind_of(fields[0].as_bytes())
        {
          let (index, worktree) = sides(fields[0].as_bytes());
          status.entries.push(Entry {
            path: path.to_owned(),
            kind,
            staged,
            original: None,
            index,
            worktree,
          });
        }
      }
      // 2 XY sub mH mI mW hH hI Xscore path, then the original path.
      "2" => {
        let original = records.next().map(str::to_owned);
        if let Some((fields, path)) = after_fields(rest, 8)
          && let Some((kind, staged)) = kind_of(fields[0].as_bytes())
        {
          let (index, worktree) = sides(fields[0].as_bytes());
          status.entries.push(Entry {
            path: path.to_owned(),
            kind,
            staged,
            original: original.filter(|_| index == Some(Kind::Renamed)),
            index,
            worktree,
          });
        }
      }
      // u XY sub m1 m2 m3 mW h1 h2 h3 path
      "u" => {
        if let Some((_, path)) = after_fields(rest, 9) {
          status.entries.push(Entry {
            path: path.to_owned(),
            kind: Kind::Conflict,
            staged: false,
            original: None,
            index: None,
            worktree: None,
          });
        }
      }
      "?" => status.entries.push(Entry {
        path: rest.to_owned(),
        kind: Kind::Untracked,
        staged: false,
        original: None,
        index: None,
        worktree: Some(Kind::Untracked),
      }),
      _ => {}
    }
  }
  status
}

fn branch_header(branch: &mut Branch, header: &str) {
  let Some((key, value)) = header.split_once(' ') else {
    return;
  };
  match key {
    "branch.oid" => branch.oid = (value != "(initial)").then(|| value.to_owned()),
    "branch.head" => branch.head = (value != "(detached)").then(|| value.to_owned()),
    "branch.upstream" => branch.upstream = Some(value.to_owned()),
    "branch.ab" => {
      for count in value.split(' ') {
        if let Some(ahead) = count.strip_prefix('+') {
          branch.ahead = ahead.parse().unwrap_or(0);
        } else if let Some(behind) = count.strip_prefix('-') {
          branch.behind = behind.parse().unwrap_or(0);
        }
      }
    }
    _ => {}
  }
}

/// The status bar's text for the branch: its name (or the commit, when
/// detached), and `*` with changes.
pub fn branch_text(status: &Status) -> String {
  let branch = &status.branch;
  let name = match (&branch.head, &branch.oid) {
    (Some(head), _) => head.clone(),
    (None, Some(oid)) => oid.chars().take(8).collect(),
    (None, None) => "(no commits)".to_owned(),
  };
  // How far it's ahead and behind shows on the sync button.
  let mut text = name;
  if status.dirty() {
    text.push('*');
  }
  text
}

/// The status bar's tooltip for the branch.
pub fn branch_tooltip(status: &Status) -> String {
  let branch = &status.branch;
  let mut lines = vec![match (&branch.head, &branch.oid) {
    (Some(head), _) => format!("Git: on branch {head}"),
    (None, Some(oid)) => format!(
      "Git: HEAD detached at {}",
      oid.chars().take(8).collect::<String>()
    ),
    (None, None) => "Git: no commits yet".to_owned(),
  }];
  if let Some(upstream) = &branch.upstream {
    lines.push(match (branch.ahead, branch.behind) {
      (0, 0) => format!("Up to date with {upstream}"),
      (ahead, 0) => format!("{ahead} ahead of {upstream}"),
      (0, behind) => format!("{behind} behind {upstream}"),
      (ahead, behind) => format!("{ahead} ahead of and {behind} behind {upstream}"),
    });
  }
  let changes = status.entries.len();
  lines.push(match changes {
    0 => "No changes".to_owned(),
    1 => "1 changed file".to_owned(),
    n => format!("{n} changed files"),
  });
  lines.join("\n")
}

#[cfg(test)]
mod tests {
  use super::*;

  const OUTPUT: &[u8] = b"# branch.oid 0123456789abcdef0123456789abcdef01234567\0\
# branch.head main\0\
# branch.upstream origin/main\0\
# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaaa bbbb src/main.rs\0\
1 M. N... 100644 100644 100644 aaaa bbbb README.md\0\
1 A. N... 000000 100644 100644 0000 bbbb new file.txt\0\
1 D. N... 100644 000000 000000 aaaa 0000 gone.rs\0\
2 R. N... 100644 100644 100644 aaaa aaaa R100 docs/new.md\0docs/old.md\0\
u UU N... 100644 100644 100644 100644 aaaa bbbb cccc both.rs\0\
? scratch/\0\
? notes.txt\0";

  #[test]
  fn reads_the_branch_and_every_kind_of_change() {
    let status = parse(OUTPUT);
    assert_eq!(
      status.branch,
      Branch {
        oid: Some("0123456789abcdef0123456789abcdef01234567".into()),
        head: Some("main".into()),
        upstream: Some("origin/main".into()),
        ahead: 2,
        behind: 1,
      }
    );
    let kinds: Vec<(&str, Kind, bool)> = status
      .entries
      .iter()
      .map(|entry| (entry.path.as_str(), entry.kind, entry.staged))
      .collect();
    assert_eq!(
      kinds,
      vec![
        ("src/main.rs", Kind::Modified, false),
        ("README.md", Kind::Modified, true),
        ("new file.txt", Kind::Added, true),
        ("gone.rs", Kind::Deleted, true),
        ("docs/new.md", Kind::Renamed, true),
        ("both.rs", Kind::Conflict, false),
        ("scratch/", Kind::Untracked, false),
        ("notes.txt", Kind::Untracked, false),
      ]
    );
    assert_eq!(status.entries[4].tooltip(), "Renamed from docs/old.md");
    assert_eq!(status.entries[1].tooltip(), "Modified (staged)");
    assert_eq!(status.entries[0].tooltip(), "Modified");
  }

  #[test]
  fn the_explorer_marks_only_files_on_disk() {
    let status = parse(OUTPUT);
    let marked: Vec<&str> = status.on_disk().map(|entry| entry.path.as_str()).collect();
    assert!(
      !marked.contains(&"gone.rs"),
      "deleted files aren't in the explorer"
    );
    assert_eq!(marked.len(), status.entries.len() - 1);
    // Still a change: the branch says so.
    assert!(parse(b"1 .D N... 100644 100644 000000 a a d.rs\0").dirty());
  }

  #[test]
  fn describes_the_branch_for_the_status_bar() {
    let status = parse(OUTPUT);
    assert_eq!(branch_text(&status), "main*");
    assert_eq!(
      branch_tooltip(&status),
      "Git: on branch main\n2 ahead of and 1 behind origin/main\n8 changed files"
    );

    let detached = parse(b"# branch.oid 89abcdef0123\0# branch.head (detached)\0");
    assert_eq!(branch_text(&detached), "89abcdef");
    assert_eq!(
      branch_tooltip(&detached),
      "Git: HEAD detached at 89abcdef\nNo changes"
    );

    let fresh = parse(b"# branch.oid (initial)\0# branch.head main\0? a\0");
    assert_eq!(branch_text(&fresh), "main*");
  }

  #[test]
  fn modified_renames_and_added_changes_read_sensibly() {
    let status = parse(
      b"2 RM N... 100644 100644 100644 a a R90 b.rs\0a.rs\0\
1 AM N... 000000 100644 100644 0 b c.rs\0\
1 .T N... 100644 100644 120000 a a link\0\
1 .D N... 100644 100644 000000 a a d.rs\0",
    );
    let kinds: Vec<Kind> = status.entries.iter().map(|entry| entry.kind).collect();
    assert_eq!(
      kinds,
      vec![Kind::Modified, Kind::Added, Kind::Modified, Kind::Deleted]
    );
    assert_eq!(status.entries[0].original.as_deref(), Some("a.rs"));
    assert_eq!(status.entries[0].tooltip(), "Modified");
    let sides: Vec<_> = status
      .entries
      .iter()
      .map(|entry| (entry.index, entry.worktree))
      .collect();
    assert_eq!(
      sides,
      vec![
        (Some(Kind::Renamed), Some(Kind::Modified)),
        (Some(Kind::Added), Some(Kind::Modified)),
        (None, Some(Kind::Modified)),
        (None, Some(Kind::Deleted)),
      ]
    );
  }

  #[test]
  fn skips_what_it_does_not_understand() {
    let status = parse(b"! ignored.log\0x what\0# branch.head\0\0");
    assert_eq!(status, Status::default());
  }
}
