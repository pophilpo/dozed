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

## Decisions {#dozed-surface-decisions}

- The container owns the role. Pane items additionally declare whether they are
  normal or special buffers. Embedding an editor or picker does not turn a
  transient surface into a buffer.
- In Vim normal mode, the local quit command closes special buffers,
  transient surfaces, and navigational persistent panels. Normal buffers use
  the buffer-kill command instead.
- Pickers and context menus follow the transient local-quit rule even when they
  are not hosted by the modal layer.
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
