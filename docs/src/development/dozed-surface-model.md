# Dozed Surface Model

Dozed classifies interactive surfaces by their lifecycle, not by their visual
shape. Every surface has one role:

- **Normal buffer**: Source code and editable text in the center area. It
  participates in pane navigation and history, and remains open until you kill
  it.
- **Special buffer**: Non-code pane content such as diagnostics, previews, and
  dashboards. It has a local quit command in Vim normal mode.
- **Persistent panel**: A workspace tool such as the project tree, Git panel,
  or terminal. Navigational panels have a local quit command. The terminal
  keeps terminal input semantics until it gains its own modal editing model.
- **Transient**: A short interaction such as a picker, prompt, or workspace
  selector. Confirming or cancelling dismisses it.

## Dired parity boundary {#dozed-dired-parity}

- GNU Dired supplies the durable semantics: a directory projection, cursor
  selection, persistent marks, and operations over marked entries or the
  current entry.
- Doom configures safer defaults, project/workspace lifecycle, omitted files,
  and Evil commands. Dirvish remains a presentation and session layer for
  history, attributes, sorting, previews, quick access, and discoverable menus.
- Doom's always-editable path prompt comes from `find-file` plus Vertico, not
  Dired itself. Dozed preserves that boundary: the path prompt and the Dired
  listing have different presentation, focus, and key contexts.
- Dozed ports those behaviors onto typed filesystem state and the shared
  buffer/window model. It does not reproduce Dired's mutable text buffer,
  `ls` parsing, overlays, or dedicated Emacs windows.
- The port order is navigation and session history; marks and safe batch
  operations; create, rename, trash, and undo; sorting, omission, and help;
  then previews and subtree views. History provides backward/forward navigation
  inspired by Dirvish, with a browser-style forward branch that is discarded
  after new navigation. Dirvish itself retains a list of visited buffers.

The behavior references are GNU Emacs's
[dired.el](https://github.com/emacs-mirror/emacs/blob/master/lisp/dired.el)
(`dired-get-marked-files`, `dired-up-directory`, and deletion flags),
[dired-aux.el](https://github.com/emacs-mirror/emacs/blob/master/lisp/dired-aux.el)
(`dired-do-copy` and `dired-do-rename`), and
[Dirvish](https://github.com/alexluigit/dirvish/blob/main/dirvish.el)
(`dirvish--find-entry` and `dirvish-reuse-session`). The port also follows the
installed Doom Dired module and the fork owner's Doom configuration: jump to
Dired from the active file, retain directory state when opening files, and
show Dired when opening a new project.
Find-file interactions are checked against the installed Doom session and
[Vertico's directory extension](https://github.com/minad/vertico/blob/main/extensions/vertico-directory.el):
directory completion, component deletion, explicit input acceptance, and
Doom's `+vertico/enter-or-preview` command.

This is behavioral alignment for the core navigation workflow, not full package
parity. Orderless completion, Dired omission rules and writable listings, and
Dirvish's attribute menus, subtree views, and rich preview system are not ported.
The owner's requested Tab cycling and prefix-first fuzzy ranking are deliberate
customizations: Doom uses `vertico-next`/`vertico-previous` for cycling and
`vertico-insert` for Tab, while its file completion styles are Orderless and
partial completion. Dozed uses its existing Nucleo matcher, prioritizing filename
prefixes over interior matches (for example, `Documents` before `.docker` for
`Doc`).

## Decisions {#dozed-surface-decisions}

- The container owns the role. Pane items additionally declare whether they are
  normal or special buffers. Embedding an editor or picker does not turn a
  transient surface into a buffer.
- Buffers own their content state and local commands. The workspace owns their
  placement, focus, splits, sizing, and maximize state. A launcher creates a
  buffer and submits a display request, so the same buffer type can appear in
  the active pane or in a split without implementing either layout itself.
- In Vim normal mode, the local quit command closes special buffers,
  transient surfaces, and navigational persistent panels. Normal buffers use
  the buffer-kill command instead.
- Next- and previous-buffer commands cycle tabs in the focused pane. Closing a
  window joins its tabs into an adjacent pane instead of killing them. Killing
  a buffer and closing a window remain separate operations.
- Pickers and context menus follow the transient local-quit rule even when they
  are not hosted by the modal layer.
- Directory browsing is a center-pane special buffer backed by the filesystem,
  not bounded by the workspace's worktrees. It starts from the active file or
  project root, falls back to the home directory in an empty workspace, and can
  navigate to the filesystem root. {#kb project_browser::OpenDirectory} jumps
  directly to Dired in the current pane and selects the active file. Returning
  reuses that pane's Dired buffer for the same folder, preserving its marks.
  Entering a different Git repository stays in the browser.
  {#kb project_browser::OpenProject} explicitly opens the selected folder (or
  the current folder when a file is selected) as a project. A new project opens
  in Dired; existing project buffers remain available.
  {#kb project_browser::FindFile} opens a transient bottom path prompt, reducing
  the editor area without adding a pane. The typed directory starts selected;
  Return opens it as Dired in the current pane without maximizing. Typing filters
  candidates. Control-J/K select candidates or the input; Tab fills and cycles
  matching paths while retaining the original search and directory listing.
  Control-L explicitly enters a selected directory. Backspace deletes a directory
  component only immediately after a slash and otherwise edits a character.
  Control-H removes the component before the cursor. Control-L enters the
  selected directory or previews a file behind the prompt, retaining input
  focus. Escape or Control-G cancels and restores the original buffer, removing
  unmodified preview tabs created by the prompt. Return accepts the selection;
  Alt-Return accepts the typed input explicitly, including nonexistent paths
  which open as unsaved file buffers.
  {#action project_browser::OpenDirectorySplit} remains available for find-file
  in a forty-percent split with its legacy cycling completion. Confirming a
  folder replaces that navigator with
  Dired and maximizes its pane. Opening a file from an existing Dired listing
  preserves the pane layout.
  In Dired, Control-L previews a selected file in the adjacent pane on the right,
  creating that pane only when needed and retaining focus in Dired. This follows
  GNU Dired's `dired-display-file` (`display-buffer` plus `find-file-noselect`),
  rather than requiring an active Vertico prompt. Repeated previews reuse that
  pane and its temporary preview tab.
  Dired has no persistent text input and therefore uses direct normal-mode
  commands. {#kb project_browser::GoUp} and {#kb menu::Confirm} navigate,
  {#kb menu::SelectNext} and {#kb menu::SelectPrevious} select,
  {#kb project_browser::CreateDirectory} prompts for a folder to create,
  {#kb project_browser::RenameSelected} renames or moves, and
  {#kb project_browser::CopySelected} copies files and folders recursively.
  {#kb project_browser::FlagForDeletion} flags entries with `D`;
  {#kb project_browser::TrashFlagged} confirms and trashes only those flags.
  {#kb project_browser::TrashSelected} directly confirms and trashes the marked
  or current entries. Unmarking clears both marks and deletion flags; toggling
  marks leaves deletion flags alone.
  Entry prompts are temporary and Escape returns focus to the Dired listing.
  All of these bindings are scoped to the Dired or entry-edit key context and
  do not participate in find-file or normal editor key resolution.
  Alt-B and Alt-F move backward and forward through visited directories while
  restoring the selection remembered in each directory. Marks are stored by
  path and persist across directory history. File operations use marked entries
  from the current directory when any exist, including marks hidden by a
  transient filter; otherwise they use the current entry. Batch moves validate
  every destination before making changes and attempt to roll back completed
  moves if a later move fails. A single entry can also be moved into an existing
  folder. Copying preserves symbolic links without following them, including
  dangling links and directory loops. Existing destinations are rejected, and
  folders cannot be copied or moved inside themselves through a symlink alias.
  Copy failures report that earlier copies may remain.
  Trash operations never permanently delete and
  the most recent batch can be restored.
  The presentation stays line-oriented and compact, with an explicit parent
  entry, slash-terminated directory names, and lightweight file metadata.
  Filenames and path inputs use the buffer's monospaced font at UI text size.
  The selected Dired filename has a block cursor as well as a row highlight;
  dots and letters occupy the same cell. Path inputs use a compact line height
  instead of inheriting the source editor's line spacing.
  Text inputs and Vim normal, insert, replace, and pending-operator modes use
  block cursors by default; explicit user cursor settings remain supported.
  Folders sort first, with natural number ordering within each group.
  Refresh and returning focus to Dired reread the filesystem while preserving
  the selected path when it still exists. Marks for vanished entries are cleared.
  Entry prompts block navigation and refresh until accepted or cancelled.
  Its launchers can ask the
  workspace to display the same buffer in the active pane or as a horizontal
  split. The compact launcher requests forty percent of the pane being split;
  sizing, maximize, and restore remain workspace behavior.
  The existing Project Panel remains available while directory-buffer
  operations are added.
- Mouse, Vim, and command-driven interaction follow the same lifecycle rules.
- Center-pane maximize temporarily renders only the focused pane without
  changing the split tree, so restoring it recovers the exact prior layout.
  The pane renders flush with the center bounds because maximize is layout
  state, not a transient card or overlay.
  Project and Git panels are excluded until their replacements define this
  behavior. Terminals participate in pane maximization whether they start in
  the center or in the terminal dock; restoring a dock terminal returns it to
  the still-open dock. Pending workspace key sequences are not forwarded to the
  terminal, and terminal Vim cursor position is preserved across the resize.
- Closing a transient surface restores the previous valid focus. If that focus
  no longer exists, focus returns to the active workspace.
- The surface container captures and restores focus. Its content requests
  dismissal without choosing the next focus target.
- A live project workspace keeps its buffers, pane layout, and persistent panel
  state. Transient surfaces are never part of restored workspace state.
- An application window contains project workspaces and their surfaces. It is
  not a fourth surface role.
- Existing Zed surface abstractions remain in place. The role describes shared
  behavior; it does not introduce a parallel window system.
