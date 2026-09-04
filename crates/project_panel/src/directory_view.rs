use std::{
    cmp::Ordering,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use editor::{
    Editor, EditorEvent,
    actions::{Backspace, MoveToEndOfLine},
};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
    actions, uniform_list,
};
use menu::{Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
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
    Item, OpenMode, OpenOptions, OpenVisible, Workspace, notifications::DetachAndPromptErr,
};

actions!(
    project_browser,
    [GoUp, ConfirmPath, CompletePath, PathBackspace, Refresh]
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
    item_focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    _load_task: Task<()>,
    _open_task: Task<()>,
    _path_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectDirectoryView {
    fn new(
        fs: Arc<dyn Fs>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        current_path: PathBuf,
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
            item_focus_handle: item_focus_handle.clone(),
            scroll_handle: UniformListScrollHandle::new(),
            _load_task: Task::ready(()),
            _open_task: Task::ready(()),
            _path_task: Task::ready(()),
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
                this.resolved_path = None;
                this.path_completion = None;
                this.path_error = None;
                this.filter_query = path_query(&text, &this.current_path).to_lowercase();
                let selected_path = this.selected_entry().map(|entry| entry.path);
                this.update_visible_entries(selected_path.as_deref());
                cx.notify();
            }));
        this._subscriptions
            .push(cx.on_focus(&item_focus_handle, window, |this, window, cx| {
                this.path_editor.read(cx).focus_handle(cx).focus(window, cx);
            }));
        this.load_directory(current_path, None, window, cx);
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
        self.scroll_handle
            .scroll_to_item(self.selected_index, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(self.selected_index.saturating_add(1), cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(self.selected_index.saturating_sub(1), cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(0, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(usize::MAX, cx);
    }

    fn update_visible_entries(&mut self, preferred_path: Option<&Path>) {
        self.entries = if self.filter_query.is_empty() {
            self.all_entries.clone()
        } else {
            self.all_entries
                .iter()
                .filter(|entry| {
                    !entry.is_parent
                        && entry
                            .file_name()
                            .to_lowercase()
                            .contains(&self.filter_query)
                })
                .cloned()
                .collect()
        };
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
        self.current_path = path.clone();
        self.filter_query.clear();
        self.set_path_text(path.clone(), true, window, cx);
        self.all_entries.clear();
        self.entries.clear();
        self.selected_index = 0;
        self.loading = true;
        self.load_error = None;
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();

        let fs = self.fs.clone();
        self._load_task = cx.spawn_in(window, async move |this, cx| {
            let result = read_directory(fs.as_ref(), &path).await;
            this.update_in(cx, |this, _window, cx| {
                if this.current_path != path {
                    return;
                }

                this.loading = false;
                match result {
                    Ok(entries) => {
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

    fn open_selected(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };

        if !entry.is_directory {
            self.open_file(entry.path, OpenBehavior::Navigate, window, cx);
            return;
        }

        if entry.is_parent {
            let previous_path = self.current_path.clone();
            self.load_directory(entry.path, Some(previous_path), window, cx);
            return;
        }

        self.open_directory(entry.path, OpenBehavior::Navigate, window, cx);
    }

    fn open_directory(
        &mut self,
        path: PathBuf,
        behavior: OpenBehavior,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_current_project_root(&path, cx) {
            self.load_directory(path, None, window, cx);
            if behavior == OpenBehavior::Open {
                self.maximize(window, cx);
            }
            return;
        }

        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        self._open_task = cx.spawn_in(window, async move |this, cx| {
            let git_metadata = fs.metadata(&path.join(".git")).await;
            match git_metadata {
                Ok(Some(_)) => {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace
                                .open_fresh_workspace_for_paths(
                                    OpenMode::Activate,
                                    vec![path],
                                    window,
                                    cx,
                                )
                                .detach_and_prompt_err(
                                    "Failed to open project",
                                    window,
                                    cx,
                                    |_, _, _| None,
                                );
                        })
                        .log_err();
                }
                Ok(None) => {
                    this.update_in(cx, |this, window, cx| {
                        this.load_directory(path, None, window, cx);
                        if behavior == OpenBehavior::Open {
                            this.maximize(window, cx);
                        }
                    })
                    .log_err();
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.load_error =
                            Some(format!("Failed to inspect {}: {error:#}", path.display()).into());
                        cx.notify();
                    })
                    .log_err();
                }
            }
        });
    }

    fn confirm_path(&mut self, _: &ConfirmPath, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.path_editor.read(cx).text(cx);
        let resolved_input = resolve_path_input(&input, &self.current_path);
        let selected_path = resolved_input
            .as_ref()
            .is_some_and(|path| {
                path == &self.current_path || path.parent() == Some(self.current_path.as_path())
            })
            .then(|| self.selected_entry().map(|entry| entry.path))
            .flatten();
        let Some(path) = selected_path.or(resolved_input) else {
            self.path_error = Some("Enter a path".into());
            cx.notify();
            return;
        };

        self.path_error = None;
        let fs = self.fs.clone();
        self._path_task = cx.spawn_in(window, async move |this, cx| {
            let metadata = fs.metadata(&path).await;
            this.update_in(cx, |this, window, cx| {
                if this.path_editor.read(cx).text(cx) != input {
                    return;
                }

                match metadata {
                    Ok(Some(metadata)) if metadata.is_dir => {
                        this.open_directory(path, OpenBehavior::Open, window, cx);
                    }
                    Ok(Some(_)) => {
                        this.set_path_text(path.clone(), false, window, cx);
                        this.open_file(path, OpenBehavior::Open, window, cx);
                    }
                    Ok(None) => {
                        this.path_error = Some(format!("No such path: {}", path.display()).into());
                        cx.notify();
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

        let resolved_input = resolve_path_input(&input, &self.current_path);
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
                self.load_directory(parent_path, path_to_select, window, cx);
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
        if behavior == OpenBehavior::Open {
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

    fn maximize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let directory_view = cx.entity();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.maximize_item_pane(&directory_view, window, cx);
            })
            .log_err();
    }

    fn is_current_project_root(&self, path: &Path, cx: &App) -> bool {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .any(|worktree| worktree.read(cx).abs_path().as_ref() == path)
    }

    fn go_up(&mut self, _: &GoUp, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent_path) = self.current_path.parent().map(Path::to_path_buf) else {
            return;
        };
        let previous_path = self.current_path.clone();
        self.load_directory(parent_path, Some(previous_path), window, cx);
    }

    fn refresh(&mut self, _: &Refresh, window: &mut Window, cx: &mut Context<Self>) {
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
                let selected = index == self.selected_index;
                let icon = if entry.is_directory {
                    IconName::Folder
                } else {
                    IconName::File
                };
                let name = entry.file_name();
                let details = entry.details();
                let is_directory = entry.is_directory;

                Some(
                    ListItem::new(index)
                        .spacing(ListItemSpacing::ExtraDense)
                        .toggle_state(selected)
                        .aria_label(name.clone())
                        .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
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
                                .child(Label::new(name)),
                        )
                        .on_click(
                            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                this.select_index(index, cx);
                                if event.click_count() > 1 {
                                    this.open_selected(&Confirm, window, cx);
                                }
                                if event.click_count() == 1 || is_directory {
                                    this.path_editor.read(cx).focus_handle(cx).focus(window, cx);
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
}

impl Render for ProjectDirectoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entry_count = self.entries.len();
        v_flex()
            .key_context("ProjectDirectory")
            .track_focus(&self.item_focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::confirm_path))
            .on_action(cx.listener(Self::complete_path))
            .on_action(cx.listener(Self::path_backspace))
            .on_action(cx.listener(Self::refresh))
            .child(
                h_flex()
                    .key_context("ProjectDirectoryPath")
                    .h_8()
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("Directory")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().min_w_0().child(self.path_editor.clone()))
                    .when_some(self.path_error.clone(), |this, error| {
                        this.child(Label::new(error).color(Color::Error))
                    }),
            )
            .when(self.loading, |this| {
                this.child(div().px_3().py_2().child(Label::new("Loading…")))
            })
            .when_some(self.load_error.clone(), |this, error| {
                this.child(div().px_3().py_2().child(Label::new(error)))
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
            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
    }
}

impl Focusable for ProjectDirectoryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.item_focus_handle.clone()
    }
}

impl EventEmitter<()> for ProjectDirectoryView {}

impl Item for ProjectDirectoryView {
    type Event = ();

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
            .with_context(|| format!("reading metadata for {}", child_path.display()))?
            .with_context(|| format!("{} disappeared while reading", child_path.display()))?;
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
            _ => left.file_name().cmp(&right.file_name()),
        },
    );
    if let Some(parent_path) = path.parent() {
        entries.insert(0, DirectoryEntry::parent(parent_path.to_path_buf()));
    }
    Ok(entries)
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
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<ProjectDirectoryView> {
    let current_path = initial_directory(workspace, cx);
    let fs = workspace.app_state().fs.clone();
    let project = workspace.project().clone();
    let workspace_handle = workspace.weak_handle();
    cx.new(|cx| ProjectDirectoryView::new(fs, project, workspace_handle, current_path, window, cx))
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
        crate::display_directory(workspace, ItemPlacement::ActivePane, window, cx);
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
        directory_view.update_in(cx, |directory_view, window, cx| {
            assert_eq!(directory_view.current_path, Path::new("/project/Documents"));
            assert!(path_editor.read(cx).focus_handle(cx).is_focused(window));
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.path_backspace(&PathBackspace, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project"));
            assert_eq!(directory_view.filter_query, "");
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/Documents"))
            );
        });
        assert_eq!(
            path_editor.read_with(cx, |editor, cx| editor.text(cx)),
            "/project/"
        );
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
            directory_view.open_selected(&Confirm, window, cx);
        });
        cx.run_until_parked();
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(directory_view.current_path, Path::new("/project/src"));
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
                ["Documents Archive/", "Documents/"]
            );
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.select_next(&SelectNext, window, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/Documents"))
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
                ["Documents Archive/", "Documents/"]
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
                ["Documents Archive/", "Documents/"]
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
    async fn test_git_directory_opens_as_workspace(cx: &mut TestAppContext) {
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
            .read_with(cx, |multi_workspace, cx| {
                assert_eq!(multi_workspace.workspaces().count(), 2);
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
