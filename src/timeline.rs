//! The File History view: for the file in the editor in use, its changes
//! not committed yet, then the commits that changed it (following renames),
//! newest first. Each opens what it changed in the file.

use flareite_extension_api::types::LogLevel;
use flareite_extension_api::views::{self, Expansion, ItemIcon, TreeItem, Welcome};
use flareite_extension_api::workbench;

use crate::history::parse_name_status;
use crate::{args, is_within, log, relative_path, repository, run_git, with_state};

pub const FILE_HISTORY: &str = "file-history";
/// Most commits listed.
const LIMIT: usize = 100;
/// `git log`'s format for [`parse`]; with `-z --name-status`.
pub const FORMAT: &str = "--format=%x1e%H%x1f%an%x1f%ar%x1f%s";

/// A commit that changed the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
  pub hash: String,
  pub author: String,
  pub relative: String,
  pub subject: String,
  /// The file's path then, relative to the repository's top folder.
  pub path: String,
}

/// Parses `git log --follow -z --name-status` with [`FORMAT`].
pub fn parse(output: &[u8]) -> Vec<Entry> {
  String::from_utf8_lossy(output)
    .split('\u{1e}')
    .filter_map(|record| {
      let (header, changes) = record.split_once('\0')?;
      let mut fields = header.split('\u{1f}');
      let hash = fields.next()?.to_owned();
      let author = fields.next()?.to_owned();
      let relative = fields.next()?.to_owned();
      let subject = fields.next().unwrap_or_default().to_owned();
      let path = parse_name_status(changes.trim_start_matches('\n').as_bytes())
        .pop()?
        .path;
      Some(Entry {
        hash,
        author,
        relative,
        subject,
        path,
      })
    })
    .collect()
}

/// The view's items: uncommitted changes, if any (`uncommitted`, the file's
/// path), then the commits.
pub fn items(uncommitted: Option<&str>, entries: &[Entry]) -> Vec<TreeItem> {
  let item = |id: String,
              label: String,
              description: String,
              icon: &str,
              command: &str,
              argument: String| TreeItem {
    id,
    parent: None,
    tooltip: Some(format!("{label}\n{description}")),
    label,
    description: Some(description),
    icon: ItemIcon::Named(icon.into()),
    mark: None,
    context: None,
    expansion: Expansion::None,
    command: Some(command.into()),
    argument: Some(argument),
  };
  uncommitted
    .map(|path| {
      item(
        "uncommitted".into(),
        "Uncommitted Changes".into(),
        "Not committed yet".into(),
        "edit",
        "flareite.git.openChange",
        path.into(),
      )
    })
    .into_iter()
    .chain(entries.iter().map(|entry| {
      item(
        entry.hash.clone(),
        entry.subject.clone(),
        format!("{}, {}", entry.author, entry.relative),
        "commit",
        "flareite.git.openCommitChange",
        format!("{}/{}", entry.hash, entry.path),
      )
    }))
    .collect()
}

fn show(items: &[TreeItem], welcome: Option<&str>) {
  if let Err(problem) = views::set_items(FILE_HISTORY, items) {
    log(
      LogLevel::Error,
      &format!("can't list the file's history: {problem:?}"),
    );
  }
  let welcome = welcome.map(|text| Welcome {
    text: text.into(),
    buttons: Vec::new(),
  });
  let _ = views::set_welcome(FILE_HISTORY, welcome.as_ref());
}

/// Lists the history of the file in the editor in use.
pub fn load() {
  let Some((root, _)) = repository() else {
    return show(&[], None);
  };
  let Some(file) = workbench::active_file() else {
    return show(&[], Some("Open a file to see its history."));
  };
  if !is_within(&file, &root) {
    return show(&[], Some("The file isn't in the repository."));
  }
  let Some(relative) = relative_path(&root, &file) else {
    return show(&[], None);
  };
  let count = format!("-n{LIMIT}");
  let listed = run_git(
    &args(&[
      "--no-optional-locks",
      "log",
      "--follow",
      "-z",
      FORMAT,
      "--name-status",
      &count,
      "--",
      &relative,
    ]),
    &crate::options(&root),
  );
  let entries = match listed {
    Ok(output) if output.exit_code == Some(0) => parse(&output.stdout),
    _ => Vec::new(),
  };
  let uncommitted = with_state(|state| {
    state.status.as_ref().is_some_and(|status| {
      status
        .entries
        .iter()
        .any(|entry| entry.path == relative && entry.worktree.is_some())
    })
  });
  if entries.is_empty() && !uncommitted {
    return show(
      &[],
      Some("The file has no history: Git doesn't track it yet."),
    );
  }
  show(
    &items(uncommitted.then_some(relative.as_str()), &entries),
    None,
  );
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn commits_are_read_with_the_path_they_had() {
    let entries = parse(
      b"\x1e1fcd5f\x1fT\x1f1 second ago\x1ftwo, renamed\0\nR050\0a.txt\0b.txt\0\
\x1e175ba3\x1fT\x1f1 second ago\x1fone\0\nA\0a.txt\0",
    );
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].subject, "two, renamed");
    assert_eq!(entries[0].path, "b.txt");
    assert_eq!(entries[1].path, "a.txt", "before the rename");

    let items = items(Some("b.txt"), &entries);
    assert_eq!(items[0].label, "Uncommitted Changes");
    assert_eq!(items[0].command.as_deref(), Some("flareite.git.openChange"));
    assert_eq!(items[2].argument.as_deref(), Some("175ba3/a.txt"));
    assert_eq!(items[2].description.as_deref(), Some("T, 1 second ago"));
  }
}
