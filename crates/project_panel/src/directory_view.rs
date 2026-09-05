use std::{
    cmp::Ordering,
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use editor::{
    Editor, EditorElement, EditorEvent, EditorStyle,
    actions::{Backspace, MoveToEndOfLine},
};
use fs::{CopyOptions, Fs, RemoveOptions, RenameOptions, TrashId};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, PromptLevel, Render, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, uniform_list,
};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use project::{Project, ProjectPath};
use ui::{
    Color, Icon, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing, WithScrollbar,
    prelude::*,
};
use util::{
    ResultExt,
    paths::{self, PathExt as _},
    size::format_file_size,
};
use workspace::{
    Item, OpenMode, OpenOptions, OpenVisible, Workspace, item::ItemEvent,
    notifications::DetachAndPromptErr,
};

actions!(
    project_browser,
    [
        GoUp,
        ConfirmPath,
        ConfirmInput,
        CompletePath,
        PathBackspace,
        NavigateSelected,
        OpenProject,
        HistoryBack,
        HistoryForward,
        MarkSelected,
        UnmarkSelected,
        ClearMarks,
        ToggleMarks,
        SelectNextMarked,
        SelectPreviousMarked,
        CreateDirectory,
        CopySelected,
        RenameSelected,
        ConfirmEntryEdit,
        CancelEntryEdit,
        TrashSelected,
        FlagForDeletion,
        TrashFlagged,
        UndoTrash,
        Refresh
    ]
);

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryEntry {
    path: PathBuf,
    is_directory: bool,
    is_parent: bool,
    is_symlink: bool,
    length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathCompletionCandidate {
    path: PathBuf,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathCompletion {
    candidates: Vec<PathCompletionCandidate>,
    selected_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedPath {
    path: PathBuf,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenBehavior {
    Navigate,
    Open,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectoryViewMode {
    FindFile,
    Dired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EntryEditKind {
    CreateDirectory,
    Copy { sources: Vec<PathBuf> },
    Rename { sources: Vec<PathBuf> },
}

impl EntryEditKind {
    fn label(&self) -> SharedString {
        match self {
            Self::CreateDirectory => "Create directory".into(),
            Self::Copy { sources } => format!("Copy {} entries", sources.len()).into(),
            Self::Rename { sources } if sources.len() == 1 => "Rename".into(),
            Self::Rename { sources } => format!("Move {} entries", sources.len()).into(),
        }
    }

    fn missing_input_message(&self) -> &'static str {
        match self {
            Self::CreateDirectory => "Enter a directory name",
            Self::Copy { .. } => "Enter a destination name or directory",
            Self::Rename { sources } if sources.len() == 1 => "Enter a new name or path",
            Self::Rename { .. } => "Enter an existing destination directory",
        }
    }

    fn failure_description(&self) -> &'static str {
        match self {
            Self::CreateDirectory => "create directory",
            Self::Copy { .. } => "copy entries",
            Self::Rename { sources } if sources.len() == 1 => "rename entry",
            Self::Rename { .. } => "move entries",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryHistoryEntry {
    path: PathBuf,
    selected_path: Option<PathBuf>,
}

struct PromptOrigin {
    directory: PathBuf,
    pane: WeakEntity<workspace::Pane>,
    active_item: Option<Box<dyn workspace::ItemHandle>>,
    preview_item: Option<Box<dyn workspace::ItemHandle>>,
    item_ids: HashSet<gpui::EntityId>,
    previews: Vec<Box<dyn workspace::ItemHandle>>,
    last_preview: Option<gpui::EntityId>,
}

impl DirectoryEntry {
    fn parent(path: PathBuf) -> Self {
        Self {
            path,
            is_directory: true,
            is_parent: true,
            is_symlink: false,
            length: 0,
        }
    }

    fn file_name(&self) -> SharedString {
        if self.is_parent {
            return format!("..{}", std::path::MAIN_SEPARATOR).into();
        }
        let mut name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned());
        if self.is_directory && !name.ends_with(std::path::MAIN_SEPARATOR) {
            name.push(std::path::MAIN_SEPARATOR);
        }
        name.into()
    }

    fn details(&self) -> SharedString {
        if self.is_directory {
            "<dir>".into()
        } else if self.is_symlink {
            "link".into()
        } else {
            format_file_size(self.length, false).into()
        }
    }
}

pub struct ProjectDirectoryView {
    fs: Arc<dyn Fs>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    mode: DirectoryViewMode,
    is_prompt: bool,
    input_selected: bool,
    prompt_origin: Option<PromptOrigin>,
    committing_prompt: bool,
    current_path: PathBuf,
    all_entries: Vec<DirectoryEntry>,
    entries: Vec<DirectoryEntry>,
    selected_index: usize,
    loading: bool,
    load_error: Option<SharedString>,
    path_editor: Entity<Editor>,
    path_error: Option<SharedString>,
    path_completion: Option<PathCompletion>,
    resolved_path: Option<ResolvedPath>,
    path_editor_text: String,
    filter_query: String,
    history: Vec<DirectoryHistoryEntry>,
    history_index: usize,
    marked_paths: HashSet<PathBuf>,
    flagged_paths: HashSet<PathBuf>,
    entry_edit: Option<EntryEditKind>,
    trash_history: Vec<Vec<TrashId>>,
    operation_in_progress: bool,
    operation_error: Option<SharedString>,
    item_focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    _load_task: Task<()>,
    _path_task: Task<()>,
    _preview_task: Task<()>,
    _operation_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectDirectoryView {
    fn new(
        fs: Arc<dyn Fs>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        current_path: PathBuf,
        selected_path: Option<PathBuf>,
        mode: DirectoryViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path_text = path_text(&current_path, true);
        let path_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(path_text.clone(), window, cx);
            editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
            editor
        });
        let item_focus_handle = cx.focus_handle();
        let mut this = Self {
            fs,
            project,
            workspace,
            mode,
            is_prompt: false,
            input_selected: false,
            prompt_origin: None,
            committing_prompt: false,
            current_path: current_path.clone(),
            all_entries: Vec::new(),
            entries: Vec::new(),
            selected_index: 0,
            loading: false,
            load_error: None,
            path_editor: path_editor.clone(),
            path_error: None,
            path_completion: None,
            resolved_path: Some(ResolvedPath {
                path: current_path.clone(),
                text: path_text.clone(),
            }),
            path_editor_text: path_text,
            filter_query: String::new(),
            history: vec![DirectoryHistoryEntry {
                path: current_path.clone(),
                selected_path: None,
            }],
            history_index: 0,
            marked_paths: HashSet::new(),
            flagged_paths: HashSet::new(),
            entry_edit: None,
            trash_history: Vec::new(),
            operation_in_progress: false,
            operation_error: None,
            item_focus_handle: item_focus_handle.clone(),
            scroll_handle: UniformListScrollHandle::new(),
            _load_task: Task::ready(()),
            _path_task: Task::ready(()),
            _preview_task: Task::ready(()),
            _operation_task: Task::ready(()),
            _subscriptions: Vec::new(),
        };
        this._subscriptions
            .push(cx.subscribe(&path_editor, |this, path_editor, event, cx| {
                if !matches!(event, EditorEvent::BufferEdited) {
                    return;
                }

                let text = path_editor.read(cx).text(cx);
                if this.path_editor_text == text {
                    return;
                }

                this.path_editor_text = text.clone();
                if this.entry_edit.is_some() {
                    this.path_error = None;
                    cx.notify();
                    return;
                }
                this.resolved_path = None;
                this.path_completion = None;
                this.path_error = None;
                if this.is_prompt {
                    this.update_prompt_input(text, cx);
                    return;
                }
                this.filter_query = path_query(&text, &this.current_path).to_lowercase();
                let selected_path = this.selected_entry().map(|entry| entry.path);
                this.update_visible_entries(selected_path.as_deref());
                cx.notify();
            }));
        this._subscriptions
            .push(cx.on_focus(&item_focus_handle, window, |this, window, cx| {
                if this.mode == DirectoryViewMode::FindFile || this.entry_edit.is_some() {
                    this.path_editor.read(cx).focus_handle(cx).focus(window, cx);
                }
            }));
        this._subscriptions.push(
            cx.on_focus_in(&item_focus_handle, window, |this, window, cx| {
                if this.mode == DirectoryViewMode::Dired && !this.loading {
                    this.refresh(&Refresh, window, cx);
                }
            }),
        );
        this.load_directory(current_path, selected_path, window, cx);
        this
    }

    fn set_path_text(
        &mut self,
        path: PathBuf,
        is_directory: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = path_text(&path, is_directory);
        self.resolved_path = Some(ResolvedPath {
            path,
            text: text.clone(),
        });
        self.path_editor_text = text.clone();
        self.path_completion = None;
        self.path_error = None;
        self.path_editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
        });
    }

    fn set_completed_path(
        &mut self,
        candidate: PathCompletionCandidate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input_selected = false;
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.path == candidate.path)
        {
            self.selected_index = index;
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        self.resolved_path = Some(ResolvedPath {
            path: candidate.path,
            text: candidate.text.clone(),
        });
        self.path_editor_text = candidate.text.clone();
        self.path_error = None;
        self.path_editor.update(cx, |editor, cx| {
            editor.set_text(candidate.text, window, cx);
            editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
        });
    }

    fn selected_entry(&self) -> Option<DirectoryEntry> {
        if self.is_prompt && self.input_selected {
            return None;
        }
        self.entries.get(self.selected_index).cloned()
    }

    fn default_selected_index(entries: &[DirectoryEntry]) -> usize {
        if entries.len() > 1 && entries.first().is_some_and(|entry| entry.is_parent) {
            1
        } else {
            0
        }
    }

    fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            self.selected_index = 0;
            return;
        }

        self.selected_index = index.min(self.entries.len().saturating_sub(1));
        self.input_selected = false;
        self.scroll_handle
            .scroll_to_item(self.selected_index, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_prompt && self.input_selected {
            self.select_index(0, cx);
            return;
        }
        self.select_index(self.selected_index.saturating_add(1), cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_prompt && self.selected_index == 0 {
            self.input_selected = true;
            cx.notify();
            return;
        }
        self.select_index(self.selected_index.saturating_sub(1), cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(0, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(usize::MAX, cx);
    }

    fn mark_selected(&mut self, _: &MarkSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().filter(|entry| !entry.is_parent) else {
            return;
        };
        self.flagged_paths.remove(&entry.path);
        self.marked_paths.insert(entry.path);
        self.select_index(self.selected_index.saturating_add(1), cx);
    }

    fn unmark_selected(&mut self, _: &UnmarkSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().filter(|entry| !entry.is_parent) else {
            return;
        };
        self.marked_paths.remove(&entry.path);
        self.flagged_paths.remove(&entry.path);
        self.select_index(self.selected_index.saturating_add(1), cx);
    }

    fn clear_marks(&mut self, _: &ClearMarks, _: &mut Window, cx: &mut Context<Self>) {
        for entry in &self.all_entries {
            self.marked_paths.remove(&entry.path);
            self.flagged_paths.remove(&entry.path);
        }
        cx.notify();
    }

    fn toggle_marks(&mut self, _: &ToggleMarks, _: &mut Window, cx: &mut Context<Self>) {
        for entry in self.entries.iter().filter(|entry| !entry.is_parent) {
            if self.flagged_paths.contains(&entry.path) {
                continue;
            }
            if !self.marked_paths.remove(&entry.path) {
                self.marked_paths.insert(entry.path.clone());
            }
        }
        cx.notify();
    }

    fn select_next_marked(&mut self, _: &SelectNextMarked, _: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .entries
            .iter()
            .enumerate()
            .skip(self.selected_index.saturating_add(1))
            .find_map(|(index, entry)| self.marked_paths.contains(&entry.path).then_some(index))
        else {
            return;
        };
        self.select_index(index, cx);
    }

    fn select_previous_marked(
        &mut self,
        _: &SelectPreviousMarked,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .entries
            .iter()
            .enumerate()
            .take(self.selected_index)
            .rev()
            .find_map(|(index, entry)| self.marked_paths.contains(&entry.path).then_some(index))
        else {
            return;
        };
        self.select_index(index, cx);
    }

    fn operation_targets(&self) -> Vec<PathBuf> {
        let marked_entries = self
            .all_entries
            .iter()
            .filter(|entry| !entry.is_parent && self.marked_paths.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        if !marked_entries.is_empty() {
            return marked_entries;
        }

        self.selected_entry()
            .filter(|entry| !entry.is_parent)
            .map(|entry| vec![entry.path])
            .unwrap_or_default()
    }

    fn begin_entry_edit(
        &mut self,
        kind: EntryEditKind,
        initial_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.operation_in_progress {
            return;
        }
        self.entry_edit = Some(kind);
        self.path_editor_text = initial_text.clone();
        self.path_completion = None;
        self.resolved_path = None;
        self.path_error = None;
        self.operation_error = None;
        self.path_editor.update(cx, |editor, cx| {
            editor.set_text(initial_text, window, cx);
            editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    fn create_directory(
        &mut self,
        _: &CreateDirectory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_entry_edit(
            EntryEditKind::CreateDirectory,
            path_text(&self.current_path, true),
            window,
            cx,
        );
    }

    fn rename_selected(&mut self, _: &RenameSelected, window: &mut Window, cx: &mut Context<Self>) {
        let sources = self.operation_targets();
        if sources.is_empty() {
            return;
        }
        let initial_text = if let [source] = sources.as_slice() {
            source
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        self.begin_entry_edit(EntryEditKind::Rename { sources }, initial_text, window, cx);
    }

    fn copy_selected(&mut self, _: &CopySelected, window: &mut Window, cx: &mut Context<Self>) {
        let sources = self.operation_targets();
        if sources.is_empty() {
            return;
        }
        self.begin_entry_edit(
            EntryEditKind::Copy { sources },
            path_text(&self.current_path, true),
            window,
            cx,
        );
    }

    fn cancel_entry_edit(
        &mut self,
        _: &CancelEntryEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.operation_in_progress || self.entry_edit.take().is_none() {
            return;
        }
        self.set_path_text(self.current_path.clone(), true, window, cx);
        self.filter_query.clear();
        let selected_path = self.selected_entry().map(|entry| entry.path);
        self.update_visible_entries(selected_path.as_deref());
        self.item_focus_handle.focus(window, cx);
        cx.notify();
    }

    fn confirm_entry_edit(
        &mut self,
        _: &ConfirmEntryEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.operation_in_progress {
            return;
        }
        let Some(kind) = self.entry_edit.clone() else {
            return;
        };
        let input = self.path_editor.read(cx).text(cx);
        let Some(path) = resolve_path_input(&input, &self.current_path) else {
            self.path_error = Some(kind.missing_input_message().into());
            cx.notify();
            return;
        };

        let fs = self.fs.clone();
        self.operation_in_progress = true;
        self.path_error = None;
        cx.notify();
        self._operation_task = cx.spawn_in(window, async move |this, cx| {
            let result = match &kind {
                EntryEditKind::CreateDirectory => match fs
                    .metadata(&path)
                    .await
                    .with_context(|| format!("inspecting {}", path.display()))
                {
                    Ok(Some(_)) => Err(anyhow::anyhow!("{} already exists", path.display())),
                    Ok(None) => fs.create_dir(&path).await.map(|_| vec![path.clone()]),
                    Err(error) => Err(error),
                },
                EntryEditKind::Rename { sources } => {
                    rename_entries(fs.as_ref(), sources, &path).await
                }
                EntryEditKind::Copy { sources } => copy_entries(fs.as_ref(), sources, &path).await,
            };

            this.update_in(cx, |this, window, cx| {
                this.operation_in_progress = false;
                match result {
                    Ok(affected_paths) => {
                        if let EntryEditKind::Rename { sources } = &kind {
                            for source in sources {
                                this.marked_paths.remove(source);
                                this.flagged_paths.remove(source);
                            }
                        }
                        this.entry_edit = None;
                        let path_to_select = affected_paths
                            .into_iter()
                            .find(|path| path.parent() == Some(this.current_path.as_path()));
                        this.load_directory(this.current_path.clone(), path_to_select, window, cx);
                        this.item_focus_handle.focus(window, cx);
                    }
                    Err(error) => {
                        this.path_error = Some(
                            format!("Failed to {}: {error:#}", kind.failure_description()).into(),
                        );
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
    }

    fn trash_selected(&mut self, _: &TrashSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.trash_paths(self.operation_targets(), window, cx);
    }

    fn flag_for_deletion(&mut self, _: &FlagForDeletion, _: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().filter(|entry| !entry.is_parent) else {
            return;
        };
        self.marked_paths.remove(&entry.path);
        self.flagged_paths.insert(entry.path);
        self.select_index(self.selected_index.saturating_add(1), cx);
    }

    fn trash_flagged(&mut self, _: &TrashFlagged, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self
            .all_entries
            .iter()
            .filter(|entry| !entry.is_parent && self.flagged_paths.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect();
        self.trash_paths(targets, window, cx);
    }

    fn trash_paths(&mut self, targets: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation_in_progress {
            return;
        }
        if targets.is_empty() {
            return;
        }

        let prompt = if let [target] = targets.as_slice() {
            format!("Move {} to the trash?", target.display())
        } else {
            format!("Move {} entries to the trash?", targets.len())
        };
        let details = targets
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let answer = window.prompt(
            PromptLevel::Info,
            &prompt,
            Some(&details),
            &["Trash", "Cancel"],
            cx,
        );
        let fs = self.fs.clone();
        self.operation_in_progress = true;
        self.operation_error = None;
        cx.notify();
        self._operation_task = cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                this.update(cx, |this, cx| {
                    this.operation_in_progress = false;
                    cx.notify();
                })
                .log_err();
                return;
            }

            let mut trashed_entries = Vec::new();
            let mut trashed_paths = Vec::new();
            let mut failures = Vec::new();
            for path in targets {
                match fs
                    .trash(
                        &path,
                        RemoveOptions {
                            recursive: true,
                            ignore_if_not_exists: false,
                        },
                    )
                    .await
                {
                    Ok(trash_id) => {
                        trashed_entries.push(trash_id);
                        trashed_paths.push(path);
                    }
                    Err(error) => failures.push(format!("{}: {error:#}", path.display())),
                }
            }

            this.update_in(cx, |this, window, cx| {
                this.operation_in_progress = false;
                for path in &trashed_paths {
                    this.marked_paths.remove(path);
                    this.flagged_paths.remove(path);
                }
                if !trashed_entries.is_empty() {
                    this.trash_history.push(trashed_entries);
                }
                this.operation_error = (!failures.is_empty())
                    .then(|| format!("Failed to trash:\n{}", failures.join("\n")).into());
                this.load_directory(this.current_path.clone(), None, window, cx);
            })
            .log_err();
        });
    }

    fn undo_trash(&mut self, _: &UndoTrash, window: &mut Window, cx: &mut Context<Self>) {
        if self.operation_in_progress {
            return;
        }
        let Some(trashed_entries) = self.trash_history.pop() else {
            return;
        };

        let fs = self.fs.clone();
        self.operation_in_progress = true;
        self.operation_error = None;
        cx.notify();
        self._operation_task = cx.spawn_in(window, async move |this, cx| {
            let mut restored_paths = Vec::new();
            let mut failed_entries = Vec::new();
            let mut failures = Vec::new();
            for trash_id in trashed_entries {
                match fs.restore(trash_id).await {
                    Ok(path) => restored_paths.push(path),
                    Err(error) => {
                        failed_entries.push(trash_id);
                        failures.push(format!("{error:#}"));
                    }
                }
            }

            this.update_in(cx, |this, window, cx| {
                this.operation_in_progress = false;
                if !failed_entries.is_empty() {
                    this.trash_history.push(failed_entries);
                }
                this.operation_error = (!failures.is_empty()).then(|| {
                    format!("Failed to restore from trash:\n{}", failures.join("\n")).into()
                });
                let path_to_select = restored_paths
                    .into_iter()
                    .find(|path| path.parent() == Some(this.current_path.as_path()));
                this.load_directory(this.current_path.clone(), path_to_select, window, cx);
            })
            .log_err();
        });
    }

    fn update_visible_entries(&mut self, preferred_path: Option<&Path>) {
        self.entries = if self.filter_query.is_empty() {
            self.all_entries.clone()
        } else {
            let candidates = self
                .all_entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.is_parent)
                .map(|(index, entry)| {
                    fuzzy_nucleo::StringMatchCandidate::new(
                        index,
                        entry
                            .file_name()
                            .trim_end_matches(std::path::MAIN_SEPARATOR),
                    )
                })
                .collect::<Vec<_>>();
            let mut matches = fuzzy_nucleo::match_strings(
                &candidates,
                &self.filter_query,
                fuzzy_nucleo::Case::Ignore,
                fuzzy_nucleo::LengthPenalty::On,
                candidates.len(),
            );
            matches.sort_by_key(|matched| {
                !matched
                    .string
                    .to_lowercase()
                    .starts_with(&self.filter_query)
            });
            matches
                .into_iter()
                .filter_map(|matched| self.all_entries.get(matched.candidate_id).cloned())
                .collect()
        };
        if self.is_prompt {
            self.entries.retain(|entry| !entry.is_parent);
        }
        self.selected_index = preferred_path
            .and_then(|path| self.entries.iter().position(|entry| entry.path == path))
            .unwrap_or_else(|| Self::default_selected_index(&self.entries));
        self.scroll_handle
            .scroll_to_item(self.selected_index, ScrollStrategy::Nearest);
    }

    fn load_directory(
        &mut self,
        path: PathBuf,
        path_to_select: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._path_task = Task::ready(());
        self.current_path = path.clone();
        cx.emit(());
        self.filter_query.clear();
        self.set_path_text(path.clone(), true, window, cx);
        self.input_selected = self.is_prompt;
        self.load_entries(path, path_to_select, cx);
    }

    fn update_prompt_input(&mut self, input: String, cx: &mut Context<Self>) {
        self._path_task = Task::ready(());
        let Some(path) = self.resolve_prompt_input(&input) else {
            self.entries.clear();
            self.input_selected = true;
            cx.notify();
            return;
        };
        let (directory, query) = if input.ends_with(std::path::MAIN_SEPARATOR) {
            (path, String::new())
        } else {
            (
                path.parent().unwrap_or(&path).to_path_buf(),
                path.file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
                    .unwrap_or_default(),
            )
        };
        self.filter_query = query;
        self.input_selected = self.filter_query.is_empty();
        if directory != self.current_path {
            self.current_path = directory.clone();
            self.load_entries(directory, None, cx);
        } else {
            self.update_visible_entries(None);
        }
        cx.notify();
    }

    fn resolve_prompt_input(&self, input: &str) -> Option<PathBuf> {
        let directory = self
            .prompt_origin
            .as_ref()
            .filter(|_| self.is_prompt)
            .map_or(self.current_path.as_path(), |origin| {
                origin.directory.as_path()
            });
        resolve_path_input(input, directory)
    }

    fn load_entries(
        &mut self,
        path: PathBuf,
        path_to_select: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.all_entries.clear();
        self.entries.clear();
        self.selected_index = 0;
        self.loading = true;
        self.load_error = None;
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();

        let fs = self.fs.clone();
        self._load_task = cx.spawn(async move |this, cx| {
            let result = read_directory(fs.as_ref(), &path).await;
            this.update(cx, |this, cx| {
                if this.current_path != path {
                    return;
                }

                this.loading = false;
                match result {
                    Ok(entries) => {
                        this.marked_paths.retain(|marked| {
                            marked.parent() != Some(path.as_path())
                                || entries.iter().any(|entry| &entry.path == marked)
                        });
                        this.flagged_paths.retain(|flagged| {
                            flagged.parent() != Some(path.as_path())
                                || entries.iter().any(|entry| &entry.path == flagged)
                        });
                        this.all_entries = entries;
                        this.update_visible_entries(path_to_select.as_deref());
                    }
                    Err(error) => {
                        this.load_error =
                            Some(format!("Failed to read directory: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn navigate_to_directory(
        &mut self,
        path: PathBuf,
        path_to_select: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.entry_edit.is_some() || self.operation_in_progress {
            return;
        }
        if path != self.current_path {
            let selected_path = self.selected_entry().map(|entry| entry.path);
            if let Some(current_entry) = self.history.get_mut(self.history_index)
                && current_entry.path == self.current_path
            {
                current_entry.selected_path = selected_path;
            }
            self.history.truncate(self.history_index.saturating_add(1));
            self.history.push(DirectoryHistoryEntry {
                path: path.clone(),
                selected_path: path_to_select.clone(),
            });
            self.history_index = self.history.len().saturating_sub(1);
        }
        self.load_directory(path, path_to_select, window, cx);
    }

    fn history_back(&mut self, _: &HistoryBack, window: &mut Window, cx: &mut Context<Self>) {
        if self.entry_edit.is_some() || self.operation_in_progress || self.history_index == 0 {
            return;
        }
        self.save_current_history_selection();
        self.history_index = self.history_index.saturating_sub(1);
        let Some(entry) = self.history.get(self.history_index).cloned() else {
            return;
        };
        self.load_directory(entry.path, entry.selected_path, window, cx);
    }

    fn history_forward(&mut self, _: &HistoryForward, window: &mut Window, cx: &mut Context<Self>) {
        if self.entry_edit.is_some()
            || self.operation_in_progress
            || self.history_index.saturating_add(1) >= self.history.len()
        {
            return;
        }
        self.save_current_history_selection();
        self.history_index = self.history_index.saturating_add(1);
        let Some(entry) = self.history.get(self.history_index).cloned() else {
            return;
        };
        self.load_directory(entry.path, entry.selected_path, window, cx);
    }

    fn save_current_history_selection(&mut self) {
        let selected_path = self.selected_entry().map(|entry| entry.path);
        if let Some(entry) = self.history.get_mut(self.history_index)
            && entry.path == self.current_path
        {
            entry.selected_path = selected_path;
        }
    }

    fn open_selected(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let behavior = match self.selected_entry() {
            Some(entry) if self.mode == DirectoryViewMode::Dired && entry.is_directory => {
                OpenBehavior::Navigate
            }
            Some(_) => OpenBehavior::Open,
            None => return,
        };
        self.activate_selected(behavior, window, cx);
    }

    fn navigate_selected(
        &mut self,
        _: &NavigateSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_selected(OpenBehavior::Navigate, window, cx);
    }

    fn activate_selected(
        &mut self,
        behavior: OpenBehavior,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.entry_edit.is_some() || self.operation_in_progress {
            return;
        }
        let Some(entry) = self.selected_entry() else {
            return;
        };

        if !entry.is_directory {
            self.open_file(entry.path, behavior, window, cx);
            return;
        }

        if entry.is_parent {
            let previous_path = self.current_path.clone();
            self.navigate_to_directory(entry.path, Some(previous_path), window, cx);
            if behavior == OpenBehavior::Open {
                self.enter_dired(true, window, cx);
            }
            return;
        }

        self.open_directory(entry.path, behavior, window, cx);
    }

    fn open_directory(
        &mut self,
        path: PathBuf,
        behavior: OpenBehavior,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.entry_edit.is_some() || self.operation_in_progress {
            return;
        }
        self.navigate_to_directory(path, None, window, cx);
        if self.is_prompt && behavior == OpenBehavior::Open {
            self._preview_task = Task::ready(());
            self.committing_prompt = true;
            self.is_prompt = false;
            self.input_selected = false;
            self.mode = DirectoryViewMode::Dired;
            let view = cx.entity();
            let workspace = self.workspace.clone();
            cx.spawn_in(window, async move |_, cx| {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.hide_modal(window, cx);
                        let path = view.read(cx).current_path.clone();
                        let existing = workspace
                            .active_pane()
                            .read(cx)
                            .items_of_type::<ProjectDirectoryView>()
                            .find(|existing| {
                                existing.read(cx).mode == DirectoryViewMode::Dired
                                    && existing.read(cx).current_path == path
                            });
                        let view = existing.unwrap_or(view);
                        workspace.display_item(
                            Box::new(view),
                            workspace::ItemPlacement::ActivePane,
                            window,
                            cx,
                        );
                    })
                    .log_err();
            })
            .detach();
            return;
        }
        if behavior == OpenBehavior::Open {
            self.enter_dired(true, window, cx);
        }
    }

    fn open_project(&mut self, _: &OpenProject, window: &mut Window, cx: &mut Context<Self>) {
        if self.entry_edit.is_some() || self.operation_in_progress {
            return;
        }
        let path = self
            .selected_entry()
            .filter(|entry| entry.is_directory && !entry.is_parent)
            .map(|entry| entry.path)
            .unwrap_or_else(|| self.current_path.clone());
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            // Opening a workspace checks every item's dirty state, including this view.
            let workspace = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_fresh_workspace_for_paths(
                        OpenMode::Activate,
                        vec![path],
                        window,
                        cx,
                    )
                })?
                .await?;
            workspace.update_in(cx, |workspace, window, cx| {
                if workspace.active_item(cx).is_none() {
                    crate::display_directory(
                        workspace,
                        workspace::ItemPlacement::ActivePane,
                        DirectoryViewMode::Dired,
                        window,
                        cx,
                    );
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to open project", window, cx, |_, _, _| None);
    }

    fn confirm_path(&mut self, _: &ConfirmPath, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.path_editor.read(cx).text(cx);
        let resolved_input = self.resolve_prompt_input(&input);
        let selected_path = resolved_input
            .as_ref()
            .is_some_and(|path| {
                path == &self.current_path || path.parent() == Some(self.current_path.as_path())
            })
            .then(|| self.selected_entry().map(|entry| entry.path))
            .flatten();
        let resolved_input = if self.is_prompt {
            selected_path.clone().or(resolved_input)
        } else {
            resolved_input
        };
        let Some(path) = resolved_input else {
            self.path_error = Some("Enter a path".into());
            cx.notify();
            return;
        };

        self.path_error = None;
        let fs = self.fs.clone();
        let current_path = self.current_path.clone();
        self._path_task = cx.spawn_in(window, async move |this, cx| {
            let mut path = path;
            let mut metadata = fs.metadata(&path).await;
            if (path == current_path || matches!(metadata, Ok(None)))
                && let Some(selected_path) = selected_path
                && selected_path != path
            {
                path = selected_path;
                metadata = fs.metadata(&path).await;
            }
            this.update_in(cx, |this, window, cx| {
                if this.path_editor.read(cx).text(cx) != input {
                    return;
                }

                match metadata {
                    Ok(Some(metadata)) if metadata.is_dir => {
                        this.open_directory(path, OpenBehavior::Open, window, cx);
                    }
                    Ok(Some(_)) | Ok(None) => {
                        this.set_path_text(path.clone(), false, window, cx);
                        this.open_file(path, OpenBehavior::Open, window, cx);
                    }
                    Err(error) => {
                        this.path_error =
                            Some(format!("Failed to inspect {}: {error:#}", path.display()).into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
    }

    fn confirm_input(&mut self, _: &ConfirmInput, window: &mut Window, cx: &mut Context<Self>) {
        self.input_selected = true;
        self.confirm_path(&ConfirmPath, window, cx);
    }

    fn cancel_prompt(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_prompt {
            self._path_task = Task::ready(());
            cx.emit(DismissEvent);
        } else {
            cx.propagate();
        }
    }

    fn complete_path(&mut self, _: &CompletePath, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.path_editor.read(cx).text(cx);
        if let Some(completion) = &mut self.path_completion
            && completion
                .candidates
                .get(completion.selected_index)
                .is_some_and(|candidate| candidate.text == input)
            && completion.candidates.len() > 1
        {
            completion.selected_index =
                (completion.selected_index + 1) % completion.candidates.len();
            let candidate = completion.candidates[completion.selected_index].clone();
            self.set_completed_path(candidate, window, cx);
            return;
        }

        let resolved_input = self.resolve_prompt_input(&input);
        let input_uses_visible_entries = resolved_input.as_ref().is_some_and(|path| {
            path == &self.current_path || path.parent() == Some(self.current_path.as_path())
        });
        let candidates = input_uses_visible_entries
            .then(|| {
                self.entries
                    .iter()
                    .filter(|entry| !entry.is_parent)
                    .map(|entry| PathCompletionCandidate {
                        path: entry.path.clone(),
                        text: path_text_for_input(&entry.path, entry.is_directory, &input),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !candidates.is_empty() {
            let selected_path = self.selected_entry().map(|entry| entry.path);
            let selected_index = selected_path
                .as_ref()
                .and_then(|path| {
                    candidates
                        .iter()
                        .position(|candidate| &candidate.path == path)
                })
                .unwrap_or(0);
            let candidate = candidates[selected_index].clone();
            self.path_completion = Some(PathCompletion {
                candidates,
                selected_index,
            });
            self.set_completed_path(candidate, window, cx);
            return;
        }

        let Some(path) = resolved_input else {
            self.path_error = Some("Enter a path to complete".into());
            cx.notify();
            return;
        };

        self.path_error = None;
        let fs = self.fs.clone();
        self._path_task = cx.spawn_in(window, async move |this, cx| {
            let result = completion_candidates(fs.as_ref(), &path, &input).await;
            this.update_in(cx, |this, window, cx| {
                if this.path_editor.read(cx).text(cx) != input {
                    return;
                }

                match result {
                    Ok(candidates) if candidates.is_empty() => {
                        this.path_completion = None;
                        this.path_error = Some("No completion".into());
                        cx.notify();
                    }
                    Ok(candidates) => {
                        let candidate = candidates[0].clone();
                        this.path_completion = Some(PathCompletion {
                            candidates,
                            selected_index: 0,
                        });
                        this.set_completed_path(candidate, window, cx);
                    }
                    Err(error) => {
                        this.path_completion = None;
                        this.path_error =
                            Some(format!("Failed to complete path: {error:#}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
    }

    fn path_backspace(&mut self, _: &PathBackspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_prompt {
            if !self.delete_prompt_component(true, window, cx) {
                self.path_editor
                    .update(cx, |editor, cx| editor.backspace(&Backspace, window, cx));
            }
            return;
        }
        let input = self.path_editor.read(cx).text(cx);
        let resolved_path = self
            .resolved_path
            .as_ref()
            .filter(|resolved| resolved.text == input)
            .map(|resolved| resolved.path.clone())
            .or_else(|| {
                (input == path_text(&self.current_path, true)).then(|| self.current_path.clone())
            });
        if let Some(resolved_path) = resolved_path {
            if let Some(parent_path) = resolved_path.parent().map(Path::to_path_buf) {
                let path_to_select = Some(resolved_path);
                self.navigate_to_directory(parent_path, path_to_select, window, cx);
                self.path_editor.read(cx).focus_handle(cx).focus(window, cx);
            }
            return;
        }

        self.resolved_path = None;
        self.path_completion = None;
        self.path_error = None;
        self.path_editor.update(cx, |editor, cx| {
            editor.backspace(&Backspace, window, cx);
        });
    }

    fn open_file(
        &mut self,
        path: PathBuf,
        behavior: OpenBehavior,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode == DirectoryViewMode::Dired && behavior == OpenBehavior::Navigate {
            self.preview_file_in_other_pane(path, window, cx);
            return;
        }
        if self.is_prompt {
            if behavior == OpenBehavior::Navigate {
                self.preview_file(path, window, cx);
                return;
            }
            self._preview_task = Task::ready(());
            self.committing_prompt = true;
            let workspace = self.workspace.clone();
            cx.spawn_in(window, async move |_, cx| {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.hide_modal(window, cx);
                        workspace.open_abs_path(
                            path,
                            OpenOptions {
                                visible: Some(OpenVisible::None),
                                ..OpenOptions::default()
                            },
                            window,
                            cx,
                        )
                    })?
                    .await?;
                anyhow::Ok(())
            })
            .detach_and_prompt_err("Failed to open file", window, cx, |_, _, _| None);
            return;
        }
        if behavior == OpenBehavior::Open && self.mode == DirectoryViewMode::FindFile {
            self.maximize(window, cx);
        }
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(
                        path,
                        OpenOptions {
                            visible: Some(OpenVisible::None),
                            ..OpenOptions::default()
                        },
                        window,
                        cx,
                    )
                    .detach_and_prompt_err("Failed to open file", window, cx, |_, _, _| None);
            })
            .log_err();
    }

    fn preview_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(origin) = &self.prompt_origin else {
            return;
        };
        let pane = origin.pane.clone();
        if let Some(preview) = &origin.preview_item {
            pane.update(cx, |pane, _| {
                // A cancellable preview must not replace a tab that predates the prompt.
                pane.unpreview_item_if_preview(preview.item_id());
            })
            .log_err();
        }
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        self._preview_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                // Hidden worktrees are weakly held by the project until a buffer uses them.
                let (_worktree, path) = cx
                    .update(|_, cx| Workspace::project_path_for_path(project, &path, false, cx))?
                    .await?;
                let item = workspace
                    .update_in(cx, |workspace, window, cx| {
                        workspace.open_path_preview(path, Some(pane), false, true, true, window, cx)
                    })?
                    .await?;
                anyhow::Ok(item)
            }
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(item) => {
                        if let Some(origin) = &mut this.prompt_origin {
                            origin.last_preview = Some(item.item_id());
                            if !origin.item_ids.contains(&item.item_id()) {
                                origin.previews.push(item);
                            }
                        }
                    }
                    Err(error) => {
                        this.path_error = Some(format!("Failed to preview file: {error:#}").into())
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn preview_file_in_other_pane(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let item_id = cx.entity_id();
        self.operation_error = None;
        self._preview_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                // Keep an external file's hidden worktree alive through opening its buffer.
                let (_worktree, path) = cx
                    .update(|_, cx| Workspace::project_path_for_path(project, &path, false, cx))?
                    .await?;
                let (task, pane) = workspace.update_in(cx, |workspace, window, cx| {
                    let origin = workspace
                        .pane_for_item_id(item_id)
                        .context("Directory buffer is no longer displayed")?;
                    if workspace.is_pane_maximized() && workspace.active_pane() == &origin {
                        workspace.toggle_editor_zoom(&workspace::ToggleEditorZoom, window, cx);
                    }
                    let focus = window.focused(cx);
                    let pane = workspace.adjacent_pane_of(&origin, window, cx);
                    if let Some(focus) = focus {
                        focus.focus(window, cx);
                    }
                    let task = workspace.open_path_preview(
                        path,
                        Some(pane.downgrade()),
                        false,
                        true,
                        false,
                        window,
                        cx,
                    );
                    anyhow::Ok((task, pane))
                })??;
                let item = task.await?;
                pane.update_in(cx, |pane, window, cx| {
                    if let Some(index) = pane.index_for_item(item.as_ref()) {
                        pane.activate_item(index, false, false, window, cx);
                    }
                })?;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.operation_error =
                        Some(format!("Failed to preview file: {error:#}").into());
                    cx.notify();
                })
                .log_err();
            }
        });
    }

    fn delete_prompt_component(
        &mut self,
        require_separator: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.path_editor.update(cx, |editor, cx| {
            let selection = editor
                .selections
                .newest::<language::Point>(&editor.display_snapshot(cx));
            if !selection.is_empty() {
                return false;
            }
            let mut cursor = selection.head().column as usize;
            let mut text = editor.text(cx);
            if text.get(..cursor) == Some("~/") {
                let mut expanded = paths::home_dir().to_string_lossy().into_owned();
                if !expanded.ends_with(std::path::MAIN_SEPARATOR) {
                    expanded.push(std::path::MAIN_SEPARATOR);
                }
                text = format!("{}{}", expanded, text.get(cursor..).unwrap_or_default());
                cursor = expanded.len();
                editor.set_text(text.clone(), window, cx);
            }
            let Some(prefix) = text.get(..cursor) else {
                return false;
            };
            if prefix.is_empty()
                || (require_separator && !prefix.ends_with(std::path::MAIN_SEPARATOR))
            {
                return false;
            }
            let prefix = prefix
                .strip_suffix(std::path::MAIN_SEPARATOR)
                .unwrap_or(prefix);
            let Some(separator) = prefix.rfind(std::path::MAIN_SEPARATOR) else {
                return false;
            };
            let start = separator + std::path::MAIN_SEPARATOR.len_utf8();
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections
                    .select_ranges([language::Point::new(0, start as u32)
                        ..language::Point::new(0, cursor as u32)])
            });
            editor.insert("", window, cx);
            true
        })
    }

    fn maximize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let directory_view = cx.entity();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.maximize_item_pane(&directory_view, window, cx);
            })
            .log_err();
    }

    fn enter_dired(&mut self, maximize: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = DirectoryViewMode::Dired;
        self.filter_query.clear();
        let selected_path = self.selected_entry().map(|entry| entry.path);
        self.update_visible_entries(selected_path.as_deref());
        self.item_focus_handle.focus(window, cx);
        if maximize {
            self.maximize(window, cx);
        }
        cx.notify();
    }

    fn go_up(&mut self, _: &GoUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_prompt {
            self.delete_prompt_component(false, window, cx);
            return;
        }
        let Some(parent_path) = self.current_path.parent().map(Path::to_path_buf) else {
            return;
        };
        let previous_path = self.current_path.clone();
        self.navigate_to_directory(parent_path, Some(previous_path), window, cx);
    }

    fn refresh(&mut self, _: &Refresh, window: &mut Window, cx: &mut Context<Self>) {
        if self.entry_edit.is_some() || self.operation_in_progress {
            return;
        }
        let selected_path = self.selected_entry().map(|entry| entry.path);
        self.load_directory(self.current_path.clone(), selected_path, window, cx);
    }

    fn render_entries(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|index| {
                let entry = self.entries.get(index)?;
                let selected =
                    index == self.selected_index && !(self.is_prompt && self.input_selected);
                let icon = if entry.is_directory {
                    IconName::Folder
                } else {
                    IconName::File
                };
                let name = entry.file_name();
                let details = entry.details();
                let is_directory = entry.is_directory;
                let is_marked = self.marked_paths.contains(&entry.path);
                let is_flagged = self.flagged_paths.contains(&entry.path);

                Some(
                    ListItem::new(index)
                        .spacing(ListItemSpacing::ExtraDense)
                        .toggle_state(selected)
                        .aria_label(name.clone())
                        .start_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new(if is_flagged {
                                        "D"
                                    } else if is_marked {
                                        "*"
                                    } else {
                                        " "
                                    })
                                    .size(LabelSize::Small)
                                    .color(Color::Accent),
                                )
                                .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted)),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div().w(rems(5.)).flex_none().text_right().child(
                                        Label::new(details)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                                )
                                .child(
                                    ui::LabelLike::new().buffer_font(cx).child(
                                        gpui::StyledText::new(name.clone()).with_highlights(
                                            (selected
                                                && self.mode == DirectoryViewMode::Dired
                                                && self.entry_edit.is_none())
                                            .then(|| {
                                                (
                                                    0..name
                                                        .chars()
                                                        .next()
                                                        .map(char::len_utf8)
                                                        .unwrap_or(0),
                                                    gpui::HighlightStyle {
                                                        color: Some(
                                                            cx.theme().colors().editor_background,
                                                        ),
                                                        background_color: Some(
                                                            cx.theme().players().local().cursor,
                                                        ),
                                                        ..Default::default()
                                                    },
                                                )
                                            }),
                                        ),
                                    ),
                                ),
                        )
                        .on_click(
                            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                this.select_index(index, cx);
                                if event.click_count() > 1 {
                                    this.open_selected(&Confirm, window, cx);
                                }
                                if this.mode == DirectoryViewMode::FindFile
                                    || this.entry_edit.is_some()
                                {
                                    this.path_editor.read(cx).focus_handle(cx).focus(window, cx);
                                } else if event.click_count() == 1 || is_directory {
                                    this.item_focus_handle.focus(window, cx);
                                }
                            }),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn tab_name(&self) -> SharedString {
        self.current_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.current_path.to_string_lossy().into_owned())
            .into()
    }

    fn render_path_editor(&self, cx: &App) -> EditorElement {
        let font = theme::theme_settings(cx).buffer_font(cx).clone();
        EditorElement::new(
            &self.path_editor,
            EditorStyle {
                background: cx.theme().system().transparent,
                local_player: cx.theme().players().local(),
                syntax: cx.theme().syntax().clone(),
                text: gpui::TextStyle {
                    font_family: font.family,
                    font_features: font.features,
                    font_fallbacks: font.fallbacks,
                    font_weight: font.weight,
                    font_size: rems(0.875).into(),
                    line_height: relative(1.2),
                    color: cx.theme().colors().text,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
    }
}

impl Render for ProjectDirectoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entry_count = self.entries.len();
        v_flex()
            .key_context(if self.is_prompt {
                "ProjectDirectory ProjectDirectoryPrompt TransientSurface"
            } else if self.mode == DirectoryViewMode::Dired && self.entry_edit.is_none() {
                "ProjectDirectory ProjectDirectoryDired"
            } else {
                "ProjectDirectory"
            })
            .track_focus(&self.item_focus_handle)
            .size_full()
            .when(self.is_prompt, |this| {
                this.h(window.viewport_size().height * 0.33)
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .occlude()
            })
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::navigate_selected))
            .on_action(cx.listener(Self::open_project))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::confirm_path))
            .on_action(cx.listener(Self::confirm_input))
            .on_action(cx.listener(Self::cancel_prompt))
            .on_action(cx.listener(Self::complete_path))
            .on_action(cx.listener(Self::path_backspace))
            .on_action(cx.listener(Self::history_back))
            .on_action(cx.listener(Self::history_forward))
            .on_action(cx.listener(Self::mark_selected))
            .on_action(cx.listener(Self::unmark_selected))
            .on_action(cx.listener(Self::clear_marks))
            .on_action(cx.listener(Self::toggle_marks))
            .on_action(cx.listener(Self::select_next_marked))
            .on_action(cx.listener(Self::select_previous_marked))
            .on_action(cx.listener(Self::create_directory))
            .on_action(cx.listener(Self::copy_selected))
            .on_action(cx.listener(Self::rename_selected))
            .on_action(cx.listener(Self::confirm_entry_edit))
            .on_action(cx.listener(Self::cancel_entry_edit))
            .on_action(cx.listener(Self::trash_selected))
            .on_action(cx.listener(Self::flag_for_deletion))
            .on_action(cx.listener(Self::trash_flagged))
            .on_action(cx.listener(Self::undo_trash))
            .on_action(cx.listener(Self::refresh))
            .when(
                self.mode == DirectoryViewMode::FindFile && self.entry_edit.is_none(),
                |this| {
                    this.child(
                        h_flex()
                            .key_context("ProjectDirectoryPath")
                            .h_8()
                            .px_3()
                            .gap_2()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .when(self.is_prompt && self.input_selected, |this| {
                                this.bg(cx.theme().colors().element_selected)
                            })
                            .child(
                                Label::new("Find file")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1().min_w_0().child(self.render_path_editor(cx)))
                            .when_some(self.path_error.clone(), |this, error| {
                                this.child(Label::new(error).color(Color::Error))
                            }),
                    )
                },
            )
            .when(self.mode == DirectoryViewMode::Dired, |this| {
                this.child(
                    h_flex()
                        .h_8()
                        .px_3()
                        .gap_2()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(Icon::new(IconName::Folder).size(IconSize::Small))
                        .child(Label::new(path_text(&self.current_path, true))),
                )
            })
            .when(self.loading, |this| {
                this.child(div().px_3().py_2().child(Label::new("Loading…")))
            })
            .when_some(self.load_error.clone(), |this, error| {
                this.child(div().px_3().py_2().child(Label::new(error)))
            })
            .when_some(self.operation_error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .child(Label::new(error).color(Color::Error)),
                )
            })
            .child(
                uniform_list(
                    "project-directory-entries",
                    entry_count,
                    cx.processor(Self::render_entries),
                )
                .size_full()
                .track_scroll(&self.scroll_handle),
            )
            .when_some(self.entry_edit.as_ref(), |this, entry_edit| {
                this.child(
                    h_flex()
                        .key_context("ProjectDirectoryEntryEdit")
                        .h_8()
                        .px_3()
                        .gap_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new(entry_edit.label())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1().min_w_0().child(self.render_path_editor(cx)))
                        .when_some(self.path_error.clone(), |this, error| {
                            this.child(Label::new(error).color(Color::Error))
                        }),
                )
            })
            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
    }
}

impl Focusable for ProjectDirectoryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.item_focus_handle.clone()
    }
}

impl EventEmitter<()> for ProjectDirectoryView {}
impl EventEmitter<DismissEvent> for ProjectDirectoryView {}

impl workspace::ModalView for ProjectDirectoryView {
    fn render_bare(&self) -> bool {
        true
    }

    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        self._path_task = Task::ready(());
        self._preview_task = Task::ready(());
        let origin = self.prompt_origin.take();
        if !self.committing_prompt
            && let Some(origin) = origin
        {
            cx.spawn_in(window, async move |_, cx| {
                origin
                    .pane
                    .update_in(cx, |pane, window, cx| {
                        let restore_active = pane
                            .active_item()
                            .is_some_and(|item| Some(item.item_id()) == origin.last_preview);
                        for preview in origin.previews {
                            if !preview.is_dirty(cx) {
                                pane.remove_item(preview.item_id(), false, false, window, cx);
                            }
                        }
                        if let Some(preview) = origin.preview_item
                            && pane.index_for_item(preview.as_ref()).is_some()
                            && !preview.is_dirty(cx)
                            && pane.preview_item_id().is_none()
                        {
                            pane.replace_preview_item_id(preview.item_id(), window, cx);
                        }
                        if restore_active
                            && let Some(item) = origin.active_item
                            && let Some(index) = pane.index_for_item(item.as_ref())
                        {
                            pane.activate_item(index, true, true, window, cx);
                        }
                    })
                    .log_err();
            })
            .detach();
        }
        workspace::DismissDecision::Dismiss(true)
    }
}

impl Item for ProjectDirectoryView {
    type Event = ();

    fn to_item_events(_: &(), emit: &mut dyn FnMut(ItemEvent)) {
        emit(ItemEvent::UpdateTab);
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.tab_name()
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.project
            .read(cx)
            .find_project_path(&self.current_path, cx)
    }

    fn show_toolbar(&self) -> bool {
        false
    }
}

async fn read_directory(fs: &dyn Fs, path: &Path) -> Result<Vec<DirectoryEntry>> {
    let mut children = fs
        .read_dir(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let mut entries = Vec::new();
    while let Some(child_path) = children.next().await {
        let child_path = child_path.with_context(|| format!("reading {}", path.display()))?;
        let metadata = fs
            .metadata(&child_path)
            .await
            .with_context(|| format!("reading metadata for {}", child_path.display()))?;
        let Some(metadata) = metadata else {
            if fs.read_link(&child_path).await.is_ok() {
                entries.push(DirectoryEntry {
                    path: child_path,
                    is_directory: false,
                    is_parent: false,
                    is_symlink: true,
                    length: 0,
                });
            }
            continue;
        };
        entries.push(DirectoryEntry {
            path: child_path,
            is_directory: metadata.is_dir,
            is_parent: false,
            is_symlink: metadata.is_symlink,
            length: metadata.len,
        });
    }
    entries.sort_by(
        |left, right| match (left.is_directory, right.is_directory) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => paths::natural_sort(&left.file_name(), &right.file_name()),
        },
    );
    if let Some(parent_path) = path.parent() {
        entries.insert(0, DirectoryEntry::parent(parent_path.to_path_buf()));
    }
    Ok(entries)
}

async fn entry_destinations(
    fs: &dyn Fs,
    sources: &[PathBuf],
    destination: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let metadata = fs
        .metadata(destination)
        .await
        .with_context(|| format!("inspecting {}", destination.display()))?;
    let destination_is_directory = metadata.is_some_and(|metadata| metadata.is_dir);
    let destinations = if sources.len() == 1 && !destination_is_directory {
        sources
            .iter()
            .map(|source| (source.clone(), destination.to_path_buf()))
            .collect()
    } else {
        if !destination_is_directory {
            anyhow::bail!(
                "{} must be an existing directory for multiple entries",
                destination.display()
            );
        }
        sources
            .iter()
            .map(|source| {
                let name = source
                    .file_name()
                    .with_context(|| format!("{} has no file name", source.display()))?;
                Ok((source.clone(), destination.join(name)))
            })
            .collect::<Result<Vec<_>>>()?
    };

    let mut unique_destinations = HashSet::new();
    for (source, target) in &destinations {
        let source_metadata = fs.metadata(source).await?;
        if source_metadata.is_none() && fs.read_link(source).await.is_err() {
            anyhow::bail!("{} no longer exists", source.display());
        }
        if source == target {
            continue;
        }
        if !unique_destinations.insert(target.clone()) {
            anyhow::bail!("multiple entries would be moved to {}", target.display());
        }
        let parent = target
            .parent()
            .context("destination has no parent directory")?;
        let canonical_parent = fs
            .canonicalize(parent)
            .await
            .with_context(|| format!("opening destination directory {}", parent.display()))?;
        if !fs
            .metadata(parent)
            .await?
            .is_some_and(|metadata| metadata.is_dir)
        {
            anyhow::bail!("{} is not a directory", parent.display());
        }
        if source_metadata.is_some_and(|metadata| metadata.is_dir && !metadata.is_symlink) {
            let canonical_source = fs.canonicalize(source).await?;
            if canonical_parent.starts_with(&canonical_source) {
                anyhow::bail!(
                    "cannot put {} inside itself at {}",
                    source.display(),
                    target.display()
                );
            }
        }
        if fs
            .metadata(target)
            .await
            .with_context(|| format!("inspecting {}", target.display()))?
            .is_some()
            || fs.read_link(target).await.is_ok()
        {
            anyhow::bail!("{} already exists", target.display());
        }
    }

    Ok(destinations)
}

async fn rename_entries(
    fs: &dyn Fs,
    sources: &[PathBuf],
    destination: &Path,
) -> Result<Vec<PathBuf>> {
    let destinations = entry_destinations(fs, sources, destination).await?;

    let mut completed: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (source, target) in &destinations {
        if source == target {
            continue;
        }
        if let Err(error) = fs
            .rename(source, target, RenameOptions::default())
            .await
            .with_context(|| format!("moving {} to {}", source.display(), target.display()))
        {
            let mut rollback_failures = Vec::new();
            for (completed_source, completed_target) in completed.iter().rev() {
                if let Err(rollback_error) = fs
                    .rename(completed_target, completed_source, RenameOptions::default())
                    .await
                {
                    rollback_failures.push(format!(
                        "{} to {}: {rollback_error:#}",
                        completed_target.display(),
                        completed_source.display()
                    ));
                }
            }
            if rollback_failures.is_empty() {
                return Err(error);
            }
            return Err(error.context(format!(
                "also failed to roll back:\n{}",
                rollback_failures.join("\n")
            )));
        }
        completed.push((source.clone(), target.clone()));
    }

    Ok(destinations
        .into_iter()
        .map(|(_, destination)| destination)
        .collect())
}

async fn copy_entries(
    fs: &dyn Fs,
    sources: &[PathBuf],
    destination: &Path,
) -> Result<Vec<PathBuf>> {
    let destinations = entry_destinations(fs, sources, destination).await?;
    if destinations.iter().any(|(source, target)| source == target) {
        anyhow::bail!("cannot copy an entry onto itself");
    }
    let mut pending = destinations.clone();
    while let Some((source, target)) = pending.pop() {
        let result: Result<()> = async {
            if fs.metadata(&target).await?.is_some() || fs.read_link(&target).await.is_ok() {
                anyhow::bail!("{} already exists", target.display());
            }
            let metadata = fs.metadata(&source).await?;
            if metadata.is_none_or(|metadata| metadata.is_symlink) {
                let link_target = fs
                    .read_link(&source)
                    .await
                    .with_context(|| format!("reading symlink {}", source.display()))?;
                fs.create_symlink(&target, link_target).await?;
            } else if metadata.is_some_and(|metadata| metadata.is_dir) {
                fs.create_dir(&target).await?;
                let mut children = fs.read_dir(&source).await?;
                while let Some(child) = children.next().await {
                    let child = child?;
                    let name = child.file_name().context("entry has no file name")?;
                    let child_target = target.join(name);
                    pending.push((child, child_target));
                }
            } else {
                if metadata.is_some_and(|metadata| metadata.is_fifo) {
                    anyhow::bail!("cannot copy named pipe {}", source.display());
                }
                fs.copy_file(&source, &target, CopyOptions::default())
                    .await?;
            }
            Ok(())
        }
        .await;
        result.with_context(|| {
            format!(
                "copying {} to {}; earlier copies may already exist",
                source.display(),
                target.display()
            )
        })?;
    }
    Ok(destinations.into_iter().map(|(_, target)| target).collect())
}

async fn completion_candidates(
    fs: &dyn Fs,
    path: &Path,
    input: &str,
) -> Result<Vec<PathCompletionCandidate>> {
    let metadata = fs
        .metadata(path)
        .await
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    if metadata.as_ref().is_some_and(|metadata| !metadata.is_dir) {
        return Ok(vec![PathCompletionCandidate {
            path: path.to_path_buf(),
            text: path_text_for_input(path, false, input),
        }]);
    }

    let (directory, prefix) = if metadata.as_ref().is_some_and(|metadata| metadata.is_dir) {
        (path.to_path_buf(), None)
    } else {
        let Some(parent) = path.parent() else {
            return Ok(Vec::new());
        };
        (
            parent.to_path_buf(),
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned()),
        )
    };

    let entries = read_directory(fs, &directory).await?;
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.is_parent)
        .filter(|entry| {
            prefix
                .as_ref()
                .is_none_or(|prefix| entry.file_name().starts_with(prefix))
        })
        .map(|entry| PathCompletionCandidate {
            text: path_text_for_input(&entry.path, entry.is_directory, input),
            path: entry.path,
        })
        .collect())
}

fn path_query<'a>(input: &'a str, current_path: &Path) -> &'a str {
    let directory_text = path_text(current_path, true);
    let query = input.strip_prefix(&directory_text).unwrap_or_else(|| {
        input
            .trim_end_matches(std::path::MAIN_SEPARATOR)
            .rsplit(std::path::MAIN_SEPARATOR)
            .next()
            .unwrap_or(input)
    });
    query
        .trim_end_matches(std::path::MAIN_SEPARATOR)
        .rsplit(std::path::MAIN_SEPARATOR)
        .next()
        .unwrap_or(query)
}

fn resolve_path_input(input: &str, current_path: &Path) -> Option<PathBuf> {
    if input.is_empty() {
        return None;
    }

    let path = if input == "~" {
        paths::home_dir().clone()
    } else if let Some(relative_path) = input.strip_prefix("~/") {
        paths::home_dir().join(relative_path)
    } else {
        let path = Path::new(input);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            current_path.join(path)
        }
    };
    paths::normalize_lexically(&path).ok()
}

fn path_text(path: &Path, is_directory: bool) -> String {
    let mut text = path.compact().to_string_lossy().into_owned();
    if is_directory && path.parent().is_some() && !text.ends_with(std::path::MAIN_SEPARATOR) {
        text.push(std::path::MAIN_SEPARATOR);
    }
    text
}

fn path_text_for_input(path: &Path, is_directory: bool, input: &str) -> String {
    if input == "~" || input.starts_with("~/") {
        if path == paths::home_dir().as_path() {
            return if is_directory { "~/" } else { "~" }.to_string();
        }
        if let Ok(relative_path) = path.strip_prefix(paths::home_dir()) {
            let mut text = format!("~/{}", relative_path.to_string_lossy());
            if is_directory && !text.ends_with(std::path::MAIN_SEPARATOR) {
                text.push(std::path::MAIN_SEPARATOR);
            }
            return text;
        }
    }
    path_text(path, is_directory)
}

fn initial_directory(workspace: &Workspace, cx: &App) -> PathBuf {
    if let Some(directory_view) = workspace.active_item_as::<ProjectDirectoryView>(cx) {
        return directory_view.read(cx).current_path.clone();
    }

    let project = workspace.project();
    if let Some(project_path) = workspace
        .active_item(cx)
        .and_then(|item| item.project_path(cx))
    {
        if let Some(absolute_path) = project.read(cx).absolute_path(&project_path, cx) {
            return project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .filter(|entry| entry.is_dir())
                .map(|_| absolute_path.clone())
                .or_else(|| absolute_path.parent().map(Path::to_path_buf))
                .unwrap_or(absolute_path);
        }
    }

    project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .unwrap_or_else(|| paths::home_dir().to_path_buf())
}

pub(crate) fn create(
    workspace: &Workspace,
    mode: DirectoryViewMode,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<ProjectDirectoryView> {
    let current_path = initial_directory(workspace, cx);
    let selected_path = workspace
        .active_item(cx)
        .and_then(|item| item.project_path(cx))
        .and_then(|path| workspace.project().read(cx).absolute_path(&path, cx))
        .filter(|path| path.parent() == Some(current_path.as_path()));
    if mode == DirectoryViewMode::Dired {
        let existing = workspace
            .active_pane()
            .read(cx)
            .items_of_type::<ProjectDirectoryView>()
            .find(|view| {
                let view = view.read(cx);
                view.mode == mode && view.current_path == current_path
            });
        if let Some(existing) = existing {
            existing.update(cx, |view, cx| {
                if !view.operation_in_progress && view.entry_edit.is_none() {
                    let selected_path =
                        selected_path.or_else(|| view.selected_entry().map(|entry| entry.path));
                    view.load_directory(current_path, selected_path, window, cx);
                }
            });
            return existing;
        }
    }
    let fs = workspace.app_state().fs.clone();
    let project = workspace.project().clone();
    let workspace_handle = workspace.weak_handle();
    cx.new(|cx| {
        ProjectDirectoryView::new(
            fs,
            project,
            workspace_handle,
            current_path,
            selected_path,
            mode,
            window,
            cx,
        )
    })
}

pub(crate) fn find_file(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let current_path = initial_directory(workspace, cx);
    let fs = workspace.app_state().fs.clone();
    let project = workspace.project().clone();
    let workspace_handle = workspace.weak_handle();
    let origin = PromptOrigin {
        directory: current_path.clone(),
        pane: workspace.active_pane().downgrade(),
        active_item: workspace.active_item(cx),
        preview_item: workspace.active_pane().read(cx).preview_item(),
        item_ids: workspace
            .active_pane()
            .read(cx)
            .items()
            .map(|item| item.item_id())
            .collect(),
        previews: Vec::new(),
        last_preview: None,
    };
    workspace.toggle_modal(window, cx, move |window, cx| {
        let mut view = ProjectDirectoryView::new(
            fs,
            project,
            workspace_handle,
            current_path,
            None,
            DirectoryViewMode::FindFile,
            window,
            cx,
        );
        view.is_prompt = true;
        view.input_selected = true;
        view.prompt_origin = Some(origin);
        view
    });
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use menu::Confirm;
    use project::FakeFs;
    use serde_json::json;
    use util::rel_path::rel_path;
    use workspace::{ItemHandle, ItemPlacement, MultiWorkspace, SurfaceRole};

    use super::*;

    fn display_in_active_pane(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        crate::display_directory(
            workspace,
            ItemPlacement::ActivePane,
            DirectoryViewMode::FindFile,
            window,
            cx,
        );
    }

    #[gpui::test]
    async fn test_find_file_ranking_and_tab_cycle_preserve_search(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                ".docker": {}, "Documents": {"nested.txt": "nested"}, "other.txt": "other"
            }),
        )
        .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.dispatch_action(crate::FindFile);
        cx.run_until_parked();
        let prompt = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<ProjectDirectoryView>(cx)
            })
            .expect("file prompt");
        prompt.update_in(cx, |prompt, window, cx| {
            prompt
                .path_editor
                .update(cx, |editor, cx| editor.handle_input("Doc", window, cx));
        });
        cx.run_until_parked();
        for expected in ["Documents", ".docker", "Documents"] {
            prompt.update_in(cx, |prompt, window, cx| {
                prompt.complete_path(&CompletePath, window, cx)
            });
            cx.run_until_parked();
            prompt.update_in(cx, |prompt, window, cx| {
                assert_eq!(prompt.current_path, Path::new("/project"));
                assert_eq!(prompt.filter_query, "doc");
                assert_eq!(
                    prompt
                        .entries
                        .iter()
                        .map(DirectoryEntry::file_name)
                        .collect::<Vec<_>>(),
                    ["Documents/", ".docker/"]
                );
                assert_eq!(
                    prompt.path_editor.read(cx).text(cx),
                    format!("/project/{expected}/")
                );
                assert_eq!(
                    prompt.selected_entry().expect("completion").path,
                    Path::new("/project").join(expected)
                );
                assert!(
                    prompt
                        .path_editor
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                );
            });
        }
        prompt.update_in(cx, |prompt, window, cx| {
            prompt
                .path_editor
                .update(cx, |editor, cx| editor.set_text("/project/Dcm", window, cx));
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert_eq!(
                prompt
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["Documents/"]
            );
            prompt.navigate_selected(&NavigateSelected, window, cx);
        });
        cx.run_until_parked();
        prompt.read_with(cx, |prompt, _| {
            assert_eq!(prompt.current_path, Path::new("/project/Documents"));
            assert_eq!(
                prompt
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["nested.txt"]
            );
        });
    }

    #[gpui::test]
    async fn test_dired_preview_reuses_other_pane_without_taking_focus(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/outside",
            json!({".backup.json": "first preview", "other.txt": "second preview"}),
        )
        .await;
        fs.insert_tree("/project", json!({})).await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.dispatch_action(crate::OpenDirectory);
        cx.run_until_parked();
        let view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("Dired");
        view.update_in(cx, |view, window, cx| {
            view.navigate_to_directory(PathBuf::from("/outside"), None, window, cx);
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.maximize_item_pane(&view, window, cx);
            assert!(workspace.is_pane_maximized());
        });
        let mut preview_pane_id = None;
        for (filename, content) in [
            (".backup.json", "first preview"),
            ("other.txt", "second preview"),
        ] {
            view.update_in(cx, |view, window, cx| {
                let index = view
                    .entries
                    .iter()
                    .position(|entry| entry.path == Path::new("/outside").join(filename))
                    .expect("file entry");
                view.select_index(index, cx);
                view.navigate_selected(&NavigateSelected, window, cx);
            });
            cx.run_until_parked();
            view.update_in(cx, |view, window, _| {
                assert!(view.item_focus_handle.is_focused(window));
                assert!(view.operation_error.is_none());
            });
            workspace.read_with(cx, |workspace, cx| {
                assert_eq!(workspace.panes().len(), 2);
                assert!(!workspace.is_pane_maximized());
                assert_eq!(
                    workspace
                        .active_item(cx)
                        .expect("Dired remains active")
                        .item_id(),
                    view.entity_id()
                );
                let preview = workspace
                    .items_of_type::<Editor>(cx)
                    .next()
                    .expect("preview editor");
                assert_eq!(preview.read(cx).text(cx), content);
                let pane = workspace
                    .pane_for_item_id(preview.entity_id())
                    .expect("preview pane");
                assert_ne!(pane.entity_id(), workspace.active_pane().entity_id());
                assert_eq!(pane.read(cx).items_len(), 1);
                if let Some(previous) = preview_pane_id {
                    assert_eq!(pane.entity_id(), previous);
                }
                preview_pane_id = Some(pane.entity_id());
            });
        }
    }

    #[gpui::test]
    async fn test_find_file_prompt_navigation_and_directory_confirmation(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({"src": {"main.rs": "main"}, "current.txt": "current"}),
        )
        .await;
        fs.insert_tree("/elsewhere", json!({"other.txt": "other"}))
            .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.dispatch_action(crate::FindFile);
        cx.run_until_parked();
        let prompt = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<ProjectDirectoryView>(cx)
            })
            .expect("file prompt");
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 1);
            assert_eq!(
                workspace.items_of_type::<ProjectDirectoryView>(cx).count(),
                0
            );
        });
        prompt.update_in(cx, |prompt, window, cx| {
            assert!(prompt.input_selected);
            assert!(prompt.selected_entry().is_none());
            assert!(
                prompt
                    .path_editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
            assert_eq!(
                prompt.path_editor.read(cx).cursor_shape(),
                language::CursorShape::Block
            );
            prompt.select_next(&SelectNext, window, cx);
            assert_eq!(
                prompt.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/src"))
            );
            prompt.complete_path(&CompletePath, window, cx);
            assert_eq!(prompt.current_path, Path::new("/project"));
            assert_eq!(prompt.path_editor.read(cx).text(cx), "/project/src/");
            prompt.navigate_selected(&NavigateSelected, window, cx);
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert_eq!(prompt.current_path, Path::new("/project/src"));
            assert!(prompt.input_selected);
            assert_eq!(
                prompt
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["main.rs"]
            );
            prompt.complete_path(&CompletePath, window, cx);
            assert!(prompt.path_editor.read(cx).text(cx).ends_with("main.rs"));
            prompt.path_backspace(&PathBackspace, window, cx);
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert!(prompt.path_editor.read(cx).text(cx).ends_with("main.r"));
            prompt.go_up(&GoUp, window, cx);
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert_eq!(prompt.path_editor.read(cx).text(cx), "/project/src/");
            prompt.path_backspace(&PathBackspace, window, cx);
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert_eq!(prompt.current_path, Path::new("/project"));
            prompt
                .path_editor
                .update(cx, |editor, cx| editor.set_text("src/", window, cx));
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert_eq!(prompt.current_path, Path::new("/project/src"));
            prompt
                .path_editor
                .update(cx, |editor, cx| editor.set_text("src/main", window, cx));
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert_eq!(prompt.current_path, Path::new("/project/src"));
            assert_eq!(
                prompt.resolve_prompt_input("src/main"),
                Some(PathBuf::from("/project/src/main"))
            );
            prompt.path_editor.update(cx, |editor, cx| {
                editor.set_text("~/", window, cx);
                editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
            });
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            prompt.go_up(&GoUp, window, cx);
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert_eq!(
                prompt.current_path,
                paths::home_dir().parent().expect("home parent")
            );
            prompt
                .path_editor
                .update(cx, |editor, cx| editor.set_text("/elsewhere/", window, cx));
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            assert_eq!(prompt.current_path, Path::new("/elsewhere"));
            assert!(prompt.input_selected);
            prompt.confirm_path(&ConfirmPath, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<ProjectDirectoryView>(cx).is_none());
            assert_eq!(workspace.panes().len(), 1);
            assert!(!workspace.is_pane_maximized());
            let view = workspace
                .active_item_as::<ProjectDirectoryView>(cx)
                .expect("Dired");
            assert_eq!(view.read(cx).current_path, Path::new("/elsewhere"));
            assert_eq!(view.read(cx).mode, DirectoryViewMode::Dired);
        });
    }

    #[gpui::test]
    async fn test_find_file_preview_cancel_and_explicit_input(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({"current.txt": "current"}))
            .await;
        fs.insert_tree("/outside", json!({"other.txt": "other"}))
            .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let original = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from("/project/current.txt"),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("original file");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.replace_preview_item_id(original.item_id(), window, cx);
                assert!(pane.is_active_preview_item(original.item_id()));
            });
        });
        cx.dispatch_action(crate::FindFile);
        cx.run_until_parked();
        let prompt = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<ProjectDirectoryView>(cx)
            })
            .expect("file prompt");
        prompt.update_in(cx, |prompt, window, cx| {
            prompt.path_editor.update(cx, |editor, cx| {
                editor.set_text("/outside/other", window, cx)
            });
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            prompt.select_last(&SelectLast, window, cx);
            prompt.navigate_selected(&NavigateSelected, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<ProjectDirectoryView>(cx).is_some());
            assert_ne!(
                workspace.active_item(cx).expect("preview").item_id(),
                original.item_id()
            );
            assert!(
                workspace
                    .active_pane()
                    .read(cx)
                    .index_for_item(original.as_ref())
                    .is_some()
            );
        });
        prompt.update_in(cx, |prompt, window, cx| {
            assert!(
                prompt
                    .path_editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
            prompt.cancel_prompt(&Cancel, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<ProjectDirectoryView>(cx).is_none());
            assert_eq!(
                workspace.active_item(cx).expect("restored").item_id(),
                original.item_id()
            );
            assert_eq!(workspace.items(cx).count(), 1);
            assert!(
                workspace
                    .active_pane()
                    .read(cx)
                    .is_active_preview_item(original.item_id())
            );
        });
        cx.dispatch_action(crate::FindFile);
        cx.run_until_parked();
        let prompt = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<ProjectDirectoryView>(cx)
            })
            .expect("file prompt");
        prompt.update_in(cx, |prompt, window, cx| {
            prompt.path_editor.update(cx, |editor, cx| {
                editor.set_text("/project/new.txt", window, cx)
            });
        });
        cx.run_until_parked();
        prompt.update_in(cx, |prompt, window, cx| {
            prompt.confirm_input(&ConfirmInput, window, cx)
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<ProjectDirectoryView>(cx).is_none());
            assert_eq!(
                workspace
                    .active_item(cx)
                    .and_then(|item| item.project_path(cx))
                    .map(|path| path.path),
                Some(Arc::from(rel_path("new.txt")))
            );
            assert_eq!(workspace.panes().len(), 1);
            assert!(!workspace.is_pane_maximized());
        });
        cx.dispatch_action(crate::FindFile);
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.hide_modal(window, cx);
            crate::display_directory(
                workspace,
                ItemPlacement::ActivePane,
                DirectoryViewMode::Dired,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            let view = workspace
                .active_item_as::<ProjectDirectoryView>(cx)
                .expect("new Dired stays active");
            assert!(view.read(cx).item_focus_handle.is_focused(window));
        });
    }

    #[gpui::test]
    async fn test_copy_preserves_trees_and_symlinks(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/files",
            json!({
                "source": {"nested": {"file.txt": "contents"}},
                "destination": {}
            }),
        )
        .await;
        fs.create_symlink(
            Path::new("/files/source/link"),
            PathBuf::from("nested/file.txt"),
        )
        .await
        .expect("create link");
        fs.create_symlink(Path::new("/files/source/broken"), PathBuf::from("missing"))
            .await
            .expect("create broken link");
        fs.create_symlink(Path::new("/files/source/loop"), PathBuf::from("."))
            .await
            .expect("create loop");
        let sources = [PathBuf::from("/files/source")];
        let copied = copy_entries(fs.as_ref(), &sources, Path::new("/files/destination"))
            .await
            .expect("copy tree");
        assert_eq!(copied, [PathBuf::from("/files/destination/source")]);
        assert_eq!(
            fs.load(Path::new("/files/destination/source/nested/file.txt"))
                .await
                .expect("copied file"),
            "contents"
        );
        for (name, target) in [
            ("link", "nested/file.txt"),
            ("broken", "missing"),
            ("loop", "."),
        ] {
            assert_eq!(
                fs.read_link(&Path::new("/files/destination/source").join(name))
                    .await
                    .expect("copied symlink"),
                PathBuf::from(target)
            );
        }
        assert!(
            copy_entries(fs.as_ref(), &sources, Path::new("/files/destination"))
                .await
                .is_err()
        );
        assert!(
            copy_entries(fs.as_ref(), &sources, Path::new("/files"))
                .await
                .is_err()
        );
        assert_eq!(
            fs.load(Path::new("/files/source/nested/file.txt"))
                .await
                .expect("source remains"),
            "contents"
        );
    }

    #[gpui::test]
    async fn test_exact_path_confirmation_and_cancelled_navigation(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({"alpha": {}, "alphabet": {}}))
            .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("browser");
        view.update_in(cx, |view, window, cx| {
            view.path_editor.update(cx, |editor, cx| {
                editor.set_text("/project/alpha", window, cx)
            });
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            view.select_last(&SelectLast, window, cx);
            assert_eq!(
                view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/alphabet"))
            );
            view.confirm_path(&ConfirmPath, window, cx);
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.current_path, Path::new("/project/alpha"));
            view.path_editor.update(cx, |editor, cx| {
                editor.set_text("/project/alphabet", window, cx)
            });
            view.confirm_path(&ConfirmPath, window, cx);
            view.go_up(&GoUp, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.current_path, Path::new("/project"))
        });
    }

    #[gpui::test]
    async fn test_move_into_directory_and_validate_entire_batch(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/files",
            json!({
                "alpha": "alpha", "beta": "beta", "directory": {"nested": {}},
                "destination": {"beta": "existing"}
            }),
        )
        .await;
        let sources = [PathBuf::from("/files/alpha"), PathBuf::from("/files/beta")];
        assert!(
            rename_entries(fs.as_ref(), &sources, Path::new("/files/destination"))
                .await
                .is_err()
        );
        assert!(fs.is_file(Path::new("/files/alpha")).await);
        assert!(!fs.is_file(Path::new("/files/destination/alpha")).await);
        assert_eq!(
            fs.load(Path::new("/files/destination/beta"))
                .await
                .expect("existing file"),
            "existing"
        );
        let moved = rename_entries(fs.as_ref(), &sources[..1], Path::new("/files/destination"))
            .await
            .expect("move single file into directory");
        assert_eq!(moved, [PathBuf::from("/files/destination/alpha")]);
        assert!(!fs.is_file(Path::new("/files/alpha")).await);

        fs.create_symlink(
            Path::new("/files/alias"),
            PathBuf::from("/files/directory/nested"),
        )
        .await
        .expect("create alias");
        let directory = [PathBuf::from("/files/directory")];
        for destination in ["/files/directory/nested", "/files/alias"] {
            assert!(
                rename_entries(fs.as_ref(), &directory, Path::new(destination))
                    .await
                    .is_err()
            );
            assert!(
                copy_entries(fs.as_ref(), &directory, Path::new(destination))
                    .await
                    .is_err()
            );
        }
        assert!(fs.is_dir(Path::new("/files/directory/nested")).await);

        let missing = [
            PathBuf::from("/files/beta"),
            PathBuf::from("/files/missing"),
        ];
        assert!(
            rename_entries(fs.as_ref(), &missing, Path::new("/files/directory"))
                .await
                .is_err()
        );
        assert!(fs.is_file(Path::new("/files/beta")).await);
    }

    #[gpui::test]
    async fn test_listing_preserves_broken_links_and_sorts_numbers(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/files",
            json!({"file10": "", "file2": "", "directory": {}}),
        )
        .await;
        fs.create_symlink(Path::new("/files/broken"), PathBuf::from("missing"))
            .await
            .expect("create link");
        let entries = read_directory(fs.as_ref(), Path::new("/files"))
            .await
            .expect("read listing");
        assert_eq!(
            entries
                .iter()
                .map(DirectoryEntry::file_name)
                .collect::<Vec<_>>(),
            ["../", "directory/", "broken", "file2", "file10"]
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == Path::new("/files/broken") && entry.is_symlink)
        );
    }

    #[gpui::test]
    async fn test_dired_jump_reuses_buffer_and_preserves_panes(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({"alpha.txt": "alpha", "beta.txt": "beta"}),
        )
        .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from("/project/beta.txt"),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("open file");
        cx.dispatch_action(crate::OpenDirectory);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("Dired");
        directory_view.update_in(cx, |view, window, cx| {
            assert_eq!(view.mode, DirectoryViewMode::Dired);
            assert!(view.item_focus_handle.is_focused(window));
            assert_eq!(
                view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/beta.txt"))
            );
            view.mark_selected(&MarkSelected, window, cx);
            view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(crate::OpenDirectory);
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 1);
            assert!(!workspace.is_pane_maximized());
            assert_eq!(
                workspace.active_item_as::<ProjectDirectoryView>(cx),
                Some(directory_view.clone())
            );
            assert_eq!(
                workspace.items_of_type::<ProjectDirectoryView>(cx).count(),
                1
            );
        });
        directory_view.read_with(cx, |view, _| {
            assert!(view.marked_paths.contains(Path::new("/project/beta.txt")))
        });
    }

    #[gpui::test]
    async fn test_deletion_flags_are_separate_from_marks(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({"alpha": "", "beta": "", "gamma": ""}))
            .await;
        let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.dispatch_action(crate::OpenDirectory);
        cx.run_until_parked();
        let view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("Dired");
        view.update_in(cx, |view, window, cx| {
            view.flag_for_deletion(&FlagForDeletion, window, cx);
            view.mark_selected(&MarkSelected, window, cx);
            assert_eq!(view.operation_targets(), [PathBuf::from("/project/beta")]);
            view.trash_flagged(&TrashFlagged, window, cx);
        });
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(fs.trashed_paths().is_empty());
        view.update_in(cx, |view, window, cx| {
            assert!(view.flagged_paths.contains(Path::new("/project/alpha")));
            view.trash_flagged(&TrashFlagged, window, cx);
        });
        cx.simulate_prompt_answer("Trash");
        cx.run_until_parked();
        assert!(!fs.is_file(Path::new("/project/alpha")).await);
        assert!(fs.is_file(Path::new("/project/beta")).await);
        view.update_in(cx, |view, window, cx| {
            assert!(view.flagged_paths.is_empty());
            assert!(view.marked_paths.contains(Path::new("/project/beta")));
            view.trash_flagged(&TrashFlagged, window, cx);
        });
        assert!(!cx.has_pending_prompt());
        view.update_in(cx, |view, window, cx| {
            view.undo_trash(&UndoTrash, window, cx)
        });
        cx.run_until_parked();
        assert!(fs.is_file(Path::new("/project/alpha")).await);
    }

    #[gpui::test]
    async fn test_directory_navigation_beyond_project_root(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/home/user",
            json!({
                "project": {
                    "directory": {
                        "nested.txt": "nested"
                    },
                    "root.txt": "root"
                },
                "sibling": {}
            }),
        )
        .await;
        let project = Project::test(fs, ["/home/user/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");

        assert_eq!(
            ItemHandle::surface_role(&directory_view),
            SurfaceRole::SpecialBuffer
        );
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/home/user/project"));
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["../", "directory/", "root.txt"]
            );
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/project/directory"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.select_index(0, cx);
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/home/user"));
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/project"))
            );
            assert!(
                directory_view
                    .entries
                    .iter()
                    .any(|entry| entry.path == Path::new("/home/user/sibling"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            let sibling_index = directory_view
                .entries
                .iter()
                .position(|entry| entry.path == Path::new("/home/user/sibling"))
                .expect("sibling directory should be listed");
            directory_view.select_index(sibling_index, cx);
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/home/user/sibling"));
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.go_up(&GoUp, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            let project_index = directory_view
                .entries
                .iter()
                .position(|entry| entry.path == Path::new("/home/user/project"))
                .expect("project directory should be listed");
            directory_view.select_index(project_index, cx);
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/home/user/project"));
        });
    }

    #[gpui::test]
    async fn test_directory_path_editing_and_completion(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "Documents": { "notes.txt": "notes" },
                "Downloads": {},
                "readme.txt": "readme"
            }),
        )
        .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        let path_editor =
            directory_view.read_with(cx, |directory_view, _| directory_view.path_editor.clone());

        path_editor.update_in(cx, |editor, window, cx| {
            assert_eq!(editor.text(cx), "/project/");
            assert!(editor.focus_handle(cx).is_focused(window));
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.complete_path(&CompletePath, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            path_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "/project/Documents/"
        );
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.filter_query, "");
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["../", "Documents/", "Downloads/", "readme.txt"]
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.complete_path(&CompletePath, window, cx);
        });
        assert_eq!(
            path_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "/project/Downloads/"
        );
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.filter_query, "");
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["../", "Documents/", "Downloads/", "readme.txt"]
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.path_backspace(&PathBackspace, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project"));
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/Downloads"))
            );
        });
        assert_eq!(
            path_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "/project/"
        );

        path_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/project/Downloax", window, cx);
            editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.path_backspace(&PathBackspace, window, cx);
        });
        assert_eq!(
            path_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "/project/Downloa"
        );

        path_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/project/Documents", window, cx);
            editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.confirm_path(&ConfirmPath, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, _cx| {
            assert_eq!(directory_view.current_path, Path::new("/project/Documents"));
            assert_eq!(directory_view.mode, DirectoryViewMode::Dired);
            assert!(directory_view.item_focus_handle.is_focused(window));
        });
    }

    #[gpui::test]
    async fn test_open_file_from_directory(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "src": { "main.rs": "fn main() {}" } }))
            .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();

        let active_path = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
        });
        assert_eq!(
            active_path.map(|path| path.path),
            Some(Arc::from(rel_path("src/main.rs")))
        );

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project/src"));
        });
    }

    #[gpui::test]
    async fn test_find_file_opens_nonexistent_path_as_new_buffer(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({})).await;
        let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        let path_editor =
            directory_view.read_with(cx, |directory_view, _| directory_view.path_editor.clone());
        path_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/project/new.rs", window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.confirm_path(&ConfirmPath, window, cx);
        });
        cx.run_until_parked();

        assert!(!fs.is_file(Path::new("/project/new.rs")).await);
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .active_item(cx)
                    .and_then(|item| item.project_path(cx))
                    .map(|path| path.path),
                Some(Arc::from(rel_path("new.rs")))
            );
        });
    }

    #[gpui::test]
    async fn test_open_directory_in_split(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "src": {} })).await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        cx.dispatch_action(crate::OpenDirectorySplit);
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 2);
            assert!(!workspace.is_pane_maximized());
            assert!(
                workspace
                    .active_item_as::<ProjectDirectoryView>(cx)
                    .is_some()
            );
        });

        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        directory_view.update_in(cx, |directory_view, window, cx| {
            assert_eq!(directory_view.mode, DirectoryViewMode::FindFile);
            assert!(
                directory_view
                    .path_editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.navigate_selected(&NavigateSelected, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            assert_eq!(directory_view.current_path, Path::new("/project/src"));
            assert_eq!(directory_view.mode, DirectoryViewMode::FindFile);
            assert!(
                directory_view
                    .path_editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });
        workspace.read_with(cx, |workspace, _| {
            assert!(!workspace.is_pane_maximized());
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.go_up(&GoUp, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.confirm_path(&ConfirmPath, window, cx);
        });
        cx.run_until_parked();

        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project/src"));
            assert_eq!(directory_view.mode, DirectoryViewMode::Dired);
        });
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.is_pane_maximized());
        });
    }

    #[gpui::test]
    async fn test_enter_opens_file_from_split_in_maximized_pane(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "main.rs": "fn main() {}" }))
            .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        cx.dispatch_action(crate::OpenDirectorySplit);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        directory_view.update_in(cx, |directory_view, window, cx| {
            assert_eq!(directory_view.mode, DirectoryViewMode::FindFile);
            directory_view.confirm_path(&ConfirmPath, window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.is_pane_maximized());
            assert_eq!(
                workspace
                    .active_item(cx)
                    .and_then(|item| item.project_path(cx))
                    .map(|path| path.path),
                Some(Arc::from(rel_path("main.rs")))
            );
        });
    }

    #[gpui::test]
    async fn test_directory_history_restores_selection_and_discards_forward_branch(
        cx: &mut TestAppContext,
    ) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "alpha": { "nested": {} }, "beta": {} }))
            .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.current_path,
                Path::new("/project/alpha/nested")
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.history_back(&HistoryBack, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project/alpha"));
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/alpha/nested"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.history_back(&HistoryBack, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project"));
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/alpha"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.history_forward(&HistoryForward, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project/alpha"));
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/alpha/nested"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.history_back(&HistoryBack, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, _window, cx| {
            let beta_index = directory_view
                .entries
                .iter()
                .position(|entry| entry.path == Path::new("/project/beta"))
                .expect("beta directory should be listed");
            directory_view.select_index(beta_index, cx);
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.history_forward(&HistoryForward, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project/beta"));
            assert_eq!(directory_view.history_index, 1);
            assert_eq!(directory_view.history.len(), 2);
        });
    }

    #[gpui::test]
    async fn test_directory_marks_follow_dired_selection_semantics(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({ "alpha": {}, "beta": {}, "notes.txt": "notes" }),
        )
        .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.mark_selected(&MarkSelected, window, cx);
            directory_view.mark_selected(&MarkSelected, window, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.marked_paths,
                HashSet::from([
                    PathBuf::from("/project/alpha"),
                    PathBuf::from("/project/beta"),
                ])
            );
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/notes.txt"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.select_previous_marked(&SelectPreviousMarked, window, cx);
            directory_view.select_previous_marked(&SelectPreviousMarked, window, cx);
            directory_view.unmark_selected(&UnmarkSelected, window, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.marked_paths,
                HashSet::from([PathBuf::from("/project/beta")])
            );
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/beta"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.toggle_marks(&ToggleMarks, window, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.marked_paths,
                HashSet::from([
                    PathBuf::from("/project/alpha"),
                    PathBuf::from("/project/notes.txt"),
                ])
            );
            assert!(!directory_view.marked_paths.contains(Path::new("/project")));
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.clear_marks(&ClearMarks, window, cx);
            directory_view.select_index(0, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert!(directory_view.marked_paths.is_empty());
            assert!(directory_view.operation_targets().is_empty());
        });

        directory_view.update_in(cx, |directory_view, _window, cx| {
            let notes_index = directory_view
                .entries
                .iter()
                .position(|entry| entry.path == Path::new("/project/notes.txt"))
                .expect("notes file should be listed");
            directory_view.select_index(notes_index, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.operation_targets(),
                [PathBuf::from("/project/notes.txt")]
            );
        });
    }

    #[gpui::test]
    async fn test_trash_uses_current_directory_marks_and_can_be_undone(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "alpha": { "nested.txt": "alpha" },
                "beta": {},
                "notes.txt": "notes"
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.mark_selected(&MarkSelected, window, cx);
            directory_view.mark_selected(&MarkSelected, window, cx);
            directory_view.filter_query = "notes".into();
            directory_view.update_visible_entries(None);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.operation_targets(),
                [
                    PathBuf::from("/project/alpha"),
                    PathBuf::from("/project/beta")
                ]
            );
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/notes.txt"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.trash_selected(&TrashSelected, window, cx);
        });
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Trash");
        cx.run_until_parked();

        assert_eq!(
            fs.trashed_paths().into_iter().collect::<HashSet<_>>(),
            HashSet::from([
                PathBuf::from("/project/alpha"),
                PathBuf::from("/project/beta")
            ])
        );
        directory_view.read_with(cx, |directory_view, _| {
            assert!(directory_view.marked_paths.is_empty());
            assert_eq!(directory_view.trash_history.len(), 1);
            assert_eq!(
                directory_view
                    .all_entries
                    .iter()
                    .filter(|entry| !entry.is_parent)
                    .map(|entry| entry.path.clone())
                    .collect::<Vec<_>>(),
                [PathBuf::from("/project/notes.txt")]
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.undo_trash(&UndoTrash, window, cx);
        });
        cx.run_until_parked();

        assert!(fs.trashed_paths().is_empty());
        assert!(fs.is_dir(Path::new("/project/alpha")).await);
        assert!(fs.is_dir(Path::new("/project/beta")).await);
        directory_view.read_with(cx, |directory_view, _| {
            assert!(directory_view.trash_history.is_empty());
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/alpha"))
            );
        });
    }

    #[gpui::test]
    async fn test_create_directory_uses_dired_entry_input(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "existing-directory": {} }))
            .await;
        let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        let path_editor =
            directory_view.read_with(cx, |directory_view, _| directory_view.path_editor.clone());

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.enter_dired(false, window, cx);
            directory_view.create_directory(&CreateDirectory, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            assert_eq!(directory_view.mode, DirectoryViewMode::Dired);
            assert_eq!(directory_view.path_editor.read(cx).text(cx), "/project/");
            assert!(
                directory_view
                    .path_editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });

        path_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/project/created-directory", window, cx);
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.go_up(&GoUp, window, cx);
            directory_view.refresh(&Refresh, window, cx);
            assert_eq!(directory_view.current_path, Path::new("/project"));
            assert_eq!(
                directory_view.path_editor.read(cx).text(cx),
                "/project/created-directory"
            );
            directory_view.confirm_entry_edit(&ConfirmEntryEdit, window, cx);
        });
        cx.run_until_parked();

        assert!(fs.is_dir(Path::new("/project/created-directory")).await);
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.entry_edit, None);
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/created-directory"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.create_directory(&CreateDirectory, window, cx);
        });
        path_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/project/existing-directory", window, cx);
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.confirm_entry_edit(&ConfirmEntryEdit, window, cx);
        });
        cx.run_until_parked();

        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.entry_edit,
                Some(EntryEditKind::CreateDirectory)
            );
            assert!(directory_view.path_error.is_some());
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.cancel_entry_edit(&CancelEntryEdit, window, cx);
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            assert_eq!(directory_view.entry_edit, None);
            assert_eq!(directory_view.path_editor.read(cx).text(cx), "/project/");
            assert!(directory_view.item_focus_handle.is_focused(window));
        });
    }

    #[gpui::test]
    async fn test_rename_moves_selected_or_marked_entries(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "alpha.txt": "alpha",
                "beta.txt": "beta",
                "destination": {}
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        let path_editor =
            directory_view.read_with(cx, |directory_view, _| directory_view.path_editor.clone());

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.enter_dired(false, window, cx);
            let alpha_index = directory_view
                .entries
                .iter()
                .position(|entry| entry.path == Path::new("/project/alpha.txt"))
                .expect("alpha file should be listed");
            directory_view.select_index(alpha_index, cx);
            directory_view.rename_selected(&RenameSelected, window, cx);
        });
        assert_eq!(
            path_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "alpha.txt"
        );
        path_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("renamed.txt", window, cx);
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.confirm_entry_edit(&ConfirmEntryEdit, window, cx);
        });
        cx.run_until_parked();

        assert!(!fs.is_file(Path::new("/project/alpha.txt")).await);
        assert!(fs.is_file(Path::new("/project/renamed.txt")).await);
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/renamed.txt"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.marked_paths.extend([
                PathBuf::from("/project/beta.txt"),
                PathBuf::from("/project/renamed.txt"),
            ]);
            directory_view.rename_selected(&RenameSelected, window, cx);
        });
        assert_eq!(path_editor.read_with(cx, |editor, cx| editor.text(cx)), "");
        path_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/project/destination", window, cx);
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.confirm_entry_edit(&ConfirmEntryEdit, window, cx);
        });
        cx.run_until_parked();

        assert!(fs.is_file(Path::new("/project/destination/beta.txt")).await);
        assert!(
            fs.is_file(Path::new("/project/destination/renamed.txt"))
                .await
        );
        directory_view.read_with(cx, |directory_view, _| {
            assert!(directory_view.marked_paths.is_empty());
            assert_eq!(directory_view.entry_edit, None);
        });
    }

    #[gpui::test]
    async fn test_refresh_rereads_directory_and_preserves_selection(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "existing.txt": "existing" }))
            .await;
        let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        fs.insert_file("/project/new.txt", b"new".to_vec()).await;

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.refresh(&Refresh, window, cx);
        });
        cx.run_until_parked();

        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["../", "existing.txt", "new.txt"]
            );
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/existing.txt"))
            );
        });
    }

    #[gpui::test]
    async fn test_path_input_filters_entries_while_retaining_focus(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test_with_editor(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/home/user",
            json!({ "Alpha": {}, "Documents": {}, "Documents Archive": {}, "Zulu": {} }),
        )
        .await;
        let project = Project::test(fs, ["/home/user".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        let path_editor =
            directory_view.read_with(cx, |directory_view, _| directory_view.path_editor.clone());

        path_editor.update_in(cx, |editor, window, cx| {
            editor.handle_input("Doc", window, cx);
            assert!(editor.focus_handle(cx).is_focused(window));
        });
        cx.run_until_parked();

        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["Documents/", "Documents Archive/"]
            );
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.select_next(&SelectNext, window, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/Documents Archive"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.complete_path(&CompletePath, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, cx| {
            assert_eq!(directory_view.filter_query, "doc");
            assert_eq!(
                path_editor.read(cx).text(cx),
                "/home/user/Documents Archive/"
            );
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["Documents/", "Documents Archive/"]
            );
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.complete_path(&CompletePath, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, cx| {
            assert_eq!(directory_view.filter_query, "doc");
            assert_eq!(path_editor.read(cx).text(cx), "/home/user/Documents/");
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["Documents/", "Documents Archive/"]
            );
        });

        path_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("/home/user/", window, cx);
            editor.move_to_end_of_line(&MoveToEndOfLine::default(), window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["../", "Alpha/", "Documents Archive/", "Documents/", "Zulu/"]
            );
        });

        path_editor.update_in(cx, |editor, window, cx| {
            editor.handle_input("Zul", window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["Zulu/"]
            );
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.complete_path(&CompletePath, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            assert_eq!(directory_view.current_path, Path::new("/home/user"));
            assert_eq!(path_editor.read(cx).text(cx), "/home/user/Zulu/");
            assert_eq!(directory_view.filter_query, "zul");
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["Zulu/"]
            );
            assert!(path_editor.read(cx).focus_handle(cx).is_focused(window));
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.path_backspace(&PathBackspace, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/home/user"));
            assert_eq!(directory_view.filter_query, "");
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/Zulu"))
            );
            assert_eq!(
                directory_view
                    .entries
                    .iter()
                    .map(DirectoryEntry::file_name)
                    .collect::<Vec<_>>(),
                ["../", "Alpha/", "Documents Archive/", "Documents/", "Zulu/"]
            );
        });
    }

    #[gpui::test]
    async fn test_git_directory_only_opens_as_workspace_explicitly(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/projects",
            json!({
                "current": { ".git": {}, "current.txt": "" },
                "other": { ".git": {}, "other.txt": "" }
            }),
        )
        .await;
        let project = Project::test(fs, ["/projects/current".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.go_up(&GoUp, window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            let other_index = directory_view
                .entries
                .iter()
                .position(|entry| entry.path == Path::new("/projects/other"))
                .expect("other repository should be listed");
            directory_view.select_index(other_index, cx);
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();

        window
            .read_with(cx, |multi_workspace, _| {
                assert_eq!(multi_workspace.workspaces().count(), 1);
            })
            .expect("window should exist");
        directory_view.update_in(cx, |view, window, cx| {
            assert_eq!(view.current_path, Path::new("/projects/other"));
            view.select_last(&SelectLast, window, cx);
            view.open_project(&OpenProject, window, cx);
        });
        cx.run_until_parked();
        window
            .read_with(cx, |multi_workspace, cx| {
                assert_eq!(multi_workspace.workspaces().count(), 2);
                let view = multi_workspace
                    .workspace()
                    .read(cx)
                    .active_item_as::<ProjectDirectoryView>(cx)
                    .expect("new project should open in Dired");
                assert_eq!(view.read(cx).mode, DirectoryViewMode::Dired);
                let active_project = multi_workspace.workspace().read(cx).project();
                assert!(
                    active_project
                        .read(cx)
                        .visible_worktrees(cx)
                        .any(|worktree| {
                            worktree.read(cx).abs_path().as_ref() == Path::new("/projects/other")
                        })
                );
            })
            .expect("multi-workspace window should exist");
    }

    #[gpui::test]
    async fn test_empty_workspace_starts_at_home(cx: &mut TestAppContext) {
        crate::project_panel_tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(paths::home_dir(), json!({ "project": {} }))
            .await;
        let project = Project::test(fs, [], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace should exist");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        workspace.update_in(cx, display_in_active_pane);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, paths::home_dir().as_path());
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(paths::home_dir().join("project"))
            );
            assert!(
                directory_view
                    .entries
                    .first()
                    .is_some_and(|entry| entry.is_parent)
            );
        });
    }
}
