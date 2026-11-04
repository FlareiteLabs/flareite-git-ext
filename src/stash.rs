//! The Stashes view: changes put aside with `git stash`, newest first. Each
//! can be applied, popped, or dropped, and opened to list its files, which
//! open to compare them with the commit the stash was made on.

use ferrite_extension_api::prompts::{self, AnswerValue, InputOptions};
use ferrite_extension_api::types::LogLevel;
use ferrite_extension_api::views::{self, Expansion, ItemIcon, TreeItem, Welcome, WelcomeButton};

use crate::{
  ANSWERED, Pending, Then, args, ask_confirm, commit_changes, error, info, log, operate, operation,
  panel, repository, run_git, with_state,
};

pub const STASHES: &str = "stashes";
/// `git stash list`'s format for [`parse`].
pub const FORMAT: &str = "--format=%H%x1f%gd%x1f%gs%x1f%cr";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
  pub hash: String,
  /// Such as `stash@{0}`.
  pub reference: String,
  pub message: String,
  pub relative: String,
}

/// Parses `git stash list` with [`FORMAT`].
pub fn parse(output: &[u8]) -> Vec<Stash> {
  String::from_utf8_lossy(output)
    .lines()
    .filter_map(|line| {
      let mut fields = line.split('\u{1f}');
      Some(Stash {
        hash: fields.next()?.to_owned(),
        reference: fields.next()?.to_owned(),
        message: fields.next()?.to_owned(),
        relative: fields.next().unwrap_or_default().to_owned(),
      })
    })
    .collect()
}

/// The view's items: one per stash, opened to list its files.
pub fn items(stashes: &[Stash]) -> Vec<TreeItem> {
  stashes
    .iter()
    .map(|stash| TreeItem {
      id: stash.hash.clone(),
      parent: None,
      label: stash.message.clone(),
      description: Some(format!("{}, {}", stash.reference, stash.relative)),
      tooltip: Some(format!(
        "{}\n{} • {}",
        stash.message, stash.reference, stash.relative
      )),
      icon: ItemIcon::Named("layers".into()),
      mark: None,
      context: Some("stash".into()),
      expansion: Expansion::Collapsed,
      command: None,
      argument: Some(stash.reference.clone()),
    })
    .collect()
}

/// What a question about stashes is for.
pub enum Ask {
  /// The message of a stash to make, with `git stash push`'s options.
  Message(Vec<&'static str>),
  /// Whether to drop a stash (`None`: all of them).
  Drop(Option<String>),
}

/// Lists the stashes again.
pub fn load() {
  let Some((root, _)) = repository() else {
    return;
  };
  let listed = run_git(&args(&["stash", "list", FORMAT]), &crate::options(&root));
  let stashes = match listed {
    Ok(output) if output.exit_code == Some(0) => parse(&output.stdout),
    _ => Vec::new(),
  };
  if let Err(problem) = views::set_items(STASHES, &items(&stashes)) {
    log(LogLevel::Error, &format!("can't list stashes: {problem:?}"));
  }
  let welcome = stashes.is_empty().then(|| Welcome {
    text: "No stashes. Stashing puts your changes aside, to bring back later.".into(),
    buttons: vec![WelcomeButton {
      label: "Stash Changes".into(),
      command: "ferrite.git.stash".into(),
      argument: None,
    }],
  });
  let _ = views::set_welcome(STASHES, welcome.as_ref());
}

/// The files a stash changed, when it's opened.
pub fn expand() {
  let Some(expanding) = views::expanding() else {
    return;
  };
  let Some((root, _)) = repository() else {
    return;
  };
  let hash = expanding.item;
  match commit_changes(&root, &hash) {
    Ok(changes) => {
      let items = panel::commit_files(&hash, &changes);
      if let Err(problem) = views::set_children(STASHES, &hash, &items) {
        log(
          LogLevel::Error,
          &format!("can't list the stash's files: {problem:?}"),
        );
      }
    }
    Err(message) => error(&format!("Listing the stash's files failed: {message}")),
  }
}

/// Stashes the changes, asking for a message (which may be left empty).
pub fn stash(options: Vec<&'static str>) {
  if repository().is_none() {
    return;
  }
  let input = InputOptions {
    title: "Stash message".into(),
    value: String::new(),
    placeholder: Some("Optional: what the changes are".into()),
  };
  match prompts::input(&input, ANSWERED) {
    Ok(id) => with_state(|state| {
      state
        .prompts
        .insert(id, Pending::Stash(Ask::Message(options)));
    }),
    Err(problem) => error(&format!("Couldn't ask: {problem:?}")),
  }
}

/// Applies (or pops) the first of `references`, or the latest stash.
pub fn apply(references: &[String], pop: bool) {
  if repository().is_none() {
    return;
  }
  let verb = if pop { "pop" } else { "apply" };
  let mut command = vec!["stash", verb, "--index"];
  if let Some(reference) = references.first() {
    command.push(reference);
  }
  let title = if pop {
    "Popping the stash"
  } else {
    "Applying the stash"
  };
  operate(operation(title, args(&command), Then::Nothing));
}

/// Drops the first of `references` after asking; with none, the latest.
pub fn drop(references: &[String]) {
  let reference = references
    .first()
    .cloned()
    .unwrap_or_else(|| "stash@{0}".into());
  ask_confirm(
    &format!("Drop {reference}?"),
    "Its changes are lost.",
    &["Drop Stash"],
    true,
    Pending::Stash(Ask::Drop(Some(reference))),
  );
}

pub fn drop_all() {
  ask_confirm(
    "Drop all stashes?",
    "Their changes are lost.",
    &["Drop All"],
    true,
    Pending::Stash(Ask::Drop(None)),
  );
}

pub fn answered(ask: Ask, value: AnswerValue) {
  match (ask, value) {
    (Ask::Message(options), AnswerValue::Text(message)) => {
      let mut command = vec!["stash", "push"];
      command.extend(options);
      let message = message.trim().to_owned();
      if !message.is_empty() {
        command.extend(["-m", message.as_str()]);
      }
      operate(operation("Stashing", args(&command), Then::Nothing));
    }
    (Ask::Drop(Some(reference)), AnswerValue::Button(0)) => {
      operate(operation(
        "Dropping the stash",
        args(&["stash", "drop", &reference]),
        Then::Nothing,
      ));
    }
    (Ask::Drop(None), AnswerValue::Button(0)) => {
      operate(operation(
        "Dropping the stashes",
        args(&["stash", "clear"]),
        Then::Nothing,
      ));
    }
    _ => {}
  }
}

/// Tells the user there's nothing stashed, for commands on the latest stash.
pub fn has_stashes() -> bool {
  let Some((root, _)) = repository() else {
    return false;
  };
  let listed = run_git(&args(&["stash", "list", FORMAT]), &crate::options(&root));
  let any =
    matches!(listed, Ok(output) if output.exit_code == Some(0) && !output.stdout.is_empty());
  if !any {
    info("There are no stashes.");
  }
  any
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn stashes_are_listed_newest_first() {
    let stashes = parse(
      b"e368eb\x1fstash@{0}\x1fOn main: my work\x1f2 minutes ago\n\
a1b2c3\x1fstash@{1}\x1fWIP on main: 1fcd5f2 two\x1f3 days ago\n",
    );
    assert_eq!(stashes.len(), 2);
    assert_eq!(stashes[1].reference, "stash@{1}");
    let items = items(&stashes);
    assert_eq!(items[0].id, "e368eb");
    assert_eq!(items[0].label, "On main: my work");
    assert_eq!(
      items[0].description.as_deref(),
      Some("stash@{0}, 2 minutes ago")
    );
    assert_eq!(items[0].argument.as_deref(), Some("stash@{0}"));
    assert_eq!(items[0].expansion, Expansion::Collapsed);
  }
}
