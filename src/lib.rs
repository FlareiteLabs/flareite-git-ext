//! Git for Ferrite: the Git status of files in the explorer, the current
//! branch in the status bar, and the Source Control views: changes to stage,
//! unstage, discard, and commit, and the commits of the current branch.
//! Changed files open in diff tabs, comparing versions the extension
//! provides (see `diff.rs`), and editors mark lines changed from the index
//! as they're edited, with each change revertible or stageable on its own
//! (the editor's quick diff, for which the index version is the original).
//!
//! When a trusted folder opens, the extension asks Git for the repository
//! the folder is in, watches the folder (and the repository's `.git`
//! folder, if it's elsewhere), and runs `git status` in the background
//! after changes settle. One status runs at a time; changes that arrive
//! meanwhile run one more. Commands that change the repository run one
//! after another, each followed by a status.

mod conflicts;
mod diff;
mod graph;
mod history;
mod panel;
mod progress;
mod remote;
mod stash;
mod status;
mod timeline;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use ferrite_extension_api::documents;
use ferrite_extension_api::explorer::{self, FileMark};
use ferrite_extension_api::processes::{self, ProcessError, ProcessOptions, ProcessOutput};
use ferrite_extension_api::prompts::{self, AnswerValue, ConfirmOptions, InputOptions};
use ferrite_extension_api::quick_diff;
use ferrite_extension_api::types::{
  Activation, CommandContext, ConfigValue, DocumentChange, DocumentId, DocumentInfo, LogLevel,
  MessageLevel, PreviewElement, PreviewNode, PreviewOptions,
};
use ferrite_extension_api::views::{self, Welcome, WelcomeButton};
use ferrite_extension_api::{Guest, export_extension, files, host, workbench, workspace};

use crate::history::{Commit, DETAILS_FORMAT, FileChange, LOG_FORMAT};
use crate::status::{Kind, Status, branch_text, branch_tooltip};

const JOB_FINISHED: &str = "ferrite.git.jobFinished";
const FILES_CHANGED: &str = "ferrite.git.filesChanged";
const ANSWERED: &str = "ferrite.git.answered";
/// The Source Control container's views.
const CHANGES: &str = "changes";
const COMMITS: &str = "commits";
/// Commits loaded at a time.
const PAGE: usize = 50;
/// Most files marked; a repository with more changes than this shows the
/// first ones.
const MAX_MARKS: usize = 50_000;

/// The repository the open folder is in.
struct Repository {
  /// Its top folder, in the platform's form.
  root: String,
  /// Its Git folder (usually `<root>/.git`), in the platform's form.
  git_dir: String,
}

/// What a running program is for.
enum Job {
  Status,
  /// `git log`, from the `skip`th commit.
  Log {
    skip: usize,
  },
  Operation(Operation),
  /// A fetch in the background, whose failures are only logged.
  Fetch,
  /// Asking which operation is in progress (see `progress.rs`).
  Progress,
}

/// A command that changes the repository.
struct Operation {
  /// Says what failed, as "<title> failed: …".
  title: String,
  args: Vec<String>,
  stdin: Option<Vec<u8>>,
  /// Where it runs: the repository's top folder by default.
  cwd: Option<String>,
  then: Then,
  /// It reaches a remote: the status bar shows it while it runs.
  remote: bool,
}

/// What to do once an operation succeeds.
enum Then {
  Nothing,
  /// Empty the commit message box.
  ClearMessage,
  /// Put text in the commit message box (an undone commit's message).
  SetMessage(String),
  /// Look for the repository again (after `git init`).
  Restart,
  /// Push (after a sync's pull).
  Push,
  /// Offer to open a repository just cloned.
  OfferOpen(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitKind {
  /// Staged changes, or all of them when none are staged (as the
  /// `smart_commit` setting says).
  Default,
  Staged,
  All,
  Amend,
}

/// A prompt waiting for the user's answer.
enum Pending {
  /// The commit message, asked for when the box is empty.
  Message(CommitKind),
  /// Whether to stage everything and commit, when nothing is staged.
  SmartCommit(String),
  /// Whether to discard: the tracked files to restore, and the untracked
  /// ones to delete.
  Discard {
    tracked: Vec<String>,
    untracked: Vec<String>,
  },
  /// About branches and remotes.
  Remote(remote::Ask),
  /// About stashes.
  Stash(stash::Ask),
  /// Whether to abort what's in progress.
  Abort(progress::Progress),
  /// Whether to stage files that still have conflict markers.
  StageConflicts(Vec<String>),
}

#[derive(Default)]
struct State {
  /// The open folder, if Git can work on it (it's trusted).
  folder: Option<String>,
  repository: Option<Repository>,
  watches: Vec<u64>,
  /// Programs running, by process id.
  jobs: HashMap<u64, Job>,
  /// A `git status` is running.
  status_running: bool,
  /// Files changed while it ran: run it again.
  stale: bool,
  status: Option<Status>,
  /// The commit history was loaded for (`None`: no commits yet).
  history_head: Option<Option<String>>,
  commits: Vec<Commit>,
  more_commits: bool,
  /// Operations waiting, and whether one is running.
  operations: VecDeque<Operation>,
  operating: bool,
  prompts: HashMap<u64, Pending>,
  /// Git wasn't found, and the user was told.
  told_missing: bool,
  /// What a command reaching a remote is doing, while it runs.
  busy: Option<String>,
  /// The timer of the next background fetch.
  autofetch_timer: Option<u64>,
  /// Open documents whose conflicts are shown.
  conflicted_documents: HashSet<DocumentId>,
  /// What's in progress: a merge, rebase, cherry-pick, or revert.
  progress: Option<progress::Progress>,
  /// The Changes view lists only some of the changes.
  too_many_changes: bool,
}

thread_local! {
  static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
  STATE.with(|state| f(&mut state.borrow_mut()))
}

fn log(level: LogLevel, message: &str) {
  host::log(level, message);
}

fn error(message: &str) {
  host::show_message(MessageLevel::Error, message);
}

fn info(message: &str) {
  host::show_message(MessageLevel::Info, message);
}

/// Whether paths here use `\` (Windows).
fn backslashes(folder: &str) -> bool {
  folder.contains('\\')
}

/// A path Git printed (with `/`), in the folder's form.
fn platform_path(path: &str, folder: &str) -> String {
  if backslashes(folder) {
    path.replace('/', "\\")
  } else {
    path.to_owned()
  }
}

/// Whether `path` is `folder` or inside it; case doesn't matter on Windows.
fn is_within(path: &str, folder: &str) -> bool {
  let (path, folder) = if backslashes(folder) {
    (path.to_lowercase(), folder.to_lowercase())
  } else {
    (path.to_owned(), folder.to_owned())
  };
  let folder = folder.trim_end_matches(['/', '\\']);
  path == folder
    || path
      .strip_prefix(folder)
      .is_some_and(|rest| rest.starts_with(['/', '\\']))
}

fn options(cwd: &str) -> ProcessOptions {
  ProcessOptions {
    cwd: Some(cwd.to_owned()),
    env: vec![
      // Never wait for a password, a pager, or an editor.
      ("GIT_TERMINAL_PROMPT".into(), "0".into()),
      ("GIT_PAGER".into(), "cat".into()),
      ("GIT_EDITOR".into(), "true".into()),
    ],
    stdin: None,
    timeout_ms: None,
  }
}

fn args(args: &[&str]) -> Vec<String> {
  args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn setting(key: &str) -> Option<ConfigValue> {
  host::configuration(key)
}

fn text_setting(key: &str, default: &str) -> String {
  match setting(key) {
    Some(ConfigValue::Text(text)) => text,
    _ => default.to_owned(),
  }
}

fn marks_enabled() -> bool {
  !matches!(setting("explorer_marks"), Some(ConfigValue::Boolean(false)))
}

fn tree_view() -> bool {
  text_setting("changes_view", "list") == "tree"
}

/// The repository's top folder and the open folder, if there's a
/// repository.
fn repository() -> Option<(String, String)> {
  with_state(|state| {
    Some((
      state.repository.as_ref()?.root.clone(),
      state.folder.clone()?,
    ))
  })
}

/// The absolute path of a path relative to the repository's top folder.
fn absolute(root: &str, path: &str) -> String {
  let separator = if backslashes(root) { "\\" } else { "/" };
  format!("{root}{separator}{}", platform_path(path, root))
}

/// The first line of a failed program's message.
fn failure(output: &ProcessOutput) -> String {
  let stderr = String::from_utf8_lossy(&output.stderr);
  let stdout = String::from_utf8_lossy(&output.stdout);
  let text = if stderr.trim().is_empty() {
    stdout
  } else {
    stderr
  };
  let lines: Vec<&str> = text
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
    .collect();
  let message = lines.join(" ");
  if message.is_empty() {
    format!("Git exited with {:?}", output.exit_code)
  } else {
    message
      .trim_start_matches("error: ")
      .trim_start_matches("fatal: ")
      .to_owned()
  }
}

/// Tells the user once a session that Git couldn't be found.
fn report_missing(reason: &str) {
  let told = with_state(|state| std::mem::replace(&mut state.told_missing, true));
  log(LogLevel::Warn, reason);
  if !told {
    host::show_message(
      MessageLevel::Warning,
      "Git was not found. Install Git (https://git-scm.com), or set where it is in the settings: \
       \"ferrite.git.git\" under [extensions.programs].",
    );
  }
  welcome(
    "Git was not found. Install Git, or set where it is in the settings (\"ferrite.git.git\" \
     under [extensions.programs]), then refresh.",
    &[("Refresh", "ferrite.git.refresh")],
  );
}

/// Shows the Changes view's welcome message (it shows while the view has
/// no items).
fn welcome(text: &str, buttons: &[(&str, &str)]) {
  let welcome = Welcome {
    text: text.into(),
    buttons: buttons
      .iter()
      .map(|(label, command)| WelcomeButton {
        label: (*label).into(),
        command: (*command).into(),
        argument: None,
      })
      .collect(),
  };
  let _ = views::set_welcome(CHANGES, Some(&welcome));
}

fn clear_output() {
  let _ = explorer::set_file_marks(&[]);
  let _ = workbench::set_status_item("branch", None);
  let _ = workbench::set_status_item("sync", None);
  with_state(|state| state.progress = None);
  // No repository: nothing to fetch.
  if let Some(timer) = with_state(|state| state.autofetch_timer.take()) {
    ferrite_extension_api::timers::clear_timer(timer);
  }
  for view in [CHANGES, COMMITS] {
    let _ = views::set_items(view, &[]);
    let _ = views::set_welcome(view, None);
    let _ = views::set_badge(view, 0);
    let _ = views::set_flags(view, &[]);
    let _ = workbench::set_view_message(view, None);
  }
}

/// Starts over for the open folder: finds its repository, watches it, and
/// runs `git status`.
fn start() {
  let (watches, jobs) = with_state(|state| {
    state.folder = None;
    state.repository = None;
    state.stale = false;
    state.status = None;
    state.status_running = false;
    state.history_head = None;
    state.commits.clear();
    state.operations.clear();
    state.operating = false;
    (
      std::mem::take(&mut state.watches),
      std::mem::take(&mut state.jobs),
    )
  });
  for watch in watches {
    workspace::unwatch(watch);
  }
  for (process, job) in jobs {
    // An operation in progress finishes; reading is just stopped.
    if !matches!(job, Job::Operation(_)) {
      processes::kill(process);
    }
  }
  clear_output();

  let Some(folder) = workspace::folder() else {
    welcome(
      "Open a folder to use Git, or clone a repository.",
      &[("Clone Repository", "ferrite.git.clone")],
    );
    return;
  };
  if !folder.trusted {
    // Git runs code from the repository (hooks, configured programs).
    log(
      LogLevel::Info,
      "the folder isn't trusted, so Git doesn't run",
    );
    welcome(
      "Git doesn't run in folders you haven't trusted, because it can run code from them \
       (hooks, and programs its configuration names).",
      &[("Trust Folder", "ferrite.git.trust")],
    );
    return;
  }
  let path = folder.path;
  with_state(|state| state.folder = Some(path.clone()));
  // Watch the folder in any case: `git init`, or a clone into it, makes it
  // a repository.
  watch(&path);

  let found = run_git(
    &args(&["rev-parse", "--show-toplevel", "--absolute-git-dir"]),
    &options(&path),
  );
  let output = match found {
    Ok(output) => output,
    Err(ProcessError::NotFound(reason)) => return report_missing(&reason),
    Err(error) => {
      log(LogLevel::Warn, &format!("git rev-parse failed: {error:?}"));
      return;
    }
  };
  if output.exit_code != Some(0) {
    log(
      LogLevel::Debug,
      &format!(
        "not a Git repository: {}",
        String::from_utf8_lossy(&output.stderr).trim()
      ),
    );
    welcome(
      "The open folder isn't in a Git repository.",
      &[
        ("Initialize Repository", "ferrite.git.init"),
        ("Clone Repository", "ferrite.git.clone"),
      ],
    );
    return;
  }
  let text = String::from_utf8_lossy(&output.stdout);
  let mut lines = text.lines();
  let (Some(root), Some(git_dir)) = (lines.next(), lines.next()) else {
    log(LogLevel::Warn, "git rev-parse printed nothing");
    return;
  };
  let repository = Repository {
    root: platform_path(root.trim(), &path),
    git_dir: platform_path(git_dir.trim(), &path),
  };
  if !is_within(&repository.git_dir, &path) {
    watch(&repository.git_dir);
  }
  log(
    LogLevel::Info,
    &format!("repository at {} ({})", repository.root, repository.git_dir),
  );
  with_state(|state| state.repository = Some(repository));
  let flags = vec!["repository".to_owned()];
  let _ = views::set_flags(COMMITS, &flags);
  set_changes_flags();
  refresh();
  remote::schedule_autofetch();
  stash::load();
  timeline::load();
}

fn set_changes_flags() {
  let mut flags = vec!["repository".to_owned()];
  if tree_view() {
    flags.push("tree".into());
  }
  flags.extend(progress::flags());
  let _ = views::set_flags(CHANGES, &flags);
}

fn watch(path: &str) {
  match workspace::watch(path, FILES_CHANGED) {
    Ok(id) => with_state(|state| state.watches.push(id)),
    Err(error) => log(LogLevel::Warn, &format!("can't watch {path}: {error:?}")),
  }
}

/// Starts a program in the repository for a job.
/// Notes a Git command in the extension's output (the Output view's Git).
fn log_command(args: &[String]) {
  log(LogLevel::Info, &format!("> git {}", args.join(" ")));
}

/// Notes what a failed Git command said.
fn log_outcome(outcome: &Result<ProcessOutput, ProcessError>) {
  match outcome {
    Ok(output) if output.exit_code != Some(0) => {
      let said = String::from_utf8_lossy(&output.stderr);
      log(
        LogLevel::Warn,
        &format!(
          "git exited with {}: {}",
          output
            .exit_code
            .map_or_else(|| "a signal".to_owned(), |code| code.to_string()),
          said.trim().chars().take(4_000).collect::<String>()
        ),
      );
    }
    Ok(_) => {}
    Err(problem) => log(LogLevel::Warn, &format!("git didn't finish: {problem:?}")),
  }
}

/// Runs Git and waits for it, within the running command's deadline.
fn run_git(args: &[String], options: &ProcessOptions) -> Result<ProcessOutput, ProcessError> {
  log_command(args);
  let outcome = processes::run("git", args, options);
  log_outcome(&outcome);
  outcome
}

fn spawn(job: Job, args: &[String], cwd: &str, stdin: Option<Vec<u8>>) -> bool {
  let mut options = options(cwd);
  options.stdin = stdin;
  log_command(args);
  match processes::spawn("git", args, &options, JOB_FINISHED) {
    Ok(id) => {
      with_state(|state| state.jobs.insert(id, job));
      true
    }
    Err(ProcessError::NotFound(reason)) => {
      report_missing(&reason);
      false
    }
    Err(problem) => {
      error(&format!("Git couldn't start: {problem:?}"));
      false
    }
  }
}

/// Runs `git status` in the background, or again once the running one ends.
fn refresh() {
  let Some((root, _)) = repository() else {
    return;
  };
  if with_state(|state| std::mem::replace(&mut state.status_running, true)) {
    with_state(|state| state.stale = true);
    return;
  }
  let started = spawn(
    Job::Status,
    &args(&[
      "--no-optional-locks",
      "-c",
      "core.quotepath=false",
      "status",
      "--porcelain=v2",
      "-z",
      "--branch",
      "--untracked-files=all",
    ]),
    &root,
    None,
  );
  if !started {
    with_state(|state| state.status_running = false);
  }
}

/// A program ended: handles it for its job.
fn job_finished() {
  let Some(finished) = processes::finished() else {
    return;
  };
  let Some(job) = with_state(|state| state.jobs.remove(&finished.process)) else {
    return;
  };
  log_outcome(&finished.outcome);
  match job {
    Job::Status => status_finished(finished.outcome),
    Job::Log { skip } => log_finished(skip, finished.outcome),
    Job::Operation(operation) => operation_finished(operation, finished.outcome),
    Job::Fetch => refresh(),
    Job::Progress => progress::finished(finished.outcome),
  }
}

fn status_finished(outcome: Result<ProcessOutput, ProcessError>) {
  with_state(|state| state.status_running = false);
  match outcome {
    Ok(output) if output.exit_code == Some(0) => {
      if output.truncated {
        log(
          LogLevel::Warn,
          "git status printed too much; some changes aren't shown",
        );
      }
      show(status::parse(&output.stdout));
    }
    Ok(output) => {
      let message = failure(&output);
      log(LogLevel::Warn, &format!("git status failed: {message}"));
      if message.contains("not a git repository") {
        // Its .git folder went away.
        return start();
      }
    }
    Err(ProcessError::Killed) => {}
    Err(problem) => log(LogLevel::Warn, &format!("git status failed: {problem:?}")),
  }
  if with_state(|state| std::mem::take(&mut state.stale)) {
    refresh();
  }
}

/// Shows a status: marks in the explorer, the branch, and the Changes view;
/// and loads the commits again if HEAD moved.
fn show(status: Status) {
  let Some((root, folder)) = repository() else {
    return;
  };
  let separator = if backslashes(&folder) { "\\" } else { "/" };
  let marks: Vec<FileMark> = if marks_enabled() {
    status
      .on_disk()
      .take(MAX_MARKS)
      .map(|entry| {
        let relative = entry.path.trim_end_matches('/');
        FileMark {
          path: format!("{root}{separator}{}", platform_path(relative, &folder)),
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
      .collect()
  } else {
    Vec::new()
  };
  let on_disk = status.on_disk().count();
  if on_disk > MAX_MARKS {
    log(
      LogLevel::Warn,
      &format!("{on_disk} changed files; the explorer marks the first {MAX_MARKS}"),
    );
  }
  if let Err(problem) = explorer::set_file_marks(&marks) {
    log(LogLevel::Error, &format!("can't mark files: {problem:?}"));
  }
  let (items, count) = panel::changes(&status, tree_view());
  if let Err(problem) = views::set_items(CHANGES, &items) {
    log(LogLevel::Error, &format!("can't list changes: {problem:?}"));
  }
  let _ = views::set_welcome(CHANGES, None);
  let _ = views::set_badge(CHANGES, u32::try_from(count).unwrap_or(u32::MAX));

  let head = status.branch.oid.clone();
  let reload = with_state(|state| {
    let reload = state.history_head.as_ref() != Some(&head);
    state.history_head = Some(head.clone());
    state.status = Some(status);
    state.too_many_changes = count > panel::MAX_FILES;
    reload
  });
  // The branch item and the view's message, until Git says what's in
  // progress.
  progress::show();
  progress::check();
  conflicts::refresh_open();
  remote::update_sync();
  if reload {
    match head {
      Some(_) => load_commits(0),
      None => {
        with_state(|state| {
          state.commits.clear();
          state.more_commits = false;
        });
        let _ = views::set_items(COMMITS, &[]);
        let no_commits = Welcome {
          text: "No commits yet.".into(),
          buttons: Vec::new(),
        };
        let _ = views::set_welcome(COMMITS, Some(&no_commits));
      }
    }
  }
}

/// Loads commits of the current branch, from the `skip`th.
fn load_commits(skip: usize) {
  let Some((root, _)) = repository() else {
    return;
  };
  let count = format!("-n{}", PAGE + 1);
  let skip_arg = format!("--skip={skip}");
  let mut log_args = args(&[
    "--no-optional-locks",
    "log",
    "--topo-order",
    LOG_FORMAT,
    &count,
    &skip_arg,
    "HEAD",
  ]);
  // The branch it follows too, as in VS Code's graph.
  if with_state(|state| {
    state
      .status
      .as_ref()
      .is_some_and(|status| status.branch.upstream.is_some())
  }) {
    log_args.push("@{upstream}".into());
  }
  log_args.push("--".into());
  spawn(Job::Log { skip }, &log_args, &root, None);
}

fn log_finished(skip: usize, outcome: Result<ProcessOutput, ProcessError>) {
  let output = match outcome {
    Ok(output) if output.exit_code == Some(0) => output,
    Ok(output) => {
      return log(
        LogLevel::Warn,
        &format!("git log failed: {}", failure(&output)),
      );
    }
    Err(problem) => return log(LogLevel::Warn, &format!("git log failed: {problem:?}")),
  };
  let mut page = history::parse_log(&output.stdout);
  let more = page.len() > PAGE;
  page.truncate(PAGE);
  let items = with_state(|state| {
    if skip == 0 {
      state.commits = page;
    } else {
      state.commits.truncate(skip);
      state.commits.extend(page);
    }
    state.more_commits = more;
    (
      panel::commits(&state.commits, more),
      graph::lay_out(&state.commits),
    )
  });
  let (items, rows) = items;
  let _ = views::set_welcome(COMMITS, None);
  if let Err(problem) = views::set_items(COMMITS, &items) {
    log(LogLevel::Error, &format!("can't list commits: {problem:?}"));
  }
  if let Err(problem) = views::set_graph(COMMITS, &rows) {
    log(
      LogLevel::Error,
      &format!("can't draw the history graph: {problem:?}"),
    );
  }
}

/// Queues an operation; it runs once those before it end.
fn operate(operation: Operation) {
  with_state(|state| state.operations.push_back(operation));
  next_operation();
}

fn next_operation() {
  // `git init` runs before there's a repository, and a clone maybe without
  // a folder.
  let root = repository()
    .map(|(root, _)| root)
    .or_else(|| with_state(|state| state.folder.clone()));
  let Some(operation) = with_state(|state| {
    if state.operating {
      return None;
    }
    let operation = state.operations.pop_front()?;
    state.operating = true;
    Some(operation)
  }) else {
    return;
  };
  let Some(cwd) = operation.cwd.clone().or(root) else {
    with_state(|state| state.operating = false);
    return next_operation();
  };
  let (operation_args, stdin) = (operation.args.clone(), operation.stdin.clone());
  if operation.remote {
    with_state(|state| state.busy = Some(operation.title.clone()));
    remote::update_sync();
  }
  if !spawn(Job::Operation(operation), &operation_args, &cwd, stdin) {
    with_state(|state| state.operating = false);
    next_operation();
  }
}

fn operation_finished(operation: Operation, outcome: Result<ProcessOutput, ProcessError>) {
  with_state(|state| state.operating = false);
  if operation.remote {
    with_state(|state| state.busy = None);
    remote::update_sync();
  }
  // A clone asks whether to open it instead of offering background fetches,
  // which would replace that question.
  let reached_remote = operation.remote
    && !matches!(operation.then, Then::OfferOpen(_))
    && matches!(&outcome, Ok(output) if output.exit_code == Some(0));
  let succeeded = match outcome {
    Ok(output) if output.exit_code == Some(0) => true,
    Ok(output) if stopped_for_conflicts(&output) => {
      host::show_message(
        MessageLevel::Warning,
        &format!(
          "{} stopped for merge conflicts. Resolve them and stage the files, then Continue (or Abort).",
          operation.title
        ),
      );
      false
    }
    Ok(output) => {
      error(&format!("{} failed: {}", operation.title, failure(&output)));
      false
    }
    Err(problem) => {
      error(&format!("{} failed: {problem:?}", operation.title));
      false
    }
  };
  if succeeded {
    match operation.then {
      Then::Nothing => {}
      Then::ClearMessage => {
        let _ = views::set_input(CHANGES, "");
      }
      Then::SetMessage(message) => {
        let _ = views::set_input(CHANGES, &message);
      }
      Then::Restart => {
        start();
        return next_operation();
      }
      Then::Push => {
        with_state(|state| state.operations.push_front(remote::sync_push()));
      }
      Then::OfferOpen(path) => remote::offer_open(path),
    }
  }
  refresh();
  // Stashes may have changed, and with a commit, the file's history.
  stash::load();
  timeline::load();
  next_operation();
  if reached_remote {
    remote::offer_autofetch();
  }
}

/// Whether Git stopped because of conflicts, leaving them to resolve.
fn stopped_for_conflicts(output: &ProcessOutput) -> bool {
  let said = |bytes: &[u8]| String::from_utf8_lossy(bytes).contains("CONFLICT (");
  said(&output.stdout) || said(&output.stderr)
}

fn operation(title: &str, operation_args: Vec<String>, then: Then) -> Operation {
  Operation {
    title: title.into(),
    args: operation_args,
    stdin: None,
    cwd: None,
    then,
    remote: false,
  }
}

/// `git <command> -- <paths>`, with paths taken literally (not as patterns).
fn with_paths(command: &[&str], paths: &[String]) -> Vec<String> {
  let mut all = args(&["--literal-pathspecs"]);
  all.extend(args(command));
  all.push("--".into());
  all.extend(paths.iter().cloned());
  all
}

/// The items a view command is for: the selected ones, or the argument.
fn chosen() -> Vec<String> {
  let selection = views::selection();
  if selection.is_empty() {
    workbench::command_argument().into_iter().collect()
  } else {
    selection
  }
}

fn has_head() -> bool {
  with_state(|state| {
    state
      .status
      .as_ref()
      .is_some_and(|status| status.branch.oid.is_some())
  })
}

/// Stages files, asking first if conflicted ones still have conflict
/// markers.
fn stage(paths: Vec<String>) {
  if paths.is_empty() {
    return;
  }
  let Some((root, _)) = repository() else {
    return;
  };
  let marked = paths
    .iter()
    .filter(|path| {
      let path = absolute(&root, path);
      is_conflicted(&path)
        && files::read_file(&path).is_ok_and(|text| conflicts::has_conflicts(&text))
    })
    .count();
  if marked > 0 {
    let message = if marked == 1 {
      "A file still has merge conflicts. Stage it anyway?".to_owned()
    } else {
      format!("{marked} files still have merge conflicts. Stage them anyway?")
    };
    return ask_confirm(
      &message,
      "Their conflict markers would be committed.",
      &["Stage"],
      false,
      Pending::StageConflicts(paths),
    );
  }
  stage_now(paths);
}

fn stage_now(paths: Vec<String>) {
  operate(operation(
    "Staging",
    with_paths(&["add", "-A"], &paths),
    Then::Nothing,
  ));
}

/// Whether the file at `path` (absolute) has merge conflicts, as Git's
/// status says.
fn is_conflicted(path: &str) -> bool {
  let Some((root, _)) = repository() else {
    return false;
  };
  let Some(relative) = relative_path(&root, path) else {
    return false;
  };
  with_state(|state| {
    state.status.as_ref().is_some_and(|status| {
      status
        .entries
        .iter()
        .any(|entry| entry.kind == Kind::Conflict && entry.path == relative)
    })
  })
}

fn unstage(paths: Vec<String>) {
  if paths.is_empty() {
    return;
  }
  let command: &[&str] = if has_head() {
    &["restore", "--staged"]
  } else {
    // Before the first commit, there's nothing to restore from.
    &["rm", "--cached", "-r", "-q"]
  };
  operate(operation(
    "Unstaging",
    with_paths(command, &paths),
    Then::Nothing,
  ));
}

/// Asks whether to discard the changes to `paths` (untracked files are
/// deleted), and does if the user agrees.
fn discard(paths: Vec<String>) {
  let (tracked, untracked): (Vec<String>, Vec<String>) = with_state(|state| {
    let Some(status) = &state.status else {
      return (Vec::new(), Vec::new());
    };
    let mut tracked = Vec::new();
    let mut untracked = Vec::new();
    for path in paths {
      match status
        .entries
        .iter()
        .find(|entry| entry.path.trim_end_matches('/') == path)
        .and_then(|entry| entry.worktree)
      {
        Some(Kind::Untracked) => untracked.push(path),
        Some(_) => tracked.push(path),
        None => {}
      }
    }
    (tracked, untracked)
  });
  let total = tracked.len() + untracked.len();
  if total == 0 {
    return;
  }
  let to_trash = text_setting("discard_untracked", "trash") != "delete";
  let trash = match repository() {
    Some((root, _)) if backslashes(&root) => "Recycle Bin",
    _ => "Trash",
  };
  let name = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
  let message = match (tracked.as_slice(), untracked.as_slice()) {
    ([one], []) => format!("Discard the changes to {}?", name(one)),
    ([], [one]) => format!("Delete {}?", name(one)),
    _ => format!("Discard the changes to {total} files?"),
  };
  let mut detail = Vec::new();
  if !tracked.is_empty() {
    detail.push("Changes that aren't staged are lost.".to_owned());
  }
  if !untracked.is_empty() {
    let files = if untracked.len() == 1 {
      "The untracked file".to_owned()
    } else {
      format!("{} untracked files", untracked.len())
    };
    detail.push(if to_trash {
      format!("{files} will be moved to the {trash}.")
    } else {
      format!("{files} will be deleted permanently. This can't be undone.")
    });
  }
  let button = if untracked.is_empty() || !tracked.is_empty() {
    if total == 1 {
      "Discard File"
    } else {
      "Discard Files"
    }
  } else if untracked.len() == 1 {
    "Delete File"
  } else {
    "Delete Files"
  };
  ask_confirm(
    &message,
    &detail.join(" "),
    &[button],
    !to_trash || !tracked.is_empty(),
    Pending::Discard { tracked, untracked },
  );
}

fn do_discard(tracked: Vec<String>, untracked: Vec<String>) {
  let Some((root, _)) = repository() else {
    return;
  };
  let to_trash = text_setting("discard_untracked", "trash") != "delete";
  for path in &untracked {
    if let Err(problem) = files::delete(&absolute(&root, path), to_trash) {
      error(&format!("Couldn't delete {path}: {problem:?}"));
    }
  }
  if tracked.is_empty() {
    refresh();
  } else {
    operate(operation(
      "Discarding changes",
      with_paths(&["restore", "--worktree"], &tracked),
      Then::Nothing,
    ));
  }
}

fn ask_confirm(message: &str, detail: &str, buttons: &[&str], destructive: bool, pending: Pending) {
  let confirm = ConfirmOptions {
    message: message.into(),
    detail: (!detail.is_empty()).then(|| detail.to_owned()),
    buttons: buttons.iter().map(|button| (*button).to_owned()).collect(),
    destructive,
  };
  match prompts::confirm(&confirm, ANSWERED) {
    Ok(id) => with_state(|state| {
      state.prompts.insert(id, pending);
    }),
    Err(problem) => error(&format!("Couldn't ask: {problem:?}")),
  }
}

/// All the changes: those to restore and those to delete.
fn all_changes() -> (Vec<String>, Vec<String>) {
  with_state(|state| {
    let mut paths = (Vec::new(), Vec::new());
    for entry in state.status.iter().flat_map(|status| &status.entries) {
      let path = entry.path.trim_end_matches('/').to_owned();
      match entry.worktree {
        Some(Kind::Untracked) => paths.1.push(path),
        Some(_) => paths.0.push(path),
        None => {}
      }
    }
    paths
  })
}

fn counts() -> (usize, usize) {
  with_state(|state| {
    let entries = state.status.iter().flat_map(|status| &status.entries);
    let staged = entries
      .clone()
      .filter(|entry| entry.index.is_some())
      .count();
    (staged, entries.count())
  })
}

/// Commits with the message in the box (asking for one if it's empty).
fn commit(kind: CommitKind) {
  if repository().is_none() {
    return;
  }
  let message = views::input(CHANGES).unwrap_or_default();
  if message.trim().is_empty() && progress::commits_with_prepared_message() {
    // A merge (or cherry-pick, or revert) has its message ready.
    return operate(Operation {
      title: "Committing".into(),
      args: args(&["commit", "--no-edit", "--quiet"]),
      stdin: None,
      cwd: None,
      then: Then::ClearMessage,
      remote: false,
    });
  }
  if message.trim().is_empty() && kind != CommitKind::Amend {
    let input = InputOptions {
      title: "Commit message".into(),
      value: String::new(),
      placeholder: Some("What the commit does".into()),
    };
    match prompts::input(&input, ANSWERED) {
      Ok(id) => with_state(|state| {
        state.prompts.insert(id, Pending::Message(kind));
      }),
      Err(problem) => error(&format!("Couldn't ask: {problem:?}")),
    }
    return;
  }
  commit_with(message, kind);
}

fn commit_with(message: String, kind: CommitKind) {
  let (staged, all) = counts();
  match kind {
    CommitKind::Amend => {}
    CommitKind::All if all == 0 => return info("There are no changes to commit."),
    CommitKind::All => return commit_all(message),
    CommitKind::Staged if staged == 0 => return info("There are no staged changes to commit."),
    CommitKind::Default if staged == 0 => {
      if all == 0 {
        return info("There are no changes to commit.");
      }
      return match text_setting("smart_commit", "ask").as_str() {
        "always" => commit_all(message),
        "never" => info("There are no staged changes to commit. Stage changes first."),
        _ => ask_confirm(
          "There are no staged changes. Stage all your changes and commit them?",
          "Choose Always or Never to stop asking (the Git extension's Smart Commit setting).",
          &["Yes", "Always", "Never"],
          false,
          Pending::SmartCommit(message),
        ),
      };
    }
    CommitKind::Default | CommitKind::Staged => {}
  }
  operate(commit_operation(message, kind == CommitKind::Amend));
}

fn commit_all(message: String) {
  operate(operation("Staging", args(&["add", "-A"]), Then::Nothing));
  operate(commit_operation(message, false));
}

fn commit_operation(message: String, amend: bool) -> Operation {
  let keep = amend && message.trim().is_empty();
  let mut commit_args = args(&["commit", "--quiet"]);
  if amend {
    commit_args.push("--amend".into());
  }
  if keep {
    commit_args.push("--no-edit".into());
  } else {
    commit_args.extend(args(&["-F", "-"]));
  }
  Operation {
    title: "Committing".into(),
    args: commit_args,
    stdin: (!keep).then(|| message.into_bytes()),
    cwd: None,
    then: Then::ClearMessage,
    remote: false,
  }
}

/// Undoes the last commit, keeping its changes staged and its message in
/// the box.
fn undo_commit() {
  let Some((root, _)) = repository() else {
    return;
  };
  if !has_head() {
    return info("There's no commit to undo.");
  }
  let last = run_git(
    &args(&["log", "-1", "--format=%P%x1f%B", "HEAD"]),
    &options(&root),
  );
  let output = match last {
    Ok(output) if output.exit_code == Some(0) => output,
    Ok(output) => return error(&format!("Undoing the commit failed: {}", failure(&output))),
    Err(problem) => return error(&format!("Undoing the commit failed: {problem:?}")),
  };
  let text = String::from_utf8_lossy(&output.stdout);
  let (parents, message) = text.split_once('\u{1f}').unwrap_or(("", &text));
  let undo = if parents.trim().is_empty() {
    // The first commit: the branch goes back to having none.
    args(&["update-ref", "-d", "HEAD"])
  } else {
    args(&["reset", "--soft", "HEAD~"])
  };
  operate(operation(
    "Undoing the commit",
    undo,
    Then::SetMessage(message.trim_end().to_owned()),
  ));
}

/// Shows changes in a diff tab.
fn open_diff(sides: diff::Sides) {
  if let Err(problem) = documents::open_diff(&sides.original, &sides.modified, &sides.title) {
    error(&format!("Showing the changes failed: {problem:?}"));
  }
}

/// The diff `sides` picks for a file's status entry, if it has one.
fn entry_diff(
  path: &str,
  sides: impl Fn(&status::Entry) -> Option<diff::Sides>,
) -> Option<diff::Sides> {
  with_state(|state| {
    let entry = state
      .status
      .as_ref()?
      .entries
      .iter()
      .find(|entry| entry.path.trim_end_matches('/') == path)?;
    sides(entry)
  })
}

/// Shows changed files' changes not staged: the index against the file.
/// One with nothing to compare opens instead.
fn open_changes(paths: Vec<String>) {
  let Some((root, _)) = repository() else {
    return;
  };
  for path in paths.iter().take(10) {
    match entry_diff(path, |entry| diff::working(entry, absolute(&root, path))) {
      Some(sides) => open_diff(sides),
      None => open(vec![path.clone()]),
    }
  }
}

/// Shows staged files' changes: `HEAD` against the index.
fn open_staged_changes(paths: Vec<String>) {
  for path in paths.iter().take(10) {
    match entry_diff(path, diff::staged) {
      Some(sides) => open_diff(sides),
      None => open(vec![path.clone()]),
    }
  }
}

/// The files a commit changed (against its first parent).
fn commit_changes(root: &str, hash: &str) -> Result<Vec<FileChange>, String> {
  let output = run_git(
    &args(&[
      "diff-tree",
      "--no-commit-id",
      "-r",
      "--name-status",
      "-z",
      "--root",
      "-m",
      "--first-parent",
      hash,
    ]),
    &options(root),
  );
  match output {
    Ok(output) if output.exit_code == Some(0) => Ok(history::parse_name_status(&output.stdout)),
    Ok(output) => Err(failure(&output)),
    Err(problem) => Err(format!("{problem:?}")),
  }
}

/// Shows what commits did to files: each argument is `<hash>/<path>`.
fn open_commit_changes(files: Vec<String>) {
  let Some((root, _)) = repository() else {
    return;
  };
  for file in files.iter().take(10) {
    let Some((hash, path)) = file.split_once('/') else {
      continue;
    };
    match commit_changes(&root, hash) {
      Ok(changes) => match changes.iter().find(|change| change.path == path) {
        Some(change) => open_diff(diff::commit(hash, change)),
        None => info(&format!("The commit didn't change {path}.")),
      },
      Err(message) => error(&format!("Showing the commit's changes failed: {message}")),
    }
  }
}

/// Opens files from commits (`<hash>/<path>`) as they are on disk.
fn open_commit_files(files: Vec<String>) {
  open(
    files
      .iter()
      .filter_map(|file| file.split_once('/').map(|(_, path)| path.to_owned()))
      .collect(),
  );
}

/// "Open Changes" for a file (the explorer's, as an absolute path) or the
/// active document: its changes not staged, else its staged ones.
fn open_file_changes(context: &CommandContext) {
  let Some((root, _)) = repository() else {
    return info("The open folder isn't in a Git repository.");
  };
  let path = workbench::command_argument().or_else(|| {
    let document = host::document(context.document?).ok()?;
    document.path
  });
  let Some(path) = path else {
    return info("Open a file to see its changes.");
  };
  if !is_within(&path, &root) {
    return info(&format!("{path} isn't in the repository."));
  }
  let relative = relative_path(&root, &path).unwrap_or_default();
  let sides = entry_diff(&relative, |entry| {
    diff::working(entry, path.clone()).or_else(|| diff::staged(entry))
  });
  match sides {
    Some(sides) => open_diff(sides),
    None => info(&format!("{relative} has no changes.")),
  }
}

/// Provides the text at a `git:` address (see `diff.rs`); a version that
/// doesn't exist is empty.
fn provide() {
  let Some(uri) = documents::requested() else {
    return;
  };
  let text = diff::parse(&uri).and_then(|(revision, path)| {
    if revision == diff::EMPTY {
      return Some(String::new());
    }
    let (root, _) = repository()?;
    if let Some(choice) = diff::conflict_side(revision) {
      return Some(conflicts::resolve_all(
        &current_text(&absolute(&root, path)),
        choice,
      ));
    }
    // A conflicted file's versions: its base (1), ours (2), theirs (3).
    // One that's missing (a file both sides added has no base) is empty.
    let stage = diff::stage(revision);
    let object = match stage {
      Some(stage) => format!(":{stage}:{path}"),
      None => format!("{revision}:{path}"),
    };
    let output = run_git(&args(&["cat-file", "blob", &object]), &options(&root)).ok()?;
    match output.exit_code {
      Some(0) => Some(String::from_utf8_lossy(&output.stdout).into_owned()),
      _ if stage.is_some() => Some(String::new()),
      _ => None,
    }
  });
  if let Err(problem) = documents::provide(&uri, text.as_deref()) {
    log(LogLevel::Warn, &format!("can't provide {uri}: {problem:?}"));
  }
}

/// A file's text as it's being edited: its open document's, or else what's
/// on disk.
fn current_text(path: &str) -> String {
  let open = host::documents().ok().and_then(|documents| {
    let document = documents
      .into_iter()
      .find(|document| document.path.as_deref() == Some(path))?;
    let slice = host::read_lines(document.id, 0, document.line_count).ok()?;
    Some(slice.lines.join("\n"))
  });
  open
    .or_else(|| files::read_file(path).ok())
    .unwrap_or_default()
}

/// The file a conflict command is for: the code lens's or the active
/// document's, or a path chosen in a view.
fn conflict_file(context: &CommandContext) -> Option<String> {
  let (root, _) = repository()?;
  let argument = workbench::command_argument();
  let from_document = || host::document(context.document?).ok()?.path;
  match argument {
    // A code lens's argument is its line.
    Some(line) if line.parse::<usize>().is_ok() => from_document(),
    Some(path) if is_within(&path, &root) => Some(path),
    Some(relative) => Some(absolute(&root, &relative)),
    None => from_document(),
  }
}

/// Compares a conflicted file's sides: every conflict as the current side
/// has it, against every conflict as the incoming side has it.
fn compare_conflict(context: &CommandContext) {
  let Some((root, _)) = repository() else {
    return;
  };
  let Some(path) = conflict_file(context) else {
    return info("Open a file with merge conflicts first.");
  };
  let Some(relative) = relative_path(&root, &path) else {
    return info(&format!("{path} isn't in the repository."));
  };
  let current = diff::address(diff::CONFLICT_CURRENT, &relative);
  let incoming = diff::address(diff::CONFLICT_INCOMING, &relative);
  // They're worked out again from the file as it is now.
  let _ = documents::changed(&[current.clone(), incoming.clone()]);
  open_diff(diff::Sides {
    original: current,
    modified: incoming,
    title: format!(
      "{}: Current Changes \u{2194} Incoming Changes",
      relative.rsplit('/').next().unwrap_or(&relative)
    ),
  });
}

/// The commit being brought in, for the merge editor: its branch's name if
/// it has one.
fn incoming_name(progress: Option<progress::Progress>) -> Option<String> {
  let (root, _) = repository()?;
  let head = progress.map_or("MERGE_HEAD", progress::Progress::head);
  let output = run_git(
    &args(&[
      "name-rev",
      "--name-only",
      "--no-undefined",
      "--exclude=refs/tags/*",
      head,
    ]),
    &options(&root),
  )
  .ok()?;
  let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
  (output.exit_code == Some(0) && !name.is_empty()).then_some(name)
}

/// Opens a conflicted file in the merge editor: theirs and ours above the
/// result, which starts again from their base if the file still has
/// conflict markers.
fn open_merge_editor(context: &CommandContext) {
  let Some((root, _)) = repository() else {
    return;
  };
  let Some(path) = conflict_file(context) else {
    return info("Open a file with merge conflicts first.");
  };
  merge_editor_for(&root, &path);
}

fn merge_editor_for(root: &str, path: &str) {
  let Some(relative) = relative_path(root, path) else {
    return info(&format!("{path} isn't in the repository."));
  };
  if !is_conflicted(path) {
    return info(&format!("{relative} has no merge conflicts."));
  }
  let in_progress = progress::current();
  let current = with_state(|state| {
    state
      .status
      .as_ref()
      .and_then(|status| status.branch.head.clone())
  });
  let marked = conflicts::has_conflicts(&current_text(path));
  let name = relative.rsplit('/').next().unwrap_or(&relative).to_owned();
  let options = documents::MergeOptions {
    base: diff::address("~1", &relative),
    incoming: documents::MergeSide {
      uri: diff::address("~3", &relative),
      title: "Incoming".into(),
      description: incoming_name(in_progress),
    },
    current: documents::MergeSide {
      uri: diff::address("~2", &relative),
      title: "Current".into(),
      description: current,
    },
    result_file: path.to_owned(),
    title: format!("Merging: {name}"),
    reset_result: marked,
    on_complete: Some("ferrite.git.mergeCompleted".into()),
  };
  if let Err(problem) = documents::open_merge(&options) {
    error(&format!("Opening the merge editor failed: {problem:?}"));
  }
}

/// Opens conflicted files from the Changes view: in the merge editor if
/// the `merge_editor` setting says so, else as files.
fn open_conflicts(paths: Vec<String>) {
  let use_editor = matches!(setting("merge_editor"), Some(ConfigValue::Boolean(true)));
  let Some((root, _)) = repository() else {
    return;
  };
  if !use_editor {
    return open(paths);
  }
  for path in paths.iter().take(10) {
    merge_editor_for(&root, &absolute(&root, path));
  }
}

/// The merge editor's result was saved: it's resolved, so it's staged.
fn merge_completed() {
  let Some((root, _)) = repository() else {
    return;
  };
  let Some(path) = workbench::command_argument() else {
    return;
  };
  if let Some(relative) = relative_path(&root, &path) {
    stage_now(vec![relative]);
  }
}

/// The index or HEAD moved: diff tabs and change markers showing their
/// versions update, and files that became tracked (or stopped being) get
/// an original (or lose it).
fn versions_changed() {
  if let Err(problem) = documents::changed(&["git::*".into(), "git:HEAD:*".into()]) {
    log(LogLevel::Warn, &format!("can't refresh diffs: {problem:?}"));
  }
  if let Err(problem) = quick_diff::originals_changed(&[]) {
    log(
      LogLevel::Warn,
      &format!("can't refresh change markers: {problem:?}"),
    );
  }
}

/// A file's path relative to the repository's top folder, with `/`, if
/// it's in the repository.
fn relative_path(root: &str, path: &str) -> Option<String> {
  is_within(path, root).then(|| {
    path[root.len()..]
      .trim_start_matches(['/', '\\'])
      .replace('\\', "/")
  })
}

/// Names a file's original for change markers: its version in the index,
/// if Git tracks it.
fn original() {
  let Some(path) = quick_diff::requested_file() else {
    return;
  };
  let uri = repository().and_then(|(root, _)| {
    let relative = relative_path(&root, &path).filter(|relative| !relative.is_empty())?;
    let tracked = run_git(
      &with_paths(
        &["ls-files", "--error-unmatch"],
        std::slice::from_ref(&relative),
      ),
      &options(&root),
    );
    matches!(tracked, Ok(output) if output.exit_code == Some(0))
      .then(|| diff::address("", &relative))
  });
  if let Err(problem) = quick_diff::set_original(&path, uri.as_deref()) {
    log(
      LogLevel::Warn,
      &format!("can't name {path}'s original: {problem:?}"),
    );
  }
}

/// Stages changes the editor chose: makes the index version of a file the
/// text it gives (its index version with those changes made).
fn stage_changes() {
  let Some(applied) = quick_diff::to_apply() else {
    return;
  };
  let Some((root, _)) = repository() else {
    return;
  };
  let Some(("", path)) = diff::parse(&applied.uri) else {
    return error("Only changes to the index can be staged.");
  };
  let run = |command: &[&str], stdin: Option<Vec<u8>>| -> Result<String, String> {
    let mut options = options(&root);
    options.stdin = stdin;
    match run_git(&args(command), &options) {
      Ok(output) if output.exit_code == Some(0) => {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
      }
      Ok(output) => Err(failure(&output)),
      Err(problem) => Err(format!("{problem:?}")),
    }
  };
  let staged = (|| {
    // Its mode in the index (an executable stays one).
    let entry = run(&["ls-files", "-s", "--", path], None)?;
    let mode = entry
      .split_whitespace()
      .next()
      .filter(|mode| !mode.is_empty())
      .unwrap_or("100644")
      .to_owned();
    let object = run(
      &["hash-object", "-w", "--stdin", &format!("--path={path}")],
      Some(applied.text.into_bytes()),
    )?;
    run(
      &[
        "update-index",
        "--cacheinfo",
        &format!("{mode},{object},{path}"),
      ],
      None,
    )
  })();
  match staged {
    Ok(_) => {
      versions_changed();
      refresh();
    }
    Err(message) => error(&format!("Staging failed: {message}")),
  }
}

/// Adds files or folders (absolute paths, from the explorer) to the
/// repository's `.gitignore`.
fn add_to_gitignore(paths: Vec<String>) {
  let Some((root, _)) = repository() else {
    return;
  };
  let lines: Vec<String> = paths
    .iter()
    .filter_map(|path| relative_path(&root, path))
    .filter(|relative| !relative.is_empty())
    .collect();
  if lines.is_empty() {
    return info("Only files in the repository can be ignored.");
  }
  let file = absolute(&root, ".gitignore");
  let mut text = match files::read_file(&file) {
    Ok(text) => text,
    Err(files::FileError::NotFound) => String::new(),
    Err(problem) => return error(&format!("Reading .gitignore failed: {problem:?}")),
  };
  let known: Vec<String> = text.lines().map(|line| line.trim().to_owned()).collect();
  let mut added = false;
  for line in lines {
    if !known.contains(&line) {
      if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
      }
      text.push_str(&line);
      text.push('\n');
      added = true;
    }
  }
  if added && let Err(problem) = files::write_file(&file, &text) {
    error(&format!("Changing .gitignore failed: {problem:?}"));
  }
}

/// Opens changed files in the editor.
fn open(paths: Vec<String>) {
  let Some((root, _)) = repository() else {
    return;
  };
  for path in paths.iter().take(10) {
    if let Err(problem) = workspace::open_file(&absolute(&root, path)) {
      log(LogLevel::Info, &format!("can't open {path}: {problem:?}"));
      info(&format!("{path} isn't on disk (it was deleted)."));
    }
  }
}

/// The files a commit changed, for its item in the Commits view.
fn expand_commit() {
  let Some(expanding) = views::expanding() else {
    return;
  };
  let Some((root, _)) = repository() else {
    return;
  };
  let hash = expanding.item;
  let changes = match commit_changes(&root, &hash) {
    Ok(changes) => changes,
    Err(message) => return error(&format!("Listing the commit's files failed: {message}")),
  };
  let items = panel::commit_files(&hash, &changes);
  if let Err(problem) = views::set_children(COMMITS, &hash, &items) {
    log(
      LogLevel::Error,
      &format!("can't list the commit's files: {problem:?}"),
    );
  }
}

/// Shows a commit in a tab: its message, author, date, and changed files.
fn commit_details(hash: &str) {
  let Some((root, _)) = repository() else {
    return;
  };
  let run = |command: &[&str]| -> Result<Vec<u8>, String> {
    match run_git(&args(command), &options(&root)) {
      Ok(output) if output.exit_code == Some(0) => Ok(output.stdout),
      Ok(output) => Err(failure(&output)),
      Err(problem) => Err(format!("{problem:?}")),
    }
  };
  let tree = [
    "diff-tree",
    "--no-commit-id",
    "-r",
    "-z",
    "--root",
    "-m",
    "--first-parent",
  ];
  let loaded = run(&["show", "-s", DETAILS_FORMAT, hash]).and_then(|details| {
    let mut names = tree.to_vec();
    names.extend(["--name-status", hash]);
    let mut numbers = tree.to_vec();
    numbers.extend(["--numstat", hash]);
    Ok((details, run(&names)?, run(&numbers)?))
  });
  let (details, names, numbers) = match loaded {
    Ok(loaded) => loaded,
    Err(message) => return error(&format!("Showing the commit failed: {message}")),
  };
  let Some(details) = history::parse_details(&details) else {
    return error("Showing the commit failed: Git's answer couldn't be read.");
  };
  let changes = history::parse_name_status(&names);
  let counts = history::parse_numstat(&numbers);

  let mut nodes: Vec<PreviewNode> = Vec::new();
  let mut add = |parent: Option<u32>, element: PreviewElement| {
    nodes.push(PreviewNode { parent, element });
    u32::try_from(nodes.len() - 1).unwrap_or(u32::MAX)
  };
  let (subject, body) = details
    .message
    .split_once('\n')
    .map_or((details.message.as_str(), ""), |(subject, body)| {
      (subject, body.trim())
    });
  let heading = add(None, PreviewElement::Heading(1));
  add(Some(heading), PreviewElement::Text(subject.into()));
  for paragraph in body
    .split("\n\n")
    .filter(|paragraph| !paragraph.trim().is_empty())
  {
    let block = add(None, PreviewElement::Paragraph);
    add(
      Some(block),
      PreviewElement::Text(paragraph.replace('\n', " ")),
    );
  }
  // "Label: value" lines.
  let mut line = |label: &str, value: PreviewElement| {
    let block = add(None, PreviewElement::Paragraph);
    let strong = add(Some(block), PreviewElement::Strong);
    add(Some(strong), PreviewElement::Text(format!("{label}: ")));
    add(Some(block), value);
  };
  line("Commit", PreviewElement::Code(details.hash.clone()));
  if !details.parents.is_empty() {
    let label = if details.parents.len() == 1 {
      "Parent"
    } else {
      "Parents"
    };
    line(label, PreviewElement::Code(details.parents.join(" ")));
  }
  line(
    "Author",
    PreviewElement::Text(format!(
      "{} <{}>, {}",
      details.author, details.author_email, details.author_date
    )),
  );
  if details.committer != details.author || details.commit_date != details.author_date {
    line(
      "Committer",
      PreviewElement::Text(format!(
        "{} <{}>, {}",
        details.committer, details.committer_email, details.commit_date
      )),
    );
  }
  let files = add(None, PreviewElement::Heading(2));
  add(
    Some(files),
    PreviewElement::Text(format!(
      "{} changed {}",
      changes.len(),
      if changes.len() == 1 { "file" } else { "files" }
    )),
  );
  let list = add(None, PreviewElement::BulletedList);
  for change in &changes {
    let item = add(Some(list), PreviewElement::ListItem(None));
    add(Some(item), PreviewElement::Code(change.kind.badge().into()));
    let path = match &change.original {
      Some(original) => format!(" {original} → {}", change.path),
      None => format!(" {}", change.path),
    };
    add(Some(item), PreviewElement::Text(path));
    match counts.get(&change.path) {
      Some(Some((added, deleted))) => {
        add(
          Some(item),
          PreviewElement::Text(format!("  +{added} −{deleted}")),
        );
      }
      Some(None) => {
        add(Some(item), PreviewElement::Text("  (binary)".into()));
      }
      None => {}
    }
  }
  let title = format!("Commit {}", details.hash.get(..8).unwrap_or(&details.hash));
  let shown = workbench::show_preview(
    "commit",
    &details.hash,
    &PreviewOptions {
      title,
      document: None,
      beside: false,
    },
  );
  if let Err(problem) = shown {
    return error(&format!("Showing the commit failed: {problem:?}"));
  }
  if let Err(problem) = workbench::set_preview_content("commit", &details.hash, &nodes) {
    error(&format!("Showing the commit failed: {problem:?}"));
  }
}

/// The user answered one of the extension's prompts.
fn answered() {
  let Some(answer) = prompts::answer() else {
    return;
  };
  let Some(pending) = with_state(|state| state.prompts.remove(&answer.prompt)) else {
    return;
  };
  match (pending, answer.value) {
    (Pending::Message(kind), AnswerValue::Text(message)) if !message.trim().is_empty() => {
      commit_with(message, kind);
    }
    (Pending::SmartCommit(message), AnswerValue::Button(button)) => match button {
      0 => commit_all(message),
      1 => {
        remember_smart_commit("always");
        commit_all(message);
      }
      2 => {
        remember_smart_commit("never");
        info("There are no staged changes to commit. Stage changes first.");
      }
      _ => {}
    },
    (Pending::Discard { tracked, untracked }, AnswerValue::Button(0)) => {
      do_discard(tracked, untracked);
    }
    (Pending::Remote(ask), value) => remote::answered(ask, value),
    (Pending::Stash(ask), value) => stash::answered(ask, value),
    (Pending::Abort(what), AnswerValue::Button(0)) => progress::do_abort(what),
    (Pending::StageConflicts(paths), AnswerValue::Button(0)) => stage_now(paths),
    _ => {}
  }
}

fn remember_smart_commit(choice: &str) {
  if let Err(problem) =
    host::set_configuration("smart_commit", Some(&ConfigValue::Text(choice.into())))
  {
    log(
      LogLevel::Warn,
      &format!("can't save the Smart Commit setting: {problem:?}"),
    );
  }
}

fn set_tree_view(tree: bool) {
  let value = ConfigValue::Text(if tree { "tree" } else { "list" }.into());
  if let Err(problem) = host::set_configuration("changes_view", Some(&value)) {
    return error(&format!("Couldn't change the view: {problem:?}"));
  }
  set_changes_flags();
  if let Some(status) = with_state(|state| state.status.clone()) {
    let (items, _) = panel::changes(&status, tree);
    let _ = views::set_items(CHANGES, &items);
  }
}

/// Whether a change to `path` can change what `git status` says. Git's own
/// bookkeeping (objects, logs, lock files) can't.
fn matters(path: &str, git_dir: &str) -> bool {
  if !is_within(path, git_dir) {
    return true;
  }
  let inside = path
    .get(git_dir.len()..)
    .unwrap_or_default()
    .trim_start_matches(['/', '\\']);
  let first = inside.split(['/', '\\']).next().unwrap_or_default();
  !(matches!(first, "objects" | "logs" | "hooks" | "fsmonitor--daemon")
    || inside.ends_with(".lock"))
}

/// Watched files changed.
fn files_changed() {
  let Some(changes) = workspace::changes() else {
    return;
  };
  let git_dir = with_state(|state| state.repository.as_ref().map(|repo| repo.git_dir.clone()));
  match git_dir {
    Some(git_dir) => {
      if changes.overflow || changes.paths.iter().any(|path| matters(path, &git_dir)) {
        refresh();
      }
      // The index or HEAD may have moved: diff tabs showing them update.
      let versions = changes.overflow
        || changes
          .paths
          .iter()
          .any(|path| is_within(path, &git_dir) && matters(path, &git_dir));
      if versions {
        versions_changed();
        timeline::load();
      }
      // A stash made or dropped outside Ferrite.
      if changes.paths.iter().any(|path| {
        is_within(path, &git_dir) && path.trim_end_matches(['/', '\\']).ends_with("stash")
      }) {
        stash::load();
      }
    }
    // Not a repository yet: did one appear?
    None => {
      let appeared = changes.paths.iter().any(|path| {
        path
          .rsplit(['/', '\\'])
          .next()
          .is_some_and(|name| name == ".git")
      });
      if appeared || changes.overflow {
        start();
      }
    }
  }
}

fn initialize() {
  let Some(folder) = with_state(|state| state.folder.clone()) else {
    return;
  };
  if repository().is_some() {
    return;
  }
  operate(Operation {
    title: "Initializing the repository".into(),
    args: args(&["init"]),
    stdin: None,
    cwd: Some(folder),
    then: Then::Restart,
    remote: false,
  });
}

struct Git;

impl Guest for Git {
  fn activate(_: Activation) -> Result<(), String> {
    start();
    Ok(())
  }

  fn deactivate() {}

  fn run_command(command: String, context: CommandContext) -> Result<(), String> {
    match command.trim_start_matches("ferrite.git.") {
      "refresh" | "folderChanged" => start(),
      "jobFinished" => job_finished(),
      "filesChanged" => files_changed(),
      "answered" => answered(),
      "trust" => workspace::request_trust(),
      "init" => initialize(),
      "open" => open(chosen()),
      "openChange" => open_changes(chosen()),
      "openStagedChange" => open_staged_changes(chosen()),
      "openCommitChange" => open_commit_changes(chosen()),
      "openCommitFile" => open_commit_files(chosen()),
      "openFileChanges" => open_file_changes(&context),
      "provide" => provide(),
      "checkout" => remote::checkout(),
      "createBranch" => remote::create_branch(),
      "createBranchFrom" => remote::create_branch_from(),
      "renameBranch" => remote::rename_branch(),
      "deleteBranch" => remote::delete_branch(),
      "mergeBranch" => remote::merge_branch(),
      "publish" => remote::publish(),
      "fetch" => remote::fetch(&[]),
      "fetchPrune" => remote::fetch(&["--prune"]),
      "fetchAll" => remote::fetch(&["--all"]),
      "pull" => remote::pull(false),
      "pullRebase" => remote::pull(true),
      "push" => remote::push(),
      "sync" => remote::sync(),
      "addRemote" => remote::add_remote(),
      "removeRemote" => remote::remove_remote(),
      "autofetch" => remote::autofetch(),
      "activeFileChanged" => timeline::load(),
      "expandStash" => stash::expand(),
      "stash" => stash::stash(Vec::new()),
      "stashIncludeUntracked" => stash::stash(vec!["--include-untracked"]),
      "stashStaged" => stash::stash(vec!["--staged"]),
      "stashApply" => stash::apply(&chosen(), false),
      "stashPop" => stash::apply(&chosen(), true),
      "stashDrop" => stash::drop(&chosen()),
      "stashApplyLatest" if stash::has_stashes() => stash::apply(&[], false),
      "stashPopLatest" if stash::has_stashes() => stash::apply(&[], true),
      "stashApplyLatest" | "stashPopLatest" => {}
      "stashDropAll" => stash::drop_all(),
      "createTag" => remote::create_tag(None),
      "createTagAt" => remote::create_tag(chosen().into_iter().next()),
      "deleteTag" => remote::delete_tag(),
      "pushTags" => remote::push_tags(),
      "cherryPick" => remote::cherry_pick(&chosen()),
      "revertCommit" => remote::revert_commit(&chosen()),
      "checkoutCommit" => remote::checkout_commit(&chosen()),
      "branchAt" => remote::branch_at(&chosen()),
      "clone" => remote::clone(),
      "addToGitignore" => add_to_gitignore(chosen()),
      "acceptCurrent" => conflicts::accept(&context, conflicts::Choice::Current),
      "acceptIncoming" => conflicts::accept(&context, conflicts::Choice::Incoming),
      "acceptBoth" => conflicts::accept(&context, conflicts::Choice::Both),
      "acceptAllCurrent" => conflicts::accept_all(&context, conflicts::Choice::Current),
      "acceptAllIncoming" => conflicts::accept_all(&context, conflicts::Choice::Incoming),
      "acceptAllBoth" => conflicts::accept_all(&context, conflicts::Choice::Both),
      "nextConflict" => conflicts::go_to(&context, false),
      "previousConflict" => conflicts::go_to(&context, true),
      "compareConflict" => compare_conflict(&context),
      "openMergeEditor" => open_merge_editor(&context),
      "openConflict" => open_conflicts(chosen()),
      "mergeCompleted" => merge_completed(),
      "continueOperation" => progress::continue_operation(),
      "abortOperation" => progress::abort(),
      "skipRebase" => progress::skip(),
      "rebaseBranch" => remote::rebase_branch(),
      "original" => original(),
      "stageChanges" => stage_changes(),
      "stageSelectedRanges" => {
        if let Err(problem) = quick_diff::apply_selected_ranges() {
          error(&format!("Staging failed: {problem:?}"));
        }
      }
      "stage" => stage(chosen()),
      "unstage" => unstage(chosen()),
      "discard" => discard(chosen()),
      "stageAll" => operate(operation("Staging", args(&["add", "-A"]), Then::Nothing)),
      "unstageAll" if has_head() => {
        operate(operation(
          "Unstaging",
          args(&["reset", "-q"]),
          Then::Nothing,
        ));
      }
      "unstageAll" => operate(operation(
        "Unstaging",
        args(&["rm", "--cached", "-r", "-q", "."]),
        Then::Nothing,
      )),
      "discardAll" => {
        let (tracked, untracked) = all_changes();
        discard(tracked.into_iter().chain(untracked).collect());
      }
      "commit" => commit(CommitKind::Default),
      "commitStaged" => commit(CommitKind::Staged),
      "commitAll" => commit(CommitKind::All),
      "commitAmend" => commit(CommitKind::Amend),
      "undoCommit" => undo_commit(),
      "viewAsTree" => set_tree_view(true),
      "viewAsList" => set_tree_view(false),
      "expandCommit" => expand_commit(),
      "loadMore" => {
        let loaded = with_state(|state| state.commits.len());
        load_commits(loaded);
      }
      "commitDetails" => {
        for hash in chosen().iter().take(5) {
          commit_details(hash);
        }
      }
      other => return Err(format!("unknown command {other}")),
    }
    Ok(())
  }

  fn document_opened(document: DocumentInfo) {
    conflicts::opened(&document);
  }

  fn document_changed(change: DocumentChange) {
    conflicts::changed(&change);
  }

  fn document_closed(document: DocumentId) {
    conflicts::closed(document);
  }
}

export_extension!(Git);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn paths_compare_as_the_platform_does() {
    assert!(is_within(r"C:\Code\App\src", r"c:\code\app"));
    assert!(is_within(r"C:\Code\App", r"C:\Code\App\"));
    assert!(!is_within(r"C:\Code\Application", r"C:\Code\App"));
    assert!(is_within("/code/app/src", "/code/app"));
    assert!(!is_within("/code/App/src", "/code/app"));
    assert_eq!(platform_path("C:/a/b", r"C:\a"), r"C:\a\b");
    assert_eq!(platform_path("/a/b", "/a"), "/a/b");
    assert_eq!(absolute(r"C:\repo", "src/a.rs"), r"C:\repo\src\a.rs");
  }

  #[test]
  fn only_some_changes_inside_git_folder_matter() {
    let git = r"C:\repo\.git";
    assert!(matters(r"C:\repo\src\main.rs", git));
    assert!(matters(r"C:\repo\.git\index", git));
    assert!(matters(r"C:\repo\.git\HEAD", git));
    assert!(matters(r"C:\repo\.git\refs\heads\main", git));
    assert!(!matters(r"C:\repo\.git\index.lock", git));
    assert!(!matters(r"C:\repo\.git\objects\ab\cdef", git));
    assert!(!matters(r"C:\repo\.git\logs\HEAD", git));
    assert!(matters(r"C:\repo\.gitignore", git));
  }

  #[test]
  fn commands_take_paths_literally() {
    assert_eq!(
      with_paths(&["add", "-A"], &["a*.rs".into()]),
      args(&["--literal-pathspecs", "add", "-A", "--", "a*.rs"])
    );
    let commit = commit_operation("Fix".into(), false);
    assert_eq!(commit.args, args(&["commit", "--quiet", "-F", "-"]));
    assert_eq!(commit.stdin.as_deref(), Some(b"Fix".as_slice()));
    let amend = commit_operation(String::new(), true);
    assert_eq!(
      amend.args,
      args(&["commit", "--quiet", "--amend", "--no-edit"])
    );
    assert!(amend.stdin.is_none());
  }

  #[test]
  fn failures_read_as_one_line() {
    let output = ProcessOutput {
      exit_code: Some(1),
      stdout: Vec::new(),
      stderr: b"error: pathspec 'x' did not match\nhint: try this\n".to_vec(),
      truncated: false,
    };
    assert_eq!(failure(&output), "pathspec 'x' did not match");
  }
}
