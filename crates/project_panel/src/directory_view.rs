use std::{
    cmp::Ordering,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
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
use util::{ResultExt, paths, size::format_file_size};
use workspace::{
    Item, OpenMode, OpenOptions, OpenVisible, Workspace, notifications::DetachAndPromptErr,
};

actions!(
    project_browser,
    [
        OpenDirectory,
        GoUp,
        StartSearch,
        ConfirmSearch,
        CancelSearch,
        SearchNext,
        SearchPrevious,
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
            return "..".into();
        }
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
            .into()
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
    entries: Vec<DirectoryEntry>,
    selected_index: usize,
    loading: bool,
    load_error: Option<SharedString>,
    search_editor: Option<Entity<Editor>>,
    searching: bool,
    search_origin_index: usize,
    search_match_found: bool,
    last_search_query: String,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    _load_task: Task<()>,
    _open_task: Task<()>,
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
        let mut this = Self {
            fs,
            project,
            workspace,
            current_path: current_path.clone(),
            entries: Vec::new(),
            selected_index: 0,
            loading: false,
            load_error: None,
            search_editor: None,
            searching: false,
            search_origin_index: 0,
            search_match_found: true,
            last_search_query: String::new(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            _load_task: Task::ready(()),
            _open_task: Task::ready(()),
            _subscriptions: Vec::new(),
        };
        this.load_directory(current_path, None, window, cx);
        this
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

    fn start_search(&mut self, _: &StartSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.searching = true;
        self.search_origin_index = self.selected_index;
        self.search_match_found = true;

        let search_editor = if let Some(search_editor) = &self.search_editor {
            search_editor.clone()
        } else {
            let search_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Search entries…", window, cx);
                editor
            });
            self._subscriptions.push(cx.subscribe(
                &search_editor,
                |this, search_editor, event, cx| {
                    if matches!(event, EditorEvent::BufferEdited) && this.searching {
                        let query = search_editor.read(cx).text(cx);
                        this.update_search_selection(&query, cx);
                    }
                },
            ));
            self.search_editor = Some(search_editor.clone());
            search_editor
        };

        search_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    fn update_search_selection(&mut self, query: &str, cx: &mut Context<Self>) {
        if query.is_empty() {
            self.search_match_found = true;
            self.select_index(self.search_origin_index, cx);
            return;
        }

        let entry_count = self.entries.len();
        let indices =
            (0..entry_count).map(|offset| (self.search_origin_index + offset) % entry_count);
        let matching_index = self.find_search_match(query, indices);

        self.search_match_found = matching_index.is_some();
        if let Some(index) = matching_index {
            self.select_index(index, cx);
        } else {
            cx.notify();
        }
    }

    fn confirm_search(&mut self, _: &ConfirmSearch, window: &mut Window, cx: &mut Context<Self>) {
        if !self.searching {
            return;
        }
        if let Some(search_editor) = &self.search_editor {
            self.last_search_query = search_editor.read(cx).text(cx);
        }
        self.searching = false;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn search_next(&mut self, _: &SearchNext, _: &mut Window, cx: &mut Context<Self>) {
        self.repeat_search(true, cx);
    }

    fn search_previous(&mut self, _: &SearchPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.repeat_search(false, cx);
    }

    fn repeat_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let entry_count = self.entries.len();
        if entry_count == 0 || self.last_search_query.is_empty() {
            return;
        }

        let selected_index = self.selected_index.min(entry_count.saturating_sub(1));
        let indices = (1..=entry_count).map(|offset| {
            if forward {
                (selected_index + offset) % entry_count
            } else {
                (selected_index + entry_count - (offset % entry_count)) % entry_count
            }
        });
        if let Some(index) = self.find_search_match(&self.last_search_query, indices) {
            self.select_index(index, cx);
        }
    }

    fn find_search_match(
        &self,
        query: &str,
        indices: impl Iterator<Item = usize>,
    ) -> Option<usize> {
        let query = query.to_lowercase();
        indices.into_iter().find(|index| {
            self.entries
                .get(*index)
                .is_some_and(|entry| entry.file_name().to_lowercase().contains(&query))
        })
    }

    fn cancel_search(&mut self, _: &CancelSearch, window: &mut Window, cx: &mut Context<Self>) {
        if !self.searching {
            return;
        }
        self.searching = false;
        self.select_index(self.search_origin_index, cx);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn load_directory(
        &mut self,
        path: PathBuf,
        path_to_select: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.searching {
            self.searching = false;
            self.focus_handle.focus(window, cx);
        }
        self.current_path = path.clone();
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
                        this.entries = entries;
                        this.selected_index = path_to_select
                            .as_ref()
                            .and_then(|path| {
                                this.entries.iter().position(|entry| &entry.path == path)
                            })
                            .unwrap_or_else(|| Self::default_selected_index(&this.entries));
                        this.scroll_handle
                            .scroll_to_item(this.selected_index, ScrollStrategy::Nearest);
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
            self.open_file(entry.path, window, cx);
            return;
        }

        if entry.is_parent {
            let previous_path = self.current_path.clone();
            self.load_directory(entry.path, Some(previous_path), window, cx);
            return;
        }

        if self.is_current_project_root(&entry.path, cx) {
            self.load_directory(entry.path, None, window, cx);
            return;
        }

        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        self._open_task = cx.spawn_in(window, async move |this, cx| {
            let git_metadata = fs.metadata(&entry.path.join(".git")).await;
            match git_metadata {
                Ok(Some(_)) => {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace
                                .open_fresh_workspace_for_paths(
                                    OpenMode::Activate,
                                    vec![entry.path],
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
                        this.load_directory(entry.path, None, window, cx)
                    })
                    .log_err();
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.load_error = Some(
                            format!("Failed to inspect {}: {error:#}", entry.path.display()).into(),
                        );
                        cx.notify();
                    })
                    .log_err();
                }
            }
        });
    }

    fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
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
                            }),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn display_path(&self) -> SharedString {
        self.current_path.to_string_lossy().into_owned().into()
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
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::start_search))
            .on_action(cx.listener(Self::confirm_search))
            .on_action(cx.listener(Self::cancel_search))
            .on_action(cx.listener(Self::search_next))
            .on_action(cx.listener(Self::search_previous))
            .on_action(cx.listener(Self::refresh))
            .child(
                h_flex()
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
                    .child(Label::new(self.display_path())),
            )
            .when_some(
                self.searching.then(|| self.search_editor.clone()).flatten(),
                |this, search_editor| {
                    this.child(
                        h_flex()
                            .key_context("ProjectDirectorySearch")
                            .h_8()
                            .px_3()
                            .gap_1()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(Label::new("/"))
                            .child(div().flex_1().min_w_0().child(search_editor))
                            .when(!self.search_match_found, |this| {
                                this.child(Label::new("No match").color(Color::Error))
                            }),
                    )
                },
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
        self.focus_handle.clone()
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

pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let current_path = initial_directory(workspace, cx);
    let fs = workspace.app_state().fs.clone();
    let project = workspace.project().clone();
    let workspace_handle = workspace.weak_handle();
    let directory_view = cx.new(|cx| {
        ProjectDirectoryView::new(fs, project, workspace_handle, current_path, window, cx)
    });
    workspace.add_item_to_active_pane(Box::new(directory_view), None, true, window, cx);
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use menu::Confirm;
    use project::FakeFs;
    use serde_json::json;
    use util::rel_path::rel_path;
    use workspace::{ItemHandle, MultiWorkspace, SurfaceRole};

    use super::*;

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

        workspace.update_in(cx, open);
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
                ["..", "directory", "root.txt"]
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

        workspace.update_in(cx, open);
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

        workspace.update_in(cx, open);
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

        workspace.update_in(cx, open);
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
                ["..", "existing.txt", "new.txt"]
            );
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/project/existing.txt"))
            );
        });
    }

    #[gpui::test]
    async fn test_incremental_search_selects_entry_and_restores_focus(cx: &mut TestAppContext) {
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

        workspace.update_in(cx, open);
        cx.run_until_parked();
        let directory_view = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ProjectDirectoryView>(cx)
            })
            .expect("directory view should be active");

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.start_search(&StartSearch, window, cx);
        });
        let search_editor = directory_view
            .read_with(cx, |directory_view, _| directory_view.search_editor.clone())
            .expect("search editor should exist");
        search_editor.update_in(cx, |editor, window, cx| {
            editor.handle_input("documents", window, cx);
            assert!(editor.focus_handle(cx).is_focused(window));
        });
        cx.run_until_parked();

        directory_view.read_with(cx, |directory_view, _| {
            assert!(directory_view.searching);
            assert!(directory_view.search_match_found);
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/Documents"))
            );
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.cancel_search(&CancelSearch, window, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert!(!directory_view.searching);
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/Alpha"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.start_search(&StartSearch, window, cx);
        });
        search_editor.update_in(cx, |editor, window, cx| {
            editor.handle_input("DOC", window, cx);
        });
        cx.run_until_parked();
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.confirm_search(&ConfirmSearch, window, cx);
            assert!(directory_view.focus_handle.is_focused(window));
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert!(!directory_view.searching);
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/Documents"))
            );
        });

        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.search_next(&SearchNext, window, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/Documents Archive"))
            );
        });
        directory_view.update_in(cx, |directory_view, window, cx| {
            directory_view.search_previous(&SearchPrevious, window, cx);
        });
        directory_view.read_with(cx, |directory_view, _| {
            assert_eq!(
                directory_view.selected_entry().map(|entry| entry.path),
                Some(PathBuf::from("/home/user/Documents"))
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

        workspace.update_in(cx, open);
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

        workspace.update_in(cx, open);
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
