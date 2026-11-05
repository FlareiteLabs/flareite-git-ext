//! A merge, rebase, cherry-pick, or revert in progress: Git stopped it for
//! conflicts (or, in a rebase, to edit a commit) and waits to continue.
//!
//! Git marks one with a pseudo-ref (`MERGE_HEAD`, `REBASE_HEAD`,
//! `CHERRY_PICK_HEAD`, `REVERT_HEAD`) in its folder, which the extension
//! can't read as files, so after every status it asks `git cat-file
//! --batch-check` which of them exist. `REBASE_HEAD` stays after a rebase
//! ends, so a rebase also needs its folder (`rebase-merge` or
//! `rebase-apply`), which `git -C <folder>` finds or fails on. While one does, the Changes view
//! says so, its title has Continue and Abort buttons, the branch in the
//! status bar says what's in progress, and a commit with no message takes
//! the one Git prepared.

use ferrite_extension_api::processes::{ProcessError, ProcessOutput};
use ferrite_extension_api::types::{LogLevel, StatusItem};
use ferrite_extension_api::workbench;

use crate::{
  CHANGES, Job, Pending, Then, args, ask_confirm, branch_text, branch_tooltip, operate, operation,
  repository, spawn, with_state,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
  Merge,
  Rebase,
  CherryPick,
  Revert,
}

/// The pseudo-refs, in the order they're asked about.
const HEADS: [(&str, Progress); 4] = [
  ("REBASE_HEAD", Progress::Rebase),
  ("MERGE_HEAD", Progress::Merge),
  ("CHERRY_PICK_HEAD", Progress::CherryPick),
  ("REVERT_HEAD", Progress::Revert),
];

impl Progress {
  /// The Git command that continues or aborts it.
  fn command(self) -> &'static str {
    match self {
      Self::Merge => "merge",
      Self::Rebase => "rebase",
      Self::CherryPick => "cherry-pick",
      Self::Revert => "revert",
    }
  }

  /// What's in progress, as the branch in the status bar says.
  pub fn label(self) -> &'static str {
    match self {
      Self::Merge => "Merging",
      Self::Rebase => "Rebasing",
      Self::CherryPick => "Cherry-Picking",
      Self::Revert => "Reverting",
    }
  }

  fn noun(self) -> &'static str {
    match self {
      Self::Merge => "merge",
      Self::Rebase => "rebase",
      Self::CherryPick => "cherry-pick",
      Self::Revert => "revert",
    }
  }

  /// The pseudo-ref naming the commit being brought in.
  pub fn head(self) -> &'static str {
    HEADS
      .iter()
      .find(|(_, progress)| *progress == self)
      .map_or("MERGE_HEAD", |(head, _)| head)
  }
}

/// What `git cat-file --batch-check` said of the pseudo-refs: those that
/// exist, in order.
pub fn parse(stdout: &[u8]) -> Vec<Progress> {
  let text = String::from_utf8_lossy(stdout);
  text
    .lines()
    .zip(HEADS)
    .filter(|(line, _)| !line.trim_end().ends_with(" missing"))
    .map(|(_, (_, progress))| progress)
    .collect()
}

/// Whether a rebase's folder is in the Git folder.
fn rebasing() -> bool {
  let Some((root, _)) = repository() else {
    return false;
  };
  let Some(git_dir) = with_state(|state| Some(state.repository.as_ref()?.git_dir.clone())) else {
    return false;
  };
  let separator = if git_dir.contains('\\') { '\\' } else { '/' };
  ["rebase-merge", "rebase-apply"].iter().any(|folder| {
    let path = format!("{git_dir}{separator}{folder}");
    crate::run_git(&args(&["-C", &path, "version"]), &crate::options(&root))
      .is_ok_and(|output| output.exit_code == Some(0))
  })
}

/// Asks Git which operation is in progress, in the background.
pub fn check() {
  let Some((root, _)) = repository() else {
    return;
  };
  let stdin: String = HEADS.iter().map(|(head, _)| format!("{head}\n")).collect();
  spawn(
    Job::Progress { root: root.clone() },
    &args(&["--no-optional-locks", "cat-file", "--batch-check"]),
    &root,
    Some(stdin.into_bytes()),
  );
}

pub fn finished(root: &str, outcome: Result<ProcessOutput, ProcessError>) {
  // Another repository became the active one meanwhile.
  if repository().is_none_or(|(active, _)| active != root) {
    return;
  }
  let found = match outcome {
    Ok(output) if output.exit_code == Some(0) => parse(&output.stdout),
    Ok(_) | Err(_) => return,
  };
  let progress = found
    .into_iter()
    .find(|progress| *progress != Progress::Rebase || rebasing());
  let changed = with_state(|state| std::mem::replace(&mut state.progress, progress) != progress);
  if changed {
    crate::log(
      LogLevel::Debug,
      &format!("in progress: {:?}", progress.map(Progress::label)),
    );
    crate::set_changes_flags();
  }
  show();
}

pub fn current() -> Option<Progress> {
  with_state(|state| state.progress)
}

/// The Changes view's message and the branch item, for what's in progress.
pub fn show() {
  let progress = current();
  let message = progress.map(|progress| match progress {
    Progress::Rebase => {
      "A rebase is in progress. Resolve any conflicts and stage the files, then Continue (or Abort)."
        .to_owned()
    }
    _ => format!(
      "A {} is in progress. Resolve the conflicts and stage the files, then Continue or commit (or Abort).",
      progress.noun()
    ),
  });
  let too_many = with_state(|state| state.too_many_changes);
  let message = match (message, too_many) {
    (Some(message), _) => Some(message),
    (None, true) => Some(format!(
      "Too many changes: showing the first {}.",
      crate::panel::MAX_FILES
    )),
    (None, false) => None,
  };
  let _ = workbench::set_view_message(CHANGES, message.as_deref());
  let Some(status) = with_state(|state| state.status.clone()) else {
    return;
  };
  let mut text = branch_text(&status);
  if let Some(progress) = progress {
    text = format!("{text} ({})", progress.label());
  }
  let _ = workbench::set_status_item(
    "branch",
    Some(&StatusItem {
      text,
      tooltip: Some(branch_tooltip(&status)),
      command: Some("ferrite.git.checkout".into()),
    }),
  );
}

/// The flag the Changes view's menus show Continue and Abort by.
pub fn flags() -> Vec<String> {
  match current() {
    Some(Progress::Rebase) => vec!["operation".into(), "rebasing".into()],
    Some(_) => vec!["operation".into()],
    None => Vec::new(),
  }
}

/// Continues what's in progress, taking the message Git prepared.
pub fn continue_operation() {
  let Some(progress) = current() else {
    return crate::info("Nothing is in progress to continue.");
  };
  let unresolved = with_state(|state| {
    state.status.as_ref().map_or(0, |status| {
      status
        .entries
        .iter()
        .filter(|entry| entry.kind == crate::status::Kind::Conflict)
        .count()
    })
  });
  if unresolved > 0 {
    return crate::info(&format!(
      "{unresolved} {} still {} merge conflicts: resolve them and stage the files first.",
      if unresolved == 1 { "file" } else { "files" },
      if unresolved == 1 { "has" } else { "have" },
    ));
  }
  // No editor opens: the prepared message is kept.
  let mut continue_args = args(&["-c", "core.editor=true", progress.command(), "--continue"]);
  if progress == Progress::Merge {
    // `git merge --continue` is a commit; this keeps it from asking.
    continue_args = args(&["commit", "--no-edit", "--quiet"]);
  }
  operate(operation(
    &format!("Continuing the {}", progress.noun()),
    continue_args,
    Then::ClearMessage,
  ));
}

/// Asks, then aborts what's in progress, putting things back as they were.
pub fn abort() {
  let Some(progress) = current() else {
    return crate::info("Nothing is in progress to abort.");
  };
  ask_confirm(
    &format!("Abort the {}?", progress.noun()),
    "Changes made to resolve its conflicts are lost.",
    &["Abort"],
    true,
    Pending::Abort(progress),
  );
}

pub fn do_abort(progress: Progress) {
  operate(operation(
    &format!("Aborting the {}", progress.noun()),
    args(&[progress.command(), "--abort"]),
    Then::Nothing,
  ));
}

/// Skips the commit a rebase stopped at.
pub fn skip() {
  if current() != Some(Progress::Rebase) {
    return crate::info("No rebase is in progress.");
  }
  operate(operation(
    "Skipping the commit",
    args(&["-c", "core.editor=true", "rebase", "--skip"]),
    Then::Nothing,
  ));
}

/// Commits the merge with the message Git prepared, when the box is empty.
pub fn commits_with_prepared_message() -> bool {
  matches!(
    current(),
    Some(Progress::Merge | Progress::CherryPick | Progress::Revert)
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_pseudo_refs_found_say_what_is_in_progress() {
    let missing =
      b"REBASE_HEAD missing\nMERGE_HEAD missing\nCHERRY_PICK_HEAD missing\nREVERT_HEAD missing\n";
    assert_eq!(parse(missing), []);
    let merging =
      b"REBASE_HEAD missing\n0123 commit 230\nCHERRY_PICK_HEAD missing\nREVERT_HEAD missing\n";
    assert_eq!(parse(merging), [Progress::Merge]);
    // REBASE_HEAD stays after a rebase: whether one is going on is asked
    // separately.
    let picking = b"9876 commit 1\nMERGE_HEAD missing\nabcd commit 200\nREVERT_HEAD missing\n";
    assert_eq!(parse(picking), [Progress::Rebase, Progress::CherryPick]);
    assert_eq!(Progress::Revert.head(), "REVERT_HEAD");
  }
}
