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
  Dired itself. Dozed deliberately combines that prompt with a Dired-like
  directory buffer, so command keys that conflict with path text need prefixes.
- Dozed ports those behaviors onto typed filesystem state and the shared
  buffer/window model. It does not reproduce Dired's mutable text buffer,
  `ls` parsing, overlays, or dedicated Emacs windows.
- The port order is navigation and session history; marks and safe batch
  operations; create, rename, trash, and undo; sorting, omission, and help;
  then previews and subtree views. History uses Dirvish's backward/forward
  semantics and discards the forward branch after new navigation.

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
  navigate to the filesystem root. Entering a different Git repository opens
  it as a project workspace; ordinary directories remain in the same browser.
  The directory path is the buffer's sole text input and retains keyboard
  focus. Typing in it incrementally filters the displayed entries, with an
  empty path query showing the full directory. Tab completes filesystem entries
  and cycles matching candidates without changing the filter; only typed edits
  change which entries are visible. Backspace moves to the parent directory
  when the handle contains a resolved path; while it contains a partial edit,
  Backspace deletes a character normally. Control-H/J/K/L navigate up, select,
  and traverse entries without moving focus out of the path. Enter opens the
  selected directory or file as the primary, maximized center-pane content.
  Escape closes the browser while its always-focused path input is active.
  Alt-B and Alt-F move backward and forward through visited directories while
  restoring the selection remembered in each directory. Marks are stored by
  path, persist across directory history, and use the Alt-M prefix because
  unmodified text remains path-filter input.
  The presentation stays line-oriented and compact, with an explicit parent
  entry, slash-terminated directory names, and lightweight file metadata.
  Refresh rereads the filesystem while
  preserving the selected path when it still exists. Its launchers can ask the
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
