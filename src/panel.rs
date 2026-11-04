//! The Source Control views' items: the Changes view's groups (Merge
//! Changes, Staged Changes, Changes) of changed files, as a list or a tree
//! of folders, and the Commits view's commits.
//!
//! Item arguments are paths relative to the repository's top folder, with
//! `/`, as Git takes them; a commit's argument is its hash, and a file in a
//! commit's is `<hash>/<path>`. Clicking a changed file shows its changes;
//! an untracked or conflicted one opens.

use std::collections::BTreeSet;

use ferrite_extension_api::views::{Expansion, ItemIcon, ItemMark, TreeItem};

use crate::history::{Commit, FileChange};
use crate::status::{Entry, Kind, Status};

/// Most files the Changes view lists.
pub const MAX_FILES: usize = 5_000;

/// The Changes view's groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
  Merge,
  Staged,
  Changes,
}

impl Group {
  pub fn id(self) -> &'static str {
    match self {
      Self::Merge => "merge",
      Self::Staged => "staged",
      Self::Changes => "changes",
    }
  }

  fn label(self) -> &'static str {
    match self {
      Self::Merge => "Merge Changes",
      Self::Staged => "Staged Changes",
      Self::Changes => "Changes",
    }
  }

  /// The change an entry shows in this group, if it's in it.
  fn kind(self, entry: &Entry) -> Option<Kind> {
    match self {
      Self::Merge => (entry.kind == Kind::Conflict).then_some(Kind::Conflict),
      Self::Staged => entry.index,
      Self::Changes => entry.worktree,
    }
  }

  /// The command a click on a file in it runs.
  fn command(self, kind: Kind) -> &'static str {
    match (self, kind) {
      (Self::Staged, _) => "ferrite.git.openStagedChange",
      (Self::Changes, Kind::Untracked) | (Self::Merge, _) => "ferrite.git.open",
      (Self::Changes, _) => "ferrite.git.openChange",
    }
  }

  /// The context menus match a file in it by.
  fn context(self, kind: Kind) -> &'static str {
    match (self, kind) {
      (Self::Merge, _) => "merge",
      (Self::Staged, _) => "staged",
      (Self::Changes, Kind::Untracked) => "untracked",
      (Self::Changes, _) => "changed",
    }
  }
}

fn kind_name(kind: Kind) -> &'static str {
  match kind {
    Kind::Conflict => "Conflict",
    Kind::Deleted => "Deleted",
    Kind::Modified => "Modified",
    Kind::Added => "Added",
    Kind::Renamed => "Renamed",
    Kind::Untracked => "Untracked",
  }
}

/// A file's name and the folder it's in (empty at the top).
fn split(path: &str) -> (&str, &str) {
  match path.rsplit_once('/') {
    Some((folder, name)) => (name, folder),
    None => (path, ""),
  }
}

fn mark(kind: Kind) -> ItemMark {
  ItemMark {
    badge: Some(kind.badge().into()),
    color: Some(kind.color().into()),
    strikethrough: kind == Kind::Deleted,
    faded: false,
  }
}

struct FileItem<'a> {
  id: String,
  parent: String,
  path: &'a str,
  kind: Kind,
  context: &'a str,
  description: Option<String>,
  command: &'a str,
  argument: String,
}

fn file_item(file: FileItem) -> TreeItem {
  let FileItem {
    id,
    parent,
    path,
    kind,
    context,
    description,
    command,
    argument,
  } = file;
  let (name, _) = split(path);
  TreeItem {
    id,
    parent: Some(parent),
    label: name.into(),
    description,
    tooltip: Some(format!("{path} • {}", kind_name(kind))),
    icon: ItemIcon::File(name.into()),
    mark: Some(mark(kind)),
    context: Some(context.into()),
    expansion: Expansion::None,
    command: Some(command.into()),
    argument: Some(argument),
  }
}

/// The Changes view's items, and how many changed files there are.
pub fn changes(status: &Status, tree: bool) -> (Vec<TreeItem>, usize) {
  let mut items = Vec::new();
  let mut listed = 0;
  for group in [Group::Merge, Group::Staged, Group::Changes] {
    let mut files: Vec<(&Entry, Kind)> = status
      .entries
      .iter()
      .filter_map(|entry| group.kind(entry).map(|kind| (entry, kind)))
      .collect();
    if files.is_empty() {
      continue;
    }
    files.sort_by(|(a, _), (b, _)| a.path.cmp(&b.path));
    let group_id = format!("group:{}", group.id());
    items.push(TreeItem {
      id: group_id.clone(),
      parent: None,
      label: group.label().into(),
      description: Some(files.len().to_string()),
      tooltip: None,
      icon: ItemIcon::None,
      mark: None,
      context: Some(format!("{}-group", group.id())),
      expansion: Expansion::Expanded,
      command: None,
      argument: Some(group.id().into()),
    });
    let take = MAX_FILES.saturating_sub(listed).min(files.len());
    listed += take;
    let files = &files[..take];
    if tree {
      // A folder item for each folder that holds a change, under its own.
      let folders: BTreeSet<&str> = files
        .iter()
        .flat_map(|(entry, _)| {
          let (_, folder) = split(entry.path.trim_end_matches('/'));
          folder
            .match_indices('/')
            .map(move |(at, _)| &folder[..at])
            .chain((!folder.is_empty()).then_some(folder))
        })
        .collect();
      let folder_id = |folder: &str| format!("{}:folder:{folder}", group.id());
      let parent_of = |folder: &str| {
        let (_, up) = split(folder);
        if up.is_empty() {
          group_id.clone()
        } else {
          folder_id(up)
        }
      };
      for folder in &folders {
        let (name, _) = split(folder);
        items.push(TreeItem {
          id: folder_id(folder),
          parent: Some(parent_of(folder)),
          label: name.into(),
          description: None,
          tooltip: Some((*folder).into()),
          icon: ItemIcon::Folder(name.into()),
          mark: None,
          context: Some("folder".into()),
          expansion: Expansion::Expanded,
          command: None,
          argument: None,
        });
      }
      for (entry, kind) in files {
        let path = entry.path.trim_end_matches('/');
        items.push(file_item(FileItem {
          id: format!("{}:{path}", group.id()),
          parent: parent_of(path),
          path,
          kind: *kind,
          context: group.context(*kind),
          description: None,
          command: group.command(*kind),
          argument: path.into(),
        }));
      }
    } else {
      for (entry, kind) in files {
        let path = entry.path.trim_end_matches('/');
        let (_, folder) = split(path);
        items.push(file_item(FileItem {
          id: format!("{}:{path}", group.id()),
          parent: group_id.clone(),
          path,
          kind: *kind,
          context: group.context(*kind),
          description: (!folder.is_empty()).then(|| folder.to_owned()),
          command: group.command(*kind),
          argument: path.into(),
        }));
      }
    }
  }
  let changed: BTreeSet<&str> = status
    .entries
    .iter()
    .map(|entry| entry.path.as_str())
    .collect();
  (items, changed.len())
}

/// The Commits view's items: one per commit, opened to list its files, and
/// "Load More Commits" if there are more.
pub fn commits(commits: &[Commit], more: bool) -> Vec<TreeItem> {
  let mut items: Vec<TreeItem> = commits
    .iter()
    .map(|commit| TreeItem {
      id: commit.hash.clone(),
      parent: None,
      label: commit.subject.clone(),
      description: Some(format!("{}, {}", commit.author, commit.relative)),
      tooltip: Some(commit.tooltip()),
      icon: ItemIcon::Named("commit".into()),
      mark: None,
      context: Some("commit".into()),
      expansion: Expansion::Collapsed,
      command: None,
      argument: Some(commit.hash.clone()),
    })
    .collect();
  if more {
    items.push(TreeItem {
      id: "load-more".into(),
      parent: None,
      label: "Load More Commits".into(),
      description: None,
      tooltip: None,
      icon: ItemIcon::Named("chevron-down".into()),
      mark: None,
      context: None,
      expansion: Expansion::None,
      command: Some("ferrite.git.loadMore".into()),
      argument: None,
    });
  }
  items
}

/// The files a commit changed, as children of its item.
pub fn commit_files(hash: &str, changes: &[FileChange]) -> Vec<TreeItem> {
  changes
    .iter()
    .map(|change| {
      let (_, folder) = split(&change.path);
      let id = format!("{hash}/{}", change.path);
      let mut item = file_item(FileItem {
        id: id.clone(),
        parent: hash.to_owned(),
        path: &change.path,
        kind: change.kind,
        context: "commit-file",
        description: (!folder.is_empty()).then(|| folder.to_owned()),
        command: "ferrite.git.openCommitChange",
        argument: id,
      });
      if let Some(original) = &change.original {
        item.tooltip = Some(format!("{} • Renamed from {original}", change.path));
      }
      item
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::status::parse;

  const STATUS: &[u8] = b"1 MM N... 100644 100644 100644 a b src/app/main.rs\0\
1 A. N... 000000 100644 100644 0 b src/new.rs\0\
1 .D N... 100644 100644 000000 a a gone.rs\0\
u UU N... 100644 100644 100644 100644 a b c both.rs\0\
? notes.txt\0";

  fn summary(items: &[TreeItem]) -> Vec<String> {
    items
      .iter()
      .map(|item| {
        format!(
          "{} <{}> {}",
          item.id,
          item.parent.as_deref().unwrap_or("-"),
          item.context.as_deref().unwrap_or("-")
        )
      })
      .collect()
  }

  #[test]
  fn groups_changes_as_a_list() {
    let (items, count) = changes(&parse(STATUS), false);
    assert_eq!(count, 5);
    assert_eq!(
      summary(&items),
      vec![
        "group:merge <-> merge-group",
        "merge:both.rs <group:merge> merge",
        "group:staged <-> staged-group",
        "staged:src/app/main.rs <group:staged> staged",
        "staged:src/new.rs <group:staged> staged",
        "group:changes <-> changes-group",
        "changes:gone.rs <group:changes> changed",
        "changes:notes.txt <group:changes> untracked",
        "changes:src/app/main.rs <group:changes> changed",
      ]
    );
    let gone = items
      .iter()
      .find(|item| item.id == "changes:gone.rs")
      .expect("listed");
    assert!(gone.mark.as_ref().is_some_and(|mark| mark.strikethrough));
    assert_eq!(gone.argument.as_deref(), Some("gone.rs"));
    assert_eq!(gone.command.as_deref(), Some("ferrite.git.openChange"));
    let notes = items
      .iter()
      .find(|item| item.id == "changes:notes.txt")
      .expect("listed");
    assert_eq!(notes.command.as_deref(), Some("ferrite.git.open"));
    let main = items
      .iter()
      .find(|item| item.id == "staged:src/app/main.rs")
      .expect("listed");
    assert_eq!(main.description.as_deref(), Some("src/app"));
    assert_eq!(main.label, "main.rs");
  }

  #[test]
  fn groups_changes_as_a_tree_of_folders() {
    let (items, _) = changes(&parse(STATUS), true);
    let staged: Vec<String> = summary(&items)
      .into_iter()
      .filter(|line| line.starts_with("staged"))
      .collect();
    assert_eq!(
      staged,
      vec![
        "staged:folder:src <group:staged> folder",
        "staged:folder:src/app <staged:folder:src> folder",
        "staged:src/app/main.rs <staged:folder:src/app> staged",
        "staged:src/new.rs <staged:folder:src> staged",
      ]
    );
  }

  #[test]
  fn lists_commits_and_their_files() {
    let commit = Commit {
      hash: "abc".into(),
      parents: Vec::new(),
      author: "Ada".into(),
      email: "a@x".into(),
      relative: "2 days ago".into(),
      date: "Mon".into(),
      refs: String::new(),
      subject: "Fix it".into(),
    };
    let items = commits(std::slice::from_ref(&commit), true);
    assert_eq!(items[0].description.as_deref(), Some("Ada, 2 days ago"));
    assert_eq!(items[0].expansion, Expansion::Collapsed);
    assert_eq!(items[1].command.as_deref(), Some("ferrite.git.loadMore"));
    let files = commit_files(
      "abc",
      &[FileChange {
        path: "a/b.rs".into(),
        original: None,
        kind: Kind::Added,
      }],
    );
    assert_eq!(files[0].id, "abc/a/b.rs");
    assert_eq!(files[0].parent.as_deref(), Some("abc"));
    assert_eq!(files[0].argument.as_deref(), Some("abc/a/b.rs"));
    assert_eq!(
      files[0].command.as_deref(),
      Some("ferrite.git.openCommitChange")
    );
  }
}
