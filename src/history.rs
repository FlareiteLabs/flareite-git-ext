//! Reading commits: `git log` for the Commits view, the files a commit
//! changed (`git diff-tree --name-status`), and a commit's details for its
//! tab.
//!
//! Fields are separated by the unit separator (`%x1f`) and records by the
//! record separator (`%x1e`), which commit messages and names don't use.

use std::collections::BTreeMap;

use crate::status::Kind;

/// `git log --format` for [`parse_log`].
pub const LOG_FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%ar%x1f%aD%x1f%D%x1f%s%x1e";
/// `git show -s --format` for [`parse_details`].
pub const DETAILS_FORMAT: &str =
  "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%aD%x1f%cn%x1f%ce%x1f%cD%x1f%B";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
  pub hash: String,
  pub parents: Vec<String>,
  pub author: String,
  pub email: String,
  /// How long ago, as Git says it ("3 days ago").
  pub relative: String,
  pub date: String,
  /// Branches and tags pointing at it ("HEAD -> main, origin/main").
  pub refs: String,
  pub subject: String,
}

impl Commit {
  /// The hover text in the Commits view.
  pub fn tooltip(&self) -> String {
    let mut text = format!(
      "{}\n\n{} <{}>\n{} ({})\n{}",
      self.subject, self.author, self.email, self.date, self.relative, self.hash
    );
    if !self.refs.is_empty() {
      text.push_str(&format!("\n{}", self.refs));
    }
    text
  }
}

fn fields(record: &str) -> Vec<&str> {
  record.split('\u{1f}').collect()
}

/// Parses `git log` output in [`LOG_FORMAT`].
pub fn parse_log(output: &[u8]) -> Vec<Commit> {
  String::from_utf8_lossy(output)
    .split('\u{1e}')
    .filter_map(|record| {
      let record = record.trim_start_matches(['\n', '\r']);
      let fields = fields(record);
      let [hash, parents, author, email, relative, date, refs, subject] = fields.as_slice() else {
        return None;
      };
      (!hash.is_empty()).then(|| Commit {
        hash: (*hash).to_owned(),
        parents: parents.split_whitespace().map(str::to_owned).collect(),
        author: (*author).to_owned(),
        email: (*email).to_owned(),
        relative: (*relative).to_owned(),
        date: (*date).to_owned(),
        refs: (*refs).to_owned(),
        subject: (*subject).to_owned(),
      })
    })
    .collect()
}

/// A file a commit changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
  /// Relative to the repository's top folder, with `/`.
  pub path: String,
  /// Where a renamed or copied file came from.
  pub original: Option<String>,
  pub kind: Kind,
}

/// Parses `git diff-tree -r --name-status -z`: a status, then the path (two
/// paths for a rename or copy).
pub fn parse_name_status(output: &[u8]) -> Vec<FileChange> {
  let text = String::from_utf8_lossy(output);
  let mut parts = text.split('\0').filter(|part| !part.is_empty());
  let mut changes = Vec::new();
  while let Some(status) = parts.next() {
    let kind = match status.as_bytes().first() {
      Some(b'A') => Kind::Added,
      Some(b'D') => Kind::Deleted,
      Some(b'R') => Kind::Renamed,
      Some(b'C') => Kind::Added,
      Some(b'U') => Kind::Conflict,
      Some(b'M' | b'T') => Kind::Modified,
      // Not a status: a commit hash line or something unexpected.
      _ => continue,
    };
    let two = matches!(status.as_bytes().first(), Some(b'R' | b'C'));
    let first = parts.next();
    let (original, path) = if two {
      (first.map(str::to_owned), parts.next())
    } else {
      (None, first)
    };
    if let Some(path) = path {
      changes.push(FileChange {
        path: path.to_owned(),
        original,
        kind,
      });
    }
  }
  changes
}

/// Lines added and deleted per file, from `git diff-tree -r --numstat -z`;
/// `None` for a binary file.
pub fn parse_numstat(output: &[u8]) -> BTreeMap<String, Option<(u64, u64)>> {
  let text = String::from_utf8_lossy(output);
  let mut parts = text.split('\0').filter(|part| !part.is_empty());
  let mut counts = BTreeMap::new();
  while let Some(part) = parts.next() {
    let mut columns = part.splitn(3, '\t');
    let (Some(added), Some(deleted), Some(path)) = (columns.next(), columns.next(), columns.next())
    else {
      continue;
    };
    // A rename: the path field is empty, and the two paths follow.
    let path = if path.is_empty() {
      parts.next();
      parts.next().unwrap_or_default()
    } else {
      path
    };
    let count = added.parse().ok().zip(deleted.parse().ok());
    counts.insert(path.to_owned(), count);
  }
  counts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Details {
  pub hash: String,
  pub parents: Vec<String>,
  pub author: String,
  pub author_email: String,
  pub author_date: String,
  pub committer: String,
  pub committer_email: String,
  pub commit_date: String,
  /// The whole message.
  pub message: String,
}

/// Parses `git show -s` output in [`DETAILS_FORMAT`].
pub fn parse_details(output: &[u8]) -> Option<Details> {
  let text = String::from_utf8_lossy(output);
  let fields: Vec<&str> = text.splitn(9, '\u{1f}').collect();
  let [
    hash,
    parents,
    author,
    author_email,
    author_date,
    committer,
    committer_email,
    commit_date,
    message,
  ] = fields.as_slice()
  else {
    return None;
  };
  Some(Details {
    hash: hash.trim().to_owned(),
    parents: parents.split_whitespace().map(str::to_owned).collect(),
    author: (*author).to_owned(),
    author_email: (*author_email).to_owned(),
    author_date: (*author_date).to_owned(),
    committer: (*committer).to_owned(),
    committer_email: (*committer_email).to_owned(),
    commit_date: (*commit_date).to_owned(),
    message: message.trim_end().to_owned(),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reads_commits() {
    let log = "aaaa1111bbbb\u{1f}cccc dddd\u{1f}Ada\u{1f}ada@example.com\u{1f}2 days ago\u{1f}Mon, 2 Nov 2026 10:00:00 +0000\u{1f}HEAD -> main\u{1f}Fix the thing\u{1e}\n\
eeee2222\u{1f}\u{1f}Bob\u{1f}bob@example.com\u{1f}3 weeks ago\u{1f}Tue, 13 Oct 2026 09:00:00 +0000\u{1f}\u{1f}First\u{1e}\n";
    let commits = parse_log(log.as_bytes());
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].parents, vec!["cccc", "dddd"]);
    assert_eq!(commits[1].subject, "First");
    assert!(commits[1].parents.is_empty());
    assert!(commits[0].tooltip().ends_with("aaaa1111bbbb\nHEAD -> main"));
  }

  #[test]
  fn reads_the_files_a_commit_changed() {
    let output = b"M\0src/main.rs\0A\0new file.txt\0D\0gone.rs\0R087\0old.md\0docs/new.md\0";
    let changes = parse_name_status(output);
    let summary: Vec<(&str, Kind, Option<&str>)> = changes
      .iter()
      .map(|change| {
        (
          change.path.as_str(),
          change.kind,
          change.original.as_deref(),
        )
      })
      .collect();
    assert_eq!(
      summary,
      vec![
        ("src/main.rs", Kind::Modified, None),
        ("new file.txt", Kind::Added, None),
        ("gone.rs", Kind::Deleted, None),
        ("docs/new.md", Kind::Renamed, Some("old.md")),
      ]
    );
  }

  #[test]
  fn reads_line_counts_and_details() {
    let numstat = b"3\t1\tsrc/main.rs\0-\t-\timage.png\x002\t0\t\0old.md\0docs/new.md\0";
    let counts = parse_numstat(numstat);
    assert_eq!(counts["src/main.rs"], Some((3, 1)));
    assert_eq!(counts["image.png"], None);
    assert_eq!(counts["docs/new.md"], Some((2, 0)));

    let details = parse_details(
      "abc\u{1f}p1\u{1f}Ada\u{1f}a@x\u{1f}Mon\u{1f}Bob\u{1f}b@x\u{1f}Tue\u{1f}Subject\n\nBody text\n".as_bytes(),
    )
    .expect("parses");
    assert_eq!(details.message, "Subject\n\nBody text");
    assert_eq!(details.committer, "Bob");
  }
}
