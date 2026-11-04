# Git

Source control with Git: stage and commit changes, see them side by side, browse the history of
the current branch, see the Git status of files in the explorer, and the branch you're on.

## Source Control

The **Source Control** view (its icon in the activity bar shows how many files changed) has two
sections.

**Changes** has a box for the commit message, and the changed files in groups: **Merge Changes**
(files with conflicts), **Staged Changes**, and **Changes**. Each file shows its status letter
and color, and a deleted file is struck through.

- **Click a file** to see its changes in a diff tab: under **Changes**, the index against the
  file (which you can edit there); under **Staged Changes**, the last commit against the index.
  An untracked file, or one with a conflict, opens instead.
- **Buttons on a file** (on hover or when selected) open it, stage it (**+**), unstage it
  (**−**), or discard its changes (**↶**). Right-click for the same. Select several files (Ctrl-
  or Shift-click) to act on them all.
- **Buttons on a group** stage, unstage, or discard all of its files.
- **Commit** with Ctrl+Enter in the message box, or the **✓** button. Without a message, Git asks
  for one. With nothing staged, it asks whether to stage everything and commit it. Answer
  **Always** or **Never** to stop asking (the `smart_commit` setting).
- **The "…" menu** has Commit, Commit Staged, Commit All, **Commit (Amend)** (with an empty
  message, the last commit keeps its own), **Undo Last Commit** (its changes stay staged, and its
  message comes back to the box), Stage All, Unstage All, Discard All, and **View as Tree** or
  **View as List**.
- **Discarding** asks first. For a tracked file it restores the file, losing changes that
  aren't staged. For an untracked file it deletes the file: to the Recycle Bin (or Trash) by
  default, where you can restore it, or permanently with `discard_untracked = "delete"`.

**Commits** lists the commits of the current branch, newest first, 50 at a time (**Load More
Commits** for the rest). Hover over one for its author, date, and hash. Open one to list the
files it changed (click one to see what the commit changed in it), or right-click it and choose
**Open Commit Details** for a tab with its whole message, author, committer, and changed files
with the lines added and removed.

**Git: Open Changes** (in the command palette, and on a file's menu in the explorer) shows the
changes to the active or chosen file.

## Changes in the editor

As you edit a file Git tracks, the lines that differ from the index are marked beside the line
numbers: green for added lines, blue for changed ones, and a red wedge where lines were removed.
Click a marker (or press Alt+F3 for the next change, Shift+Alt+F3 for the previous) to see the
lines the change replaced, with buttons to **stage** just that change, **revert** it, or move to
the next or previous one. **Git: Stage Selected Ranges** (also on the editor's menu) and **Diff:
Revert Selected Ranges** act on the changes your selections touch, in an editor or in a diff
tab. Tabs show their file's status like the explorer: its color and letter.

## Diff tabs

The last commit's, the index's, and older commits' versions of a file show beside the newer
version, with changed lines and characters marked. Unchanged lines away from changes collapse;
click a bar to show them. The buttons beside the tabs (and **Diff** commands) go to the next
(F7) or previous (Shift+F7) change, switch between side by side and inline, show or ignore
whitespace at the ends of lines, and collapse unchanged lines or not. A diff tab updates as you
edit the file, stage, or commit.

In a folder that isn't a repository, the view offers **Initialize Repository**. In a folder you
haven't trusted, it offers **Trust Folder**: Git doesn't run there until you do.

## The explorer and the status bar

- **In the explorer**, changed files are colored and lettered: **M** modified, **A** added,
  **R** renamed, **U** untracked, and **!** for a merge conflict. Folders that contain changes
  take the color of the most important one, with a dot. Deleted files aren't in the explorer, so
  they don't color it either (Source Control lists them). Files Git ignores are dimmed, as
  always.
- **In the status bar**, the branch you're on (or the commit, when HEAD is detached), with `*`
  when there are changes, and how far it is ahead of (↑) and behind (↓) its upstream. Click it
  to look again (**Git: Refresh**).

Everything updates as files change, including changes Git makes itself (a commit, a checkout,
or a pull in the terminal).

## Requirements

[Git](https://git-scm.com) on the `PATH`. Elsewhere, tell Ferrite where it is in
`settings.toml`:

```toml
[extensions.programs]
"ferrite.git.git" = "C:/Program Files/Git/cmd/git.exe"
```

## Permissions

- **Read open documents**: to know the active document's file, for **Open Changes**.
- **Run programs on this computer**: Git. Git can run code from the repository it works on
  (hooks, and programs its configuration names), so it runs only in folders you trust.
- **Read the files in the open folder**: to notice when they change.
- **Change and create files in the open folder**: to delete untracked files when you discard
  them.

## Settings

| Setting             | Default | What it does                                                                   |
| ------------------- | ------- | ------------------------------------------------------------------------------ |
| `explorer_marks`    | `true`  | Show the Git status of files in the explorer.                                  |
| `smart_commit`      | `ask`   | With nothing staged, `ask` whether to commit everything, `always`, or `never`. |
| `discard_untracked` | `trash` | Discarding an untracked file moves it to the `trash`, or `delete`s it.         |
| `changes_view`      | `list`  | Show changed files as a `list` or a `tree` of folders.                         |

The colors are theme colors a theme or `[workbench.colors]` can change: `ferrite.git.modified`,
`ferrite.git.added`, `ferrite.git.deleted`, `ferrite.git.renamed`, `ferrite.git.untracked`, and
`ferrite.git.conflict`.
