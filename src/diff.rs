//! Comparing a file's versions in diff tabs: what the Source Control views
//! open for each kind of change, and the `git:` addresses of the versions
//! that aren't files on disk, whose text the extension provides.
//!
//! `git:<revision>:<path>` is the file at `path` (relative to the
//! repository's top folder, with `/`) in `revision`: a commit (`HEAD`, a
//! hash, or `<hash>^` for its first parent), or nothing for the index, as in
//! `git show :<path>`. `git:~empty:<path>` is an empty text, for the side
//! where a file doesn't exist; Git doesn't allow `~` in names, so it's no
//! revision's.

use crate::history::FileChange;
use crate::status::{Entry, Kind};

/// The revision of an empty text.
pub const EMPTY: &str = "~empty";

/// The address of the file at `path` in `revision`.
pub fn address(revision: &str, path: &str) -> String {
  format!("git:{revision}:{path}")
}

/// The revision and path of a `git:` address.
pub fn parse(uri: &str) -> Option<(&str, &str)> {
  uri.strip_prefix("git:")?.split_once(':')
}

/// What a diff tab compares, and its title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sides {
  pub original: String,
  pub modified: String,
  pub title: String,
}

fn name(path: &str) -> &str {
  path.rsplit('/').next().unwrap_or(path)
}

/// The changes not staged: the index against the file on disk
/// (`working`, its absolute path). `None` for an untracked file, which has
/// nothing to compare with, or one without such changes.
pub fn working(entry: &Entry, working: String) -> Option<Sides> {
  let path = &entry.path;
  let index = address("", path);
  Some(match entry.worktree? {
    Kind::Untracked => return None,
    Kind::Deleted => Sides {
      original: index,
      modified: address(EMPTY, path),
      title: format!("{} (Deleted)", name(path)),
    },
    _ => Sides {
      original: index,
      modified: working,
      title: format!("{} (Working Tree)", name(path)),
    },
  })
}

/// The staged changes: `HEAD` against the index. `None` without any.
pub fn staged(entry: &Entry) -> Option<Sides> {
  let path = &entry.path;
  let head = address("HEAD", entry.original.as_deref().unwrap_or(path));
  let (original, modified) = match entry.index? {
    Kind::Added => (address(EMPTY, path), address("", path)),
    Kind::Deleted => (head, address(EMPTY, path)),
    _ => (head, address("", path)),
  };
  Some(Sides {
    original,
    modified,
    title: format!(
      "{} (Index{})",
      name(path),
      if entry.index == Some(Kind::Deleted) {
        ", Deleted"
      } else {
        ""
      }
    ),
  })
}

/// A file a commit changed: its first parent against the commit.
pub fn commit(hash: &str, change: &FileChange) -> Sides {
  let path = &change.path;
  let before = change.original.as_deref().unwrap_or(path);
  let original = match change.kind {
    Kind::Added => address(EMPTY, path),
    // A root commit has no parent; that version is empty too.
    _ => address(&format!("{hash}^"), before),
  };
  let modified = match change.kind {
    Kind::Deleted => address(EMPTY, path),
    _ => address(hash, path),
  };
  Sides {
    original,
    modified,
    title: format!("{} ({})", name(path), &hash[..hash.len().min(7)]),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::status::parse as parse_status;

  #[test]
  fn addresses_name_a_revision_and_a_path() {
    assert_eq!(address("HEAD", "src/a:b.rs"), "git:HEAD:src/a:b.rs");
    assert_eq!(parse("git:HEAD:src/a:b.rs"), Some(("HEAD", "src/a:b.rs")));
    assert_eq!(parse("git::src/a.rs"), Some(("", "src/a.rs")));
    assert_eq!(parse("file:/a"), None);
  }

  #[test]
  fn changes_compare_the_versions_they_are_between() {
    let status = parse_status(
      b"1 MM N... 100644 100644 100644 a b src/main.rs\0\
1 A. N... 000000 100644 100644 0 b new.rs\0\
1 .D N... 100644 100644 000000 a a gone.rs\0\
2 R. N... 100644 100644 100644 a a R100 moved.rs\0old.rs\0\
? notes.txt\0",
    );
    let entry = |path: &str| {
      status
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .expect("listed")
    };

    let main = working(entry("src/main.rs"), "/repo/src/main.rs".into()).expect("changed");
    assert_eq!(
      (main.original.as_str(), main.modified.as_str()),
      ("git::src/main.rs", "/repo/src/main.rs")
    );
    assert_eq!(main.title, "main.rs (Working Tree)");
    let main = staged(entry("src/main.rs")).expect("staged");
    assert_eq!(
      (main.original.as_str(), main.modified.as_str()),
      ("git:HEAD:src/main.rs", "git::src/main.rs")
    );

    let new = staged(entry("new.rs")).expect("staged");
    assert_eq!(new.original, "git:~empty:new.rs");
    assert!(working(entry("new.rs"), String::new()).is_none());

    let gone = working(entry("gone.rs"), String::new()).expect("deleted");
    assert_eq!(
      (gone.original.as_str(), gone.modified.as_str()),
      ("git::gone.rs", "git:~empty:gone.rs")
    );

    let moved = staged(entry("moved.rs")).expect("renamed");
    assert_eq!(moved.original, "git:HEAD:old.rs");
    assert_eq!(moved.modified, "git::moved.rs");

    assert!(working(entry("notes.txt"), String::new()).is_none());
  }

  #[test]
  fn commit_files_compare_with_the_first_parent() {
    let change = |kind, original: Option<&str>| FileChange {
      path: "src/b.rs".into(),
      original: original.map(Into::into),
      kind,
    };
    let modified = commit("0123456789", &change(Kind::Modified, None));
    assert_eq!(modified.original, "git:0123456789^:src/b.rs");
    assert_eq!(modified.modified, "git:0123456789:src/b.rs");
    assert_eq!(modified.title, "b.rs (0123456)");
    assert_eq!(
      commit("abc", &change(Kind::Added, None)).original,
      "git:~empty:src/b.rs"
    );
    assert_eq!(
      commit("abc", &change(Kind::Deleted, None)).modified,
      "git:~empty:src/b.rs"
    );
    assert_eq!(
      commit("abc", &change(Kind::Renamed, Some("a.rs"))).original,
      "git:abc^:a.rs"
    );
  }
}
