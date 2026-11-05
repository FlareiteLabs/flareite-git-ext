//! Several repositories: the one the open folder is in, repositories in its
//! subfolders (nested ones and submodules), or, when the folder isn't in a
//! repository, those in its top subfolders, as VS Code finds them.
//!
//! Every repository's status runs in the background, for the explorer's
//! marks, change markers, and the Repositories view (shown when there's
//! more than one). One repository is active: the Changes, Commits,
//! Stashes, and File History views and the branch in the status bar are
//! its. Choosing one in Repositories makes it active, as does opening a
//! file in it.
//!
//! `git:` addresses name files relative to the base folder (the top
//! repository's, or the open folder when it isn't in one), so a file in a
//! nested repository is `git:HEAD:sub/src/main.rs`; see [`resolve`].
//!
//! Large repositories (`performance`, by default `auto`): one with more
//! than [`LARGE`] files in its index runs `git status` with Git's file
//! system monitor and untracked cache, unless its configuration sets them;
//! `on` always uses them, `off` never.

use ferrite_extension_api::explorer::{self, FileMark};
use ferrite_extension_api::files;
use ferrite_extension_api::processes::{ProcessError, ProcessOutput};
use ferrite_extension_api::types::{ConfigValue, LogLevel};
use ferrite_extension_api::views::{self, Expansion, ItemIcon, TreeItem};

use crate::status::{Kind, Status, branch_text};
use crate::{
  Job, Repository, args, backslashes, is_within, log, options, platform_path, relative_path,
  repository, run_git, spawn, with_state,
};

/// The view listing the repositories.
pub const REPOSITORIES: &str = "repositories";
/// Index entries above which a repository counts as large.
pub const LARGE: usize = 20_000;
/// Most nested repositories looked for.
const MAX_CANDIDATES: usize = 30;
/// Most explorer marks, across the repositories.
const MAX_MARKS: usize = 50_000;

/// A repository and what's known of it.
pub struct Repo {
  pub root: String,
  pub git_dir: String,
  pub status: Option<Status>,
  running: bool,
  stale: bool,
  /// Arguments that speed `git status` up (`-c core.fsmonitor=true` ...).
  speedups: Vec<String>,
}

impl Repo {
  fn new(root: String, git_dir: String) -> Self {
    Self {
      root,
      git_dir,
      status: None,
      running: false,
      stale: false,
      speedups: Vec::new(),
    }
  }
}

/// The folder `git:` addresses are relative to.
pub fn base() -> Option<String> {
  with_state(|state| state.base.clone())
}

/// A repository's place under the base folder, as an address prefix:
/// empty for the top one, else `sub/`.
fn prefix(root: &str) -> String {
  base()
    .and_then(|base| relative_path(&base, root))
    .filter(|relative| !relative.is_empty())
    .map(|relative| format!("{relative}/"))
    .unwrap_or_default()
}

/// The active repository's prefix, for the addresses `diff.rs` makes.
pub fn active_prefix() -> String {
  repository()
    .map(|(root, _)| prefix(&root))
    .unwrap_or_default()
}

/// The address of `relative` (to `root`) in `revision`.
pub fn address_in(root: &str, revision: &str, relative: &str) -> String {
  crate::diff::base_address(revision, &format!("{}{relative}", prefix(root)))
}

/// The repository whose top folder holds `path` (the innermost one).
pub fn repo_of(path: &str) -> Option<Repository> {
  with_state(|state| {
    state
      .repos
      .iter()
      .filter(|repo| is_within(path, &repo.root))
      .max_by_key(|repo| repo.root.len())
      .map(|repo| Repository {
        root: repo.root.clone(),
        git_dir: repo.git_dir.clone(),
      })
  })
}

/// The repository and path (relative to it, with `/`) an address's path
/// names.
pub fn resolve(path: &str) -> Option<(String, String)> {
  let base = base()?;
  let folder = with_state(|state| state.folder.clone())?;
  let absolute = format!(
    "{base}{}{}",
    if backslashes(&folder) { "\\" } else { "/" },
    platform_path(path, &folder)
  );
  let repo = repo_of(&absolute)?;
  let relative = relative_path(&repo.root, &absolute)?;
  Some((repo.root, relative))
}

/// Whether any repository is known.
pub fn any() -> bool {
  with_state(|state| !state.repos.is_empty())
}

/// The Git folders of the repositories.
pub fn git_dirs() -> Vec<(String, String)> {
  with_state(|state| {
    state
      .repos
      .iter()
      .map(|repo| (repo.root.clone(), repo.git_dir.clone()))
      .collect()
  })
}

/// A repository's status, as last read.
pub fn status_of(root: &str) -> Option<Status> {
  with_state(|state| {
    state
      .repos
      .iter()
      .find(|repo| repo.root == root)
      .and_then(|repo| repo.status.clone())
  })
}

/// Adds a repository; it's read in the background. Returns false if it was
/// known.
pub fn add(root: String, git_dir: String) -> bool {
  let known = with_state(|state| state.repos.iter().any(|repo| repo.root == root));
  if known {
    return false;
  }
  log(LogLevel::Info, &format!("repository at {root} ({git_dir})"));
  let folder = with_state(|state| state.folder.clone()).unwrap_or_default();
  if !is_within(&git_dir, &folder) {
    crate::watch(&git_dir);
  }
  with_state(|state| state.repos.push(Repo::new(root.clone(), git_dir)));
  check_size(&root);
  refresh(&root);
  update_view();
  true
}

/// The repository at `dir`, if `dir` is a repository's top folder.
fn repository_at(dir: &str) -> Option<(String, String)> {
  let output = run_git(
    &args(&["rev-parse", "--show-toplevel", "--absolute-git-dir"]),
    &options(dir),
  )
  .ok()?;
  if output.exit_code != Some(0) {
    return None;
  }
  let folder = with_state(|state| state.folder.clone())?;
  let text = String::from_utf8_lossy(&output.stdout);
  let mut lines = text.lines();
  let root = platform_path(lines.next()?.trim(), &folder);
  let git_dir = platform_path(lines.next()?.trim(), &folder);
  let same = |a: &str, b: &str| {
    let trim = |path: &str| path.trim_end_matches(['/', '\\']).to_owned();
    if backslashes(&folder) {
      trim(a).eq_ignore_ascii_case(&trim(b))
    } else {
      trim(a) == trim(b)
    }
  };
  same(&root, dir).then_some((root, git_dir))
}

/// Looks for repositories in `dirs` (absolute folders), adding those found.
fn look_in(dirs: impl IntoIterator<Item = String>) {
  for dir in dirs.into_iter().take(MAX_CANDIDATES) {
    if let Some((root, git_dir)) = repository_at(&dir) {
      add(root, git_dir);
    }
  }
}

/// With no repository around the open folder: looks in its top
/// subfolders.
pub fn discover_subfolders(folder: &str) {
  let Ok(list) = files::list_files(folder) else {
    return;
  };
  let separator = if backslashes(folder) { '\\' } else { '/' };
  let mut dirs: Vec<String> = Vec::new();
  for path in &list.paths {
    let Some(relative) = relative_path(folder, path) else {
      continue;
    };
    let Some((first, _)) = relative.split_once('/') else {
      continue;
    };
    if first.starts_with('.') || first == "node_modules" {
      continue;
    }
    let dir = format!("{folder}{separator}{first}");
    if !dirs.contains(&dir) {
      dirs.push(dir);
    }
  }
  look_in(dirs);
}

/// The first status of the top repository: its untracked folders may be
/// repositories of their own.
fn discover_nested(root: &str, status: &Status) {
  let folder = with_state(|state| state.folder.clone()).unwrap_or_default();
  let separator = if backslashes(&folder) { "\\" } else { "/" };
  let dirs: Vec<String> = status
    .entries
    .iter()
    .filter(|entry| entry.kind == Kind::Untracked && entry.path.ends_with('/'))
    .map(|entry| {
      format!(
        "{root}{separator}{}",
        platform_path(entry.path.trim_end_matches('/'), &folder)
      )
    })
    .collect();
  look_in(dirs);
}

/// Runs a repository's `git status` in the background, or again once the
/// running one ends.
pub fn refresh(root: &str) {
  let Some(speedups) = with_state(|state| {
    let repo = state.repos.iter_mut().find(|repo| repo.root == root)?;
    if repo.running {
      repo.stale = true;
      return None;
    }
    repo.running = true;
    Some(repo.speedups.clone())
  }) else {
    return;
  };
  let mut status_args = args(&["--no-optional-locks", "-c", "core.quotepath=false"]);
  status_args.extend(speedups);
  status_args.extend(args(&[
    "status",
    "--porcelain=v2",
    "-z",
    "--branch",
    "--untracked-files=all",
  ]));
  if !spawn(
    Job::Status {
      root: root.to_owned(),
    },
    &status_args,
    root,
    None,
  ) {
    set_running(root, false);
  }
}

fn set_running(root: &str, running: bool) {
  with_state(|state| {
    if let Some(repo) = state.repos.iter_mut().find(|repo| repo.root == root) {
      repo.running = running;
    }
  });
}

pub fn status_finished(root: String, outcome: Result<ProcessOutput, ProcessError>) {
  set_running(&root, false);
  match outcome {
    Ok(output) if output.exit_code == Some(0) => {
      if output.truncated {
        log(
          LogLevel::Warn,
          "git status printed too much; some changes aren't shown",
        );
      }
      let status = crate::status::parse(&output.stdout);
      let first = with_state(|state| {
        let repo = state.repos.iter_mut().find(|repo| repo.root == root)?;
        let first = repo.status.is_none();
        repo.status = Some(status.clone());
        Some(first)
      });
      let Some(first) = first else {
        return;
      };
      if repository().is_some_and(|(active, _)| active == root) {
        crate::show(status.clone());
      } else {
        crate::conflicts::refresh_open();
      }
      update_marks();
      update_view();
      let top = base().is_some_and(|base| base == root);
      if first && top {
        discover_nested(&root, &status);
      }
    }
    Ok(output) => {
      let message = crate::failure(&output);
      log(LogLevel::Warn, &format!("git status failed: {message}"));
      if message.contains("not a git repository") {
        // Its .git folder went away.
        return crate::start();
      }
    }
    Err(ProcessError::Killed) => {}
    Err(problem) => log(LogLevel::Warn, &format!("git status failed: {problem:?}")),
  }
  let again = with_state(|state| {
    state
      .repos
      .iter_mut()
      .find(|repo| repo.root == root)
      .is_some_and(|repo| std::mem::take(&mut repo.stale))
  });
  if again {
    refresh(&root);
  }
}

/// Marks every repository's changed files in the explorer.
pub fn update_marks() {
  let Some(folder) = with_state(|state| state.folder.clone()) else {
    return;
  };
  if !crate::marks_enabled() {
    let _ = explorer::set_file_marks(&[]);
    return;
  }
  let separator = if backslashes(&folder) { "\\" } else { "/" };
  let folder = folder.as_str();
  let marks: Vec<FileMark> = with_state(|state| {
    state
      .repos
      .iter()
      .filter_map(|repo| Some((repo.root.as_str(), repo.status.as_ref()?)))
      .flat_map(|(root, status)| {
        status.on_disk().map(move |entry| {
          let relative = entry.path.trim_end_matches('/');
          FileMark {
            path: format!("{root}{separator}{}", platform_path(relative, folder)),
            badge: Some(entry.kind.badge().into()),
            color: Some(entry.kind.color().into()),
            tooltip: Some(entry.tooltip()),
            faded: false,
            // Everything in an untracked folder is untracked.
            inherit: entry.path.ends_with('/'),
            propagate: true,
            priority: entry.kind.priority(),
          }
        })
      })
      .take(MAX_MARKS + 1)
      .collect()
  });
  let marks = if marks.len() > MAX_MARKS {
    log(
      LogLevel::Warn,
      &format!("more than {MAX_MARKS} changed files; the explorer marks the first ones"),
    );
    &marks[..MAX_MARKS]
  } else {
    &marks[..]
  };
  if let Err(problem) = explorer::set_file_marks(marks) {
    log(LogLevel::Error, &format!("can't mark files: {problem:?}"));
  }
}

fn name_of(root: &str) -> &str {
  root
    .trim_end_matches(['/', '\\'])
    .rsplit(['/', '\\'])
    .next()
    .unwrap_or(root)
}

/// The Repositories view: shown with more than one repository, each with
/// its branch and how many changes it has; the active one checked.
pub fn update_view() {
  let active = repository().map(|(root, _)| root);
  let items: Vec<TreeItem> = with_state(|state| {
    state
      .repos
      .iter()
      .map(|repo| {
        let changes = repo
          .status
          .as_ref()
          .map_or(0, |status| status.entries.len());
        let branch = repo.status.as_ref().map(branch_text);
        let description = match (&branch, changes) {
          (Some(branch), 0) => Some(branch.clone()),
          (Some(branch), n) => Some(format!("{branch} · {n} changed")),
          (None, _) => None,
        };
        let selected = active.as_deref() == Some(repo.root.as_str());
        TreeItem {
          id: repo.root.clone(),
          parent: None,
          label: name_of(&repo.root).into(),
          description,
          tooltip: Some(repo.root.clone()),
          icon: ItemIcon::Named(if selected { "check" } else { "branch" }.into()),
          mark: None,
          context: Some("repository".into()),
          expansion: Expansion::None,
          command: Some("ferrite.git.selectRepository".into()),
          argument: Some(repo.root.clone()),
        }
      })
      .collect()
  });
  let _ = views::set_visible(REPOSITORIES, items.len() > 1);
  let _ = views::set_items(REPOSITORIES, &items);
}

/// Makes a repository the active one: the views and the branch show it.
pub fn select(root: &str) {
  if repository().is_some_and(|(active, _)| active == root) {
    return;
  }
  let Some((repo, status)) = with_state(|state| {
    let repo = state.repos.iter().find(|repo| repo.root == root)?;
    Some((
      Repository {
        root: repo.root.clone(),
        git_dir: repo.git_dir.clone(),
      },
      repo.status.clone(),
    ))
  }) else {
    return;
  };
  log(LogLevel::Info, &format!("active repository: {root}"));
  with_state(|state| {
    state.repository = Some(repo);
    state.status = None;
    state.history_head = None;
    state.commits.clear();
    state.more_commits = false;
    state.progress = None;
  });
  crate::set_changes_flags();
  match status {
    Some(status) => crate::show(status),
    None => refresh(root),
  }
  crate::stash::load();
  crate::timeline::load();
  update_view();
}

/// The active file changed: its repository becomes the active one.
pub fn follow(path: &str) {
  if let Some(repo) = repo_of(path) {
    select(&repo.root);
  }
}

/// Counts a repository's files in the background, to choose its
/// performance settings.
fn check_size(root: &str) {
  if mode() == "off" {
    return;
  }
  spawn(
    Job::Index {
      root: root.to_owned(),
    },
    &args(&["--no-optional-locks", "ls-files", "-z"]),
    root,
    None,
  );
}

/// The `performance` setting: `auto`, `on`, or `off`.
fn mode() -> String {
  match crate::setting("performance") {
    Some(ConfigValue::Text(mode)) => mode,
    _ => "auto".into(),
  }
}

/// Whether the repository's configuration sets `key`.
fn configured(root: &str, key: &str) -> bool {
  run_git(&args(&["config", "--get", key]), &options(root))
    .is_ok_and(|output| output.exit_code == Some(0))
}

/// Whether Git's own file system monitor works here (Windows and macOS).
fn fsmonitor_supported(root: &str) -> bool {
  run_git(&args(&["fsmonitor--daemon", "status"]), &options(root)).is_ok_and(|output| {
    let said = format!(
      "{}{}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
    said.contains("is watching") || said.contains("not watching")
  })
}

/// The arguments that speed status up for a repository, by the mode and
/// its size.
pub fn speedups(
  mode: &str,
  files: usize,
  set_by_user: (bool, bool),
  fsmonitor: bool,
) -> Vec<String> {
  let wanted = match mode {
    "on" => true,
    "off" => false,
    _ => files > LARGE,
  };
  if !wanted {
    return Vec::new();
  }
  // In `auto`, the repository's own settings stand.
  let overrides = mode == "on";
  let mut found = Vec::new();
  if fsmonitor && (overrides || !set_by_user.0) {
    found.extend(args(&["-c", "core.fsmonitor=true"]));
  }
  if overrides || !set_by_user.1 {
    found.extend(args(&["-c", "core.untrackedCache=true"]));
  }
  found
}

pub fn index_finished(root: String, outcome: Result<ProcessOutput, ProcessError>) {
  let Ok(output) = outcome else {
    return;
  };
  if output.exit_code != Some(0) {
    return;
  }
  let files = if output.truncated {
    usize::MAX
  } else {
    output.stdout.iter().filter(|&&byte| byte == 0).count()
  };
  let mode = mode();
  if mode == "auto" && files <= LARGE {
    return;
  }
  let set_by_user = (
    configured(&root, "core.fsmonitor"),
    configured(&root, "core.untrackedCache"),
  );
  let found = speedups(&mode, files, set_by_user, fsmonitor_supported(&root));
  if found.is_empty() {
    return;
  }
  log(
    LogLevel::Info,
    &format!(
      "{root} has {} files: git status runs with {}",
      if files == usize::MAX {
        "very many".to_owned()
      } else {
        files.to_string()
      },
      found
        .iter()
        .filter(|arg| *arg != "-c")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
    ),
  );
  with_state(|state| {
    if let Some(repo) = state.repos.iter_mut().find(|repo| repo.root == root) {
      repo.speedups = found;
    }
  });
  refresh(&root);
}

/// Whether a watched path is a new repository's Git folder in the open
/// folder, which isn't known yet.
pub fn appeared(path: &str) -> bool {
  let name = path.rsplit(['/', '\\']).next().unwrap_or_default();
  name == ".git" && !git_dirs().iter().any(|(_, git_dir)| git_dir == path)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn large_repositories_get_speedups_unless_configured() {
    let flags = |found: Vec<String>| found.join(" ");
    assert_eq!(flags(speedups("auto", 10, (false, false), true)), "");
    assert_eq!(
      flags(speedups("auto", LARGE + 1, (false, false), true)),
      "-c core.fsmonitor=true -c core.untrackedCache=true"
    );
    // The repository's own settings stand in auto.
    assert_eq!(
      flags(speedups("auto", LARGE + 1, (true, false), true)),
      "-c core.untrackedCache=true"
    );
    // On overrides them; the monitor only where Git has one.
    assert_eq!(
      flags(speedups("on", 10, (true, true), false)),
      "-c core.untrackedCache=true"
    );
    assert_eq!(flags(speedups("off", LARGE * 10, (false, false), true)), "");
  }
}
