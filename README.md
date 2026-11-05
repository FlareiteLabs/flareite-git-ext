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

The Commits view draws the history graph beside the commits, VS Code's way: the current branch
and the branch it follows, with lanes forking and joining at merges, and branch and tag names
beside the commits they're on. Right-click a commit to **Checkout (Detached)**, **Create
Branch…** or **Create Tag…** there, **Cherry-Pick** it onto the current branch, or **Revert** it
with a new commit.

**Stashes** lists your stashes, newest first. Stash from its **+** button (or the "…" menus:
with untracked files, or only staged changes). Each stash has buttons to **apply** (✓), **pop**
(apply and drop), and **drop** it; open one to see its files, which open to show what it
changed. Dropping asks first.

**File History** follows the file you're editing: its changes not committed yet, then the
commits that changed it, newest first and across renames. Click one to see what it changed in
the file.

**Git: Clone…** asks for a repository's address and a folder to put it in, clones it (signing in
if needed), and offers to open it. It's also on Source Control's welcome when no repository is
open. **Add to .gitignore** is on the explorer's menu.

## Branches and remotes

Click the branch in the status bar to switch: pick a branch (a remote branch checks out as a
local branch following it), a tag, **+ Create New Branch…**, **+ Create New Branch From…**, or
**Checkout Detached…**. Beside it, the sync button shows how many commits the branch is behind
(↓) and ahead of (↑) the branch it follows; click it to pull, then push (Git asks the first time;
answer **OK, Don't Ask Again** to stop that). A branch that follows none shows **↑ Publish**
instead, which pushes it to a remote and follows it from then on.

The "…" menu of **Changes**, and the command palette, have **Pull**, **Pull (Rebase)**, **Push**,
**Sync**, **Fetch**, **Fetch (Prune)**, **Fetch From All Remotes**, **Checkout to…**, **Create
Branch…**, **Create Branch From…**, **Rename Branch…**, **Delete Branch…** (one that isn't merged
is deleted only after asking), **Merge Branch…**, **Rebase Branch…**, **Publish Branch**, **Add
Remote…**, and **Remove Remote…**.

## Merge conflicts

When a merge, rebase, cherry-pick, revert, or stash stops for conflicts, the conflicted files are
under **Merge Changes**, the branch in the status bar says what's in progress (`main
(Merging)`), and **Changes** says what to do, with **Continue** (▶) and **Abort** (■) buttons
(and, in a rebase, **Skip Commit** in its "…" menu). Continue makes the merge's commit with the
message Git prepared, or carries on the rebase, cherry-pick, or revert; committing with an empty
message box does the same. Abort asks first, then puts things back as they were.

A conflicted file shows each conflict's current change (green) and incoming change (blue), with
links above it, as VS Code does: **Accept Current Change**, **Accept Incoming Change**, **Accept
Both Changes**, **Compare Changes** (the file with every conflict as each side has it, side by
side), and **Resolve in Merge Editor**. The command palette also has **Accept All Current**,
**Accept All Incoming**, **Accept All Both**, and **Next Conflict** and **Previous Conflict**.
Staging a file that still has conflict markers asks first.

The **merge editor** (Resolve in Merge Editor, the merge button on a conflicted file, or every
conflicted file you open from Changes with the `merge_editor` setting on) shows the incoming
version and the current one above the result, lined up. The result starts as their common base
with the changes that don't conflict already made. Above each change are buttons to accept it,
or both sides; above each conflict in the result, to accept either side or both, or reset it.
You can also edit the result yourself. **Complete Merge** saves the file and stages it.

**Signing in.** When a remote needs a username and password (or SSH a key's passphrase), Ferrite
asks you in the command palette, hiding what you type for passwords. Git Credential Manager or an
SSH agent, if you have one, answers first. What you type goes to Git, not to the extension.

**Fetching in the background** is off by default. After your first pull, push, or fetch, Ferrite
offers once to turn it on; or set `autofetch` (and `autofetch_period`, in seconds) yourself.

Every Git command and what it says when it fails are in the **Output** view, under **Git**.

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
  when there are changes; click it to switch branches. Beside it, the sync button (see
  [Branches and remotes](#branches-and-remotes)).

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

- **Read open documents**: to know the active document's file, for **Open Changes**, and to find
  and mark merge conflicts in open files.
- **Edit open documents**: to resolve a conflict when you click Accept Current Change and the
  like.
- **Run programs on this computer**: Git, which can ask you to sign in to remotes. Git can run code from the repository it works on
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
| `autofetch`         | `false` | Fetch from the remotes in the background.                                      |
| `autofetch_period`  | `180`   | How often to fetch in the background, in seconds (at least 30).                |
| `confirm_sync`      | `true`  | Ask before syncing (pulling, then pushing).                                    |
| `merge_editor`      | `false` | Open conflicted files from Changes in the merge editor.                        |

The colors are theme colors a theme or `[workbench.colors]` can change: `ferrite.git.modified`,
`ferrite.git.added`, `ferrite.git.deleted`, `ferrite.git.renamed`, `ferrite.git.untracked`, and
`ferrite.git.conflict`.
