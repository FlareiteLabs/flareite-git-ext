//! Branches and remotes: the branch picker (check out, create), renaming,
//! deleting, and merging branches, publishing them, fetching, pulling,
//! pushing, and syncing, the status bar's sync button, and fetching in the
//! background when the user turns it on.
//!
//! Commands that reach a remote run in the background without a time limit.
//! Git asks for credentials through Ferrite (`askpass` in the manifest), as
//! does SSH for a key's passphrase.

use ferrite_extension_api::prompts::{self, AnswerValue, InputOptions, PickItem};
use ferrite_extension_api::storage::{self, StorageScope};
use ferrite_extension_api::types::{ConfigValue, LogLevel, StatusItem};
use ferrite_extension_api::{host, timers, workbench};

use crate::status::{Branch, Status};
use crate::{
  ANSWERED, Job, Operation, Pending, Then, args, ask_confirm, error, info, log, operate, operation,
  refresh, repository, run_git, setting, spawn, with_state,
};

/// Run by the timer that fetches in the background.
pub const AUTOFETCH: &str = "ferrite.git.autofetch";
/// Remembers that fetching in the background was offered.
const AUTOFETCH_OFFERED: &str = "autofetchOffered";
/// Most refs the branch picker lists (the prompt takes 1,000 items).
const MAX_REFS: usize = 990;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
  Local,
  Remote,
  Tag,
}

/// A branch or tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
  pub kind: RefKind,
  /// Such as `refs/heads/main`.
  pub full: String,
  /// Such as `main`, `origin/main`, or `v1.0`.
  pub name: String,
  pub commit: String,
  pub subject: String,
}

/// `git for-each-ref`'s format for [`parse_refs`].
pub const REFS_FORMAT: &str =
  "--format=%(refname)%00%(refname:short)%00%(objectname:short)%00%(subject)";

/// Parses `git for-each-ref` with [`REFS_FORMAT`]. Remotes' `HEAD`s are
/// left out.
pub fn parse_refs(output: &[u8]) -> Vec<Ref> {
  String::from_utf8_lossy(output)
    .lines()
    .filter_map(|line| {
      let mut parts = line.split('\0');
      let (full, name, commit) = (parts.next()?, parts.next()?, parts.next()?);
      let kind = if full.starts_with("refs/heads/") {
        RefKind::Local
      } else if full.starts_with("refs/remotes/") && !full.ends_with("/HEAD") {
        RefKind::Remote
      } else if full.starts_with("refs/tags/") {
        RefKind::Tag
      } else {
        return None;
      };
      Some(Ref {
        kind,
        full: full.to_owned(),
        name: name.to_owned(),
        commit: commit.to_owned(),
        subject: parts.next().unwrap_or_default().to_owned(),
      })
    })
    .collect()
}

/// A ref's short name: `main`, `origin/main`, `v1.0`.
fn short(full: &str) -> &str {
  ["refs/heads/", "refs/remotes/", "refs/tags/"]
    .iter()
    .find_map(|prefix| full.strip_prefix(prefix))
    .unwrap_or(full)
}

/// What a question asked here is for.
pub enum Ask {
  /// The branch picker.
  Checkout,
  /// A new branch's name, and the ref it starts at.
  NewBranch {
    from: Option<String>,
  },
  /// The ref a new branch starts at.
  NewBranchFrom,
  /// The ref to check out without a branch.
  Detached,
  /// The branch's new name.
  Rename,
  /// The branch to delete.
  Delete,
  /// Whether to delete a branch that isn't merged.
  ForceDelete(String),
  /// The branch to merge into this one.
  Merge,
  /// The remote to publish a branch to.
  PublishTo(String),
  /// A new remote's name, then its address.
  RemoteName,
  RemoteUrl(String),
  /// The remote to remove.
  RemoveRemote,
  /// Whether to sync.
  Sync,
  /// Whether to fetch in the background.
  Autofetch,
}

fn ask_pick(title: &str, items: Vec<PickItem>, ask: Ask) {
  match prompts::pick(title, &items, ANSWERED) {
    Ok(id) => with_state(|state| {
      state.prompts.insert(id, Pending::Remote(ask));
    }),
    Err(problem) => error(&format!("Couldn't ask: {problem:?}")),
  }
}

fn ask_input(title: &str, value: &str, placeholder: Option<String>, ask: Ask) {
  let input = InputOptions {
    title: title.into(),
    value: value.into(),
    placeholder,
  };
  match prompts::input(&input, ANSWERED) {
    Ok(id) => with_state(|state| {
      state.prompts.insert(id, Pending::Remote(ask));
    }),
    Err(problem) => error(&format!("Couldn't ask: {problem:?}")),
  }
}

fn item(label: &str, description: Option<String>, value: &str) -> PickItem {
  PickItem {
    label: label.into(),
    description,
    value: value.into(),
  }
}

fn ref_item(entry: &Ref) -> PickItem {
  let description = match entry.kind {
    RefKind::Local => format!("{} • {}", entry.commit, entry.subject),
    RefKind::Remote => format!("Remote branch at {}", entry.commit),
    RefKind::Tag => format!("Tag at {}", entry.commit),
  };
  item(&entry.name, Some(description), &entry.full)
}

/// The repository's branches and tags, most recently committed first.
fn refs(root: &str) -> Vec<Ref> {
  let listed = run_git(
    &args(&[
      "for-each-ref",
      "--sort=-committerdate",
      REFS_FORMAT,
      "refs/heads",
      "refs/remotes",
      "refs/tags",
    ]),
    &crate::options(root),
  );
  match listed {
    Ok(output) if output.exit_code == Some(0) => parse_refs(&output.stdout),
    _ => Vec::new(),
  }
}

/// The repository's remotes.
fn remotes(root: &str) -> Vec<String> {
  match run_git(&args(&["remote"]), &crate::options(root)) {
    Ok(output) if output.exit_code == Some(0) => String::from_utf8_lossy(&output.stdout)
      .lines()
      .map(str::trim)
      .filter(|remote| !remote.is_empty())
      .map(str::to_owned)
      .collect(),
    _ => Vec::new(),
  }
}

fn branch() -> Option<Branch> {
  with_state(|state| Some(state.status.as_ref()?.branch.clone()))
}

/// A branch name as typed, made valid where it's easy: spaces become `-`.
fn branch_name(typed: &str) -> String {
  typed.split_whitespace().collect::<Vec<_>>().join("-")
}

/// A command that reaches a remote: shown in the status bar while it runs.
fn remote_operation(title: &str, operation_args: &[&str], then: Then) -> Operation {
  let mut operation = operation(title, args(operation_args), then);
  operation.remote = true;
  operation
}

/// The branch picker: check out a branch or tag, or create a branch.
pub fn checkout() {
  let Some((root, _)) = repository() else {
    return;
  };
  let current = branch().and_then(|branch| branch.head);
  let mut items = vec![
    item("+ Create New Branch…", None, "create"),
    item("+ Create New Branch From…", None, "createFrom"),
    item("Checkout Detached…", None, "detached"),
  ];
  items.extend(
    refs(&root)
      .iter()
      .filter(|entry| entry.kind != RefKind::Local || Some(&entry.name) != current.as_ref())
      .take(MAX_REFS)
      .map(ref_item),
  );
  ask_pick("Select a branch or tag to check out", items, Ask::Checkout);
}

/// Asks for a ref, for `ask`: branches and tags, or (`branches_only`) just
/// branches other than the current one.
fn pick_ref(title: &str, branches_only: bool, ask: Ask) {
  let Some((root, _)) = repository() else {
    return;
  };
  let current = branch().and_then(|branch| branch.head);
  let items: Vec<PickItem> = refs(&root)
    .iter()
    .filter(|entry| {
      !branches_only
        || (entry.kind != RefKind::Tag
          && !(entry.kind == RefKind::Local && Some(&entry.name) == current.as_ref()))
    })
    .take(MAX_REFS)
    .map(ref_item)
    .collect();
  if items.is_empty() {
    return info("There are no other branches.");
  }
  ask_pick(title, items, ask);
}

/// Checks out a ref the picker offered: a branch, a remote branch (as a
/// local branch following it), or a tag (without a branch).
fn checkout_ref(full: &str) {
  let Some((root, _)) = repository() else {
    return;
  };
  let checkout = if let Some(name) = full.strip_prefix("refs/heads/") {
    vec!["checkout", "-q", name]
  } else if let Some(remote_branch) = full.strip_prefix("refs/remotes/") {
    let local = remote_branch
      .split_once('/')
      .map_or(remote_branch, |(_, name)| name);
    let exists = refs(&root)
      .iter()
      .any(|entry| entry.kind == RefKind::Local && entry.name == local);
    if exists {
      vec!["checkout", "-q", local]
    } else {
      vec!["checkout", "-q", "--track", remote_branch]
    }
  } else {
    vec!["checkout", "-q", "--detach", full]
  };
  operate(operation("Checking out", args(&checkout), Then::Nothing));
}

pub fn create_branch() {
  ask_input(
    "Branch name",
    "",
    Some("The new branch, from the current commit".into()),
    Ask::NewBranch { from: None },
  );
}

pub fn create_branch_from() {
  pick_ref(
    "Select a ref to create the branch from",
    false,
    Ask::NewBranchFrom,
  );
}

pub fn rename_branch() {
  let Some(current) = branch().and_then(|branch| branch.head) else {
    return info("Check out a branch to rename it.");
  };
  ask_input("New branch name", &current, None, Ask::Rename);
}

pub fn delete_branch() {
  pick_ref("Select a branch to delete", true, Ask::Delete);
}

pub fn merge_branch() {
  pick_ref(
    "Select a branch to merge into the current one",
    true,
    Ask::Merge,
  );
}

/// Deletes a local branch; one that isn't merged is deleted only after
/// asking.
fn delete(name: &str, force: bool) {
  let Some((root, _)) = repository() else {
    return;
  };
  let flag = if force { "-D" } else { "-d" };
  match run_git(&args(&["branch", flag, name]), &crate::options(&root)) {
    Ok(output) if output.exit_code == Some(0) => refresh(),
    Ok(output) if String::from_utf8_lossy(&output.stderr).contains("not fully merged") => {
      ask_confirm(
        &format!("The branch '{name}' isn't fully merged. Delete it anyway?"),
        "Its commits that aren't on another branch are lost.",
        &["Delete Branch"],
        true,
        Pending::Remote(Ask::ForceDelete(name.to_owned())),
      );
    }
    Ok(output) => error(&format!(
      "Deleting the branch failed: {}",
      crate::failure(&output)
    )),
    Err(problem) => error(&format!("Deleting the branch failed: {problem:?}")),
  }
}

/// Publishes the current branch: pushes it to a remote, which it follows
/// from then on.
pub fn publish() {
  let Some((root, _)) = repository() else {
    return;
  };
  let Some(current) = branch().and_then(|branch| branch.head) else {
    return info("Check out a branch to publish it.");
  };
  match remotes(&root).as_slice() {
    [] => info("This repository has no remotes to publish to. Add one with Git: Add Remote…"),
    [remote] => publish_to(remote, &current),
    many => ask_pick(
      "Select a remote to publish the branch to",
      many
        .iter()
        .map(|remote| item(remote, None, remote))
        .collect(),
      Ask::PublishTo(current),
    ),
  }
}

fn publish_to(remote: &str, current: &str) {
  operate(remote_operation(
    "Publishing",
    &["push", "-u", remote, current],
    Then::Nothing,
  ));
}

pub fn fetch(how: &[&str]) {
  if repository().is_none() {
    return;
  }
  let mut fetch = vec!["fetch"];
  fetch.extend(how);
  operate(remote_operation("Fetching", &fetch, Then::Nothing));
}

pub fn pull(rebase: bool) {
  if repository().is_none() {
    return;
  }
  let pull: &[&str] = if rebase {
    &["pull", "--rebase"]
  } else {
    &["pull"]
  };
  operate(remote_operation("Pulling", pull, Then::Nothing));
}

/// Pushes the current branch, or publishes it if it follows no branch.
pub fn push() {
  match branch() {
    Some(Branch {
      head: Some(_),
      upstream: Some(_),
      ..
    }) => operate(remote_operation("Pushing", &["push"], Then::Nothing)),
    Some(Branch { head: Some(_), .. }) => publish(),
    _ => info("Check out a branch to push it."),
  }
}

/// Pulls, then pushes, asking first unless the user said not to.
pub fn sync() {
  let Some(branch) = branch() else {
    return;
  };
  let (Some(_), Some(upstream)) = (&branch.head, &branch.upstream) else {
    return if branch.head.is_some() {
      publish()
    } else {
      info("Check out a branch to sync it.")
    };
  };
  if matches!(setting("confirm_sync"), Some(ConfigValue::Boolean(false))) {
    return do_sync();
  }
  ask_confirm(
    &format!("This pulls commits from and pushes commits to '{upstream}'."),
    "",
    &["OK", "OK, Don't Ask Again"],
    false,
    Pending::Remote(Ask::Sync),
  );
}

fn do_sync() {
  operate(remote_operation("Syncing", &["pull"], Then::Push));
}

/// The push that follows a sync's pull.
pub fn sync_push() -> Operation {
  remote_operation("Syncing", &["push"], Then::Nothing)
}

pub fn add_remote() {
  ask_input("Remote name", "origin", None, Ask::RemoteName);
}

pub fn remove_remote() {
  let Some((root, _)) = repository() else {
    return;
  };
  match remotes(&root).as_slice() {
    [] => info("This repository has no remotes."),
    all => ask_pick(
      "Select a remote to remove",
      all
        .iter()
        .map(|remote| item(remote, None, remote))
        .collect(),
      Ask::RemoveRemote,
    ),
  }
}

/// The user answered a question asked here.
pub fn answered(ask: Ask, value: AnswerValue) {
  let typed = |text: &str| (!text.trim().is_empty()).then(|| text.trim().to_owned());
  match (ask, value) {
    (Ask::Checkout, AnswerValue::Picked(value)) => match value.as_str() {
      "create" => create_branch(),
      "createFrom" => create_branch_from(),
      "detached" => pick_ref("Select a ref to check out", false, Ask::Detached),
      full => checkout_ref(full),
    },
    (Ask::NewBranchFrom, AnswerValue::Picked(full)) => ask_input(
      "Branch name",
      "",
      Some(format!("The new branch, from {}", short(&full))),
      Ask::NewBranch { from: Some(full) },
    ),
    (Ask::NewBranch { from }, AnswerValue::Text(name)) => {
      if let Some(name) = typed(&name) {
        let name = branch_name(&name);
        let mut create = vec!["checkout", "-q", "-b", name.as_str()];
        if let Some(from) = &from {
          create.push(from);
        }
        operate(operation(
          "Creating the branch",
          args(&create),
          Then::Nothing,
        ));
      }
    }
    (Ask::Detached, AnswerValue::Picked(full)) => operate(operation(
      "Checking out",
      args(&["checkout", "-q", "--detach", &full]),
      Then::Nothing,
    )),
    (Ask::Rename, AnswerValue::Text(name)) => {
      if let Some(name) = typed(&name) {
        operate(operation(
          "Renaming the branch",
          args(&["branch", "-m", &branch_name(&name)]),
          Then::Nothing,
        ));
      }
    }
    (Ask::Delete, AnswerValue::Picked(full)) => match full.strip_prefix("refs/heads/") {
      Some(name) => delete(name, false),
      None => info("Only local branches can be deleted here."),
    },
    (Ask::ForceDelete(name), AnswerValue::Button(0)) => delete(&name, true),
    (Ask::Merge, AnswerValue::Picked(full)) => operate(operation(
      "Merging",
      args(&["merge", short(&full)]),
      Then::Nothing,
    )),
    (Ask::PublishTo(current), AnswerValue::Picked(remote)) => publish_to(&remote, &current),
    (Ask::RemoteName, AnswerValue::Text(name)) => {
      if let Some(name) = typed(&name) {
        ask_input(
          "Remote URL",
          "",
          Some("Such as https://example.com/repo.git".into()),
          Ask::RemoteUrl(name),
        );
      }
    }
    (Ask::RemoteUrl(name), AnswerValue::Text(url)) => {
      if let Some(url) = typed(&url) {
        operate(operation(
          "Adding the remote",
          args(&["remote", "add", &name, &url]),
          Then::Nothing,
        ));
      }
    }
    (Ask::RemoveRemote, AnswerValue::Picked(name)) => operate(operation(
      "Removing the remote",
      args(&["remote", "remove", &name]),
      Then::Nothing,
    )),
    (Ask::Sync, AnswerValue::Button(button)) if button <= 1 => {
      if button == 1
        && let Err(problem) =
          host::set_configuration("confirm_sync", Some(&ConfigValue::Boolean(false)))
      {
        log(
          LogLevel::Warn,
          &format!("can't remember the choice: {problem:?}"),
        );
      }
      do_sync();
    }
    (Ask::Autofetch, AnswerValue::Button(0)) => {
      if let Err(problem) = host::set_configuration("autofetch", Some(&ConfigValue::Boolean(true)))
      {
        error(&format!(
          "Couldn't turn on fetching in the background: {problem:?}"
        ));
      }
    }
    _ => {}
  }
}

/// The status bar's sync button: how far the branch is behind and ahead
/// of the branch it follows (pulling and pushing them), Publish for a
/// branch that follows none, or what's running. `None` without a branch.
pub fn sync_item(status: &Status, busy: Option<&str>) -> Option<StatusItem> {
  let branch = &status.branch;
  let head = branch.head.as_ref()?;
  branch.oid.as_ref()?;
  if let Some(title) = busy {
    return Some(StatusItem {
      text: format!("↻ {title}…"),
      tooltip: Some(format!("Git: {title}…")),
      command: None,
    });
  }
  Some(match &branch.upstream {
    None => StatusItem {
      text: "↑ Publish".into(),
      tooltip: Some(format!("Publish {head} to a remote")),
      command: Some("ferrite.git.publish".into()),
    },
    Some(upstream) => {
      let commits = |count: u32| match count {
        1 => "1 commit".to_owned(),
        count => format!("{count} commits"),
      };
      let mut tooltip = "Synchronize changes".to_owned();
      if branch.behind > 0 {
        tooltip.push_str(&format!(
          ": pull {} from {upstream}",
          commits(branch.behind)
        ));
      }
      if branch.ahead > 0 {
        tooltip.push_str(if branch.behind > 0 { ", and " } else { ": " });
        tooltip.push_str(&format!("push {} to {upstream}", commits(branch.ahead)));
      }
      StatusItem {
        text: format!("{}↓ {}↑", branch.behind, branch.ahead),
        tooltip: Some(tooltip),
        command: Some("ferrite.git.sync".into()),
      }
    }
  })
}

/// Shows the sync button as things stand.
pub fn update_sync() {
  let item = with_state(|state| {
    let status = state.status.as_ref()?;
    sync_item(status, state.busy.as_deref())
  });
  let _ = workbench::set_status_item("sync", item.as_ref());
}

fn autofetch_enabled() -> bool {
  matches!(setting("autofetch"), Some(ConfigValue::Boolean(true)))
}

/// Waits a period, then fetches if the user turned that on. The timer runs
/// either way, so turning it on needs no restart.
pub fn schedule_autofetch() {
  if let Some(timer) = with_state(|state| state.autofetch_timer.take()) {
    timers::clear_timer(timer);
  }
  let seconds = match setting("autofetch_period") {
    Some(ConfigValue::Integer(seconds)) => seconds.clamp(30, 86_400),
    _ => 180,
  };
  let delay = u32::try_from(seconds * 1_000).unwrap_or(180_000);
  match timers::set_timer(delay, AUTOFETCH) {
    Ok(timer) => with_state(|state| state.autofetch_timer = Some(timer)),
    Err(problem) => log(
      LogLevel::Warn,
      &format!("can't fetch in the background: {problem:?}"),
    ),
  }
}

/// The background fetch's timer ran out.
pub fn autofetch() {
  let Some(fired) = timers::fired() else {
    return;
  };
  if with_state(|state| state.autofetch_timer) != Some(fired) {
    return;
  }
  with_state(|state| state.autofetch_timer = None);
  let idle = with_state(|state| state.busy.is_none());
  if autofetch_enabled()
    && idle
    && let Some((root, _)) = repository()
  {
    spawn(Job::Fetch, &args(&["fetch"]), &root, None);
  }
  schedule_autofetch();
}

/// After the user reached a remote: offers once to fetch in the background.
pub fn offer_autofetch() {
  if autofetch_enabled() || storage::get(StorageScope::Global, AUTOFETCH_OFFERED).is_some() {
    return;
  }
  let _ = storage::set(StorageScope::Global, AUTOFETCH_OFFERED, Some("true"));
  ask_confirm(
    "Would you like Ferrite to run \"git fetch\" every few minutes?",
    "You can change this later with the \"autofetch\" setting of Git.",
    &["Yes", "No"],
    false,
    Pending::Remote(Ask::Autofetch),
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::status::parse;

  #[test]
  fn refs_are_read_and_sorted_into_kinds() {
    let refs = parse_refs(
      b"refs/heads/main\0main\0abc1234\0Fix it\n\
refs/remotes/origin/HEAD\0origin\0abc1234\0Fix it\n\
refs/remotes/origin/feature\0origin/feature\0def5678\0Add it\n\
refs/tags/v1.0\0v1.0\0abc1234\0Fix it\n",
    );
    let kinds: Vec<(RefKind, &str)> = refs
      .iter()
      .map(|entry| (entry.kind, entry.name.as_str()))
      .collect();
    assert_eq!(
      kinds,
      vec![
        (RefKind::Local, "main"),
        (RefKind::Remote, "origin/feature"),
        (RefKind::Tag, "v1.0"),
      ]
    );
    assert_eq!(
      ref_item(&refs[1]).description.as_deref(),
      Some("Remote branch at def5678")
    );
    assert_eq!(short("refs/remotes/origin/feature"), "origin/feature");
    assert_eq!(branch_name("  my  new feature "), "my-new-feature");
  }

  #[test]
  fn the_sync_button_shows_what_syncing_does() {
    let status = parse(
      b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0",
    );
    let item = sync_item(&status, None).expect("a branch");
    assert_eq!(item.text, "1↓ 2↑");
    assert_eq!(
      item.tooltip.as_deref(),
      Some(
        "Synchronize changes: pull 1 commit from origin/main, and push 2 commits to origin/main"
      )
    );
    assert_eq!(item.command.as_deref(), Some("ferrite.git.sync"));
    assert_eq!(
      sync_item(&status, Some("Pulling")).expect("busy").text,
      "↻ Pulling…"
    );

    let unpublished = parse(b"# branch.oid abc\0# branch.head topic\0");
    assert_eq!(
      sync_item(&unpublished, None).expect("a branch").text,
      "↑ Publish"
    );
    let detached = parse(b"# branch.oid abc\0# branch.head (detached)\0");
    assert!(sync_item(&detached, None).is_none());
  }
}
