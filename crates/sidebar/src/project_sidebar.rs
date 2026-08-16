use std::{collections::HashMap, path::PathBuf};

use agent_settings::AgentSettings;
use gpui::{
    Action as _, App, Context, Decorations, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Pixels, Render, ScrollHandle, SharedString, TaskExt, WeakEntity, Window, px,
};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use serde::{Deserialize, Serialize};
use settings::Settings;
use theme::CLIENT_SIDE_DECORATION_ROUNDING;
use ui::{Divider, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt;
use workspace::{
    CloseWindow, MultiWorkspace, MultiWorkspaceEvent, Open, OpenMode, ProjectGroup,
    ProjectGroupKey, Sidebar, SidebarEvent, SidebarSide, ToggleWorkspaceSidebar, Workspace,
};

use crate::connect_remote;

const DEFAULT_WIDTH: Pixels = px(300.0);
const MIN_WIDTH: Pixels = px(200.0);
const MAX_WIDTH: Pixels = px(800.0);

#[derive(Default, Serialize, Deserialize)]
struct SerializedProjectSidebar {
    #[serde(default)]
    width: Option<f32>,
}

pub struct ProjectSidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    focus_handle: FocusHandle,
    selected_group: Option<ProjectGroupKey>,
    scroll_handle: ScrollHandle,
    width: Pixels,
}

impl ProjectSidebar {
    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe_in(
            &multi_workspace,
            window,
            |_this, _multi_workspace, _event: &MultiWorkspaceEvent, _window, cx| {
                cx.notify();
            },
        )
        .detach();

        Self {
            multi_workspace: multi_workspace.downgrade(),
            focus_handle: cx.focus_handle(),
            selected_group: None,
            scroll_handle: ScrollHandle::new(),
            width: DEFAULT_WIDTH,
        }
    }

    fn groups(&self, cx: &App) -> Vec<ProjectGroup> {
        self.multi_workspace
            .upgrade()
            .map(|multi_workspace| multi_workspace.read(cx).project_groups(cx))
            .unwrap_or_default()
    }

    fn active_group_key(&self, cx: &App) -> Option<ProjectGroupKey> {
        let multi_workspace = self.multi_workspace.upgrade()?;
        let multi_workspace = multi_workspace.read(cx);
        Some(multi_workspace.project_group_key_for_workspace(multi_workspace.workspace(), cx))
    }

    fn select_active_group(&mut self, cx: &mut Context<Self>) {
        let groups = self.groups(cx);
        self.selected_group = self
            .active_group_key(cx)
            .filter(|active_key| groups.iter().any(|group| group.key == *active_key));
        if let Some(selected_index) = self.selected_group_index(&groups) {
            self.scroll_handle.scroll_to_item(selected_index);
        }
        cx.notify();
    }

    fn selected_group_index(&self, groups: &[ProjectGroup]) -> Option<usize> {
        let selected_group = self.selected_group.as_ref()?;
        groups.iter().position(|group| group.key == *selected_group)
    }

    fn select_relative_group(&mut self, forward: bool, cx: &mut Context<Self>) {
        let groups = self.groups(cx);
        if groups.is_empty() {
            self.selected_group = None;
            cx.notify();
            return;
        }

        let next_index = match self.selected_group_index(&groups) {
            Some(selected_index) if forward => (selected_index + 1) % groups.len(),
            Some(selected_index) => (selected_index + groups.len() - 1) % groups.len(),
            None if forward => 0,
            None => groups.len() - 1,
        };
        self.selected_group = groups.get(next_index).map(|group| group.key.clone());
        self.scroll_handle.scroll_to_item(next_index);
        cx.notify();
    }

    fn select_boundary_group(&mut self, first: bool, cx: &mut Context<Self>) {
        let groups = self.groups(cx);
        let selected_index = if first {
            0
        } else {
            groups.len().saturating_sub(1)
        };
        self.selected_group = groups.get(selected_index).map(|group| group.key.clone());
        if self.selected_group.is_some() {
            self.scroll_handle.scroll_to_item(selected_index);
        }
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select_relative_group(true, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.select_relative_group(false, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_boundary_group(true, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select_boundary_group(false, cx);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let key = self
            .selected_group
            .clone()
            .or_else(|| self.active_group_key(cx));
        if let Some(key) = key
            && self.groups(cx).iter().any(|group| group.key == key)
        {
            self.activate_or_open_group(key, true, window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(window, cx);
    }

    fn labels_for_groups(groups: &[ProjectGroup]) -> Vec<SharedString> {
        let mut all_paths: Vec<PathBuf> = groups
            .iter()
            .flat_map(|group| group.key.path_list().paths().iter().cloned())
            .collect();
        all_paths.sort_unstable();
        all_paths.dedup();

        let path_details =
            util::disambiguate::compute_disambiguation_details(&all_paths, |path, detail| {
                project::path_suffix(path, detail)
            });
        let path_detail_map: HashMap<PathBuf, usize> =
            all_paths.into_iter().zip(path_details).collect();

        groups
            .iter()
            .map(|group| group.key.display_name(&path_detail_map))
            .collect()
    }

    fn workspace_for_group(&self, key: &ProjectGroupKey, cx: &App) -> Option<Entity<Workspace>> {
        let multi_workspace = self.multi_workspace.upgrade()?;
        let multi_workspace = multi_workspace.read(cx);
        multi_workspace
            .last_active_workspace_for_group(key, cx)
            .or_else(|| {
                multi_workspace.workspace_for_paths(key.path_list(), key.host().as_ref(), cx)
            })
    }

    fn activate_or_open_group(
        &mut self,
        key: ProjectGroupKey,
        dismiss_after_activation: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        if let Some(workspace) = self.workspace_for_group(&key, cx) {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.activate(workspace, None, window, cx);
                multi_workspace.retain_active_workspace(cx);
            });
            if dismiss_after_activation {
                self.dismiss(window, cx);
            }
            return;
        }

        let path_list = key.path_list().clone();
        let host = key.host();
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();
        let task = multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.find_or_create_workspace(
                path_list,
                host,
                Some(key),
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                None,
                OpenMode::Activate,
                None,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |_this, cx| {
            let result = task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            result?;
            if dismiss_after_activation {
                multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                    multi_workspace.close_sidebar(window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn dismiss(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            window.defer(cx, move |window, cx| {
                multi_workspace.update(cx, |multi_workspace, cx| {
                    multi_workspace.close_sidebar(window, cx);
                });
            });
        }
    }

    fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let on_left = self.side(cx) == SidebarSide::Left;
        let not_fullscreen = !window.is_fullscreen();
        let traffic_lights = cfg!(target_os = "macos") && not_fullscreen && on_left;
        let left_window_controls = !cfg!(target_os = "macos") && not_fullscreen && on_left;
        let right_window_controls = !cfg!(target_os = "macos") && not_fullscreen && !on_left;

        h_flex()
            .h(ui::utils::platform_title_bar_height(window))
            .map(|header| match window.window_decorations() {
                Decorations::Client { .. } => header.mt(px(-1.0)),
                Decorations::Server => header.mt_px().pb_px(),
            })
            .when(left_window_controls, |header| {
                header.children(platform_title_bar::render_left_window_controls(
                    cx.button_layout(),
                    Box::new(CloseWindow),
                    window,
                ))
            })
            .when(traffic_lights, |header| {
                header
                    .pl(px(ui::utils::TRAFFIC_LIGHT_PADDING))
                    .child(Divider::vertical())
            })
            .px_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Projects")
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("open-workspace", IconName::FolderAdd)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| Tooltip::for_action("Open Project", &Open::default(), cx))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(
                            Open {
                                create_new_window: Some(false),
                            }
                            .boxed_clone(),
                            cx,
                        );
                    }),
            )
            .child(
                IconButton::new("close-workspace-sidebar", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(|_, cx| {
                        Tooltip::for_action("Close Workspace Sidebar", &ToggleWorkspaceSidebar, cx)
                    })
                    .on_click(|_, window, cx| {
                        if let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() {
                            multi_workspace.update(cx, |multi_workspace, cx| {
                                multi_workspace.close_sidebar(window, cx);
                            });
                        }
                    }),
            )
            .when(right_window_controls, |header| {
                header.children(platform_title_bar::render_right_window_controls(
                    cx.button_layout(),
                    Box::new(CloseWindow),
                    window,
                ))
            })
    }

    fn cycle_group(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let groups = self.groups(cx);
        if groups.is_empty() {
            return;
        }

        let active_key = self.active_group_key(cx);
        let current_index = active_key
            .as_ref()
            .and_then(|active_key| groups.iter().position(|group| group.key == *active_key));
        let next_index = match current_index {
            Some(current_index) if forward => (current_index + 1) % groups.len(),
            Some(current_index) => (current_index + groups.len() - 1) % groups.len(),
            None => 0,
        };
        let Some(group) = groups.get(next_index) else {
            return;
        };
        self.activate_or_open_group(group.key.clone(), false, window, cx);
    }
}

impl Sidebar for ProjectSidebar {
    const SURFACE_ROLE: workspace::SurfaceRole = workspace::SurfaceRole::Transient;

    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width.unwrap_or(DEFAULT_WIDTH).clamp(MIN_WIDTH, MAX_WIDTH);
        cx.notify();
    }

    fn has_notifications(&self, _cx: &App) -> bool {
        false
    }

    fn side(&self, cx: &App) -> SidebarSide {
        AgentSettings::get_global(cx).sidebar_side()
    }

    fn prepare_for_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this, cx| this.select_active_group(cx))
                .log_err();
        });
    }

    fn cycle_project(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_group(forward, window, cx);
    }

    fn serialized_state(&self, _cx: &App) -> Option<String> {
        serde_json::to_string(&SerializedProjectSidebar {
            width: Some(f32::from(self.width)),
        })
        .log_err()
    }

    fn restore_serialized_state(
        &mut self,
        state: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Ok(serialized) = serde_json::from_str::<SerializedProjectSidebar>(state)
            && let Some(width) = serialized.width
        {
            self.width = px(width).clamp(MIN_WIDTH, MAX_WIDTH);
        }
        cx.notify();
    }
}

impl EventEmitter<SidebarEvent> for ProjectSidebar {}

impl Focusable for ProjectSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ProjectSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = self.groups(cx);
        let labels = Self::labels_for_groups(&groups);
        let active_key = self.active_group_key(cx);
        let on_left = self.side(cx) == SidebarSide::Left;
        let ui_font = theme_settings::setup_ui_font(window, cx);
        let colors = cx.theme().colors();
        let background = colors
            .title_bar_background
            .blend(colors.panel_background.opacity(0.25));

        v_flex()
            .id("project-sidebar")
            .track_focus(&self.focus_handle)
            .key_context("ProjectSidebar")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .font(ui_font)
            .h_full()
            .w(self.width)
            .map(|element| match window.window_decorations() {
                Decorations::Server => element,
                Decorations::Client { tiling, .. } => element
                    .absolute()
                    .top(if tiling.top { px(0.0) } else { px(-1.0) })
                    .bottom(if tiling.bottom { px(0.0) } else { px(-1.0) })
                    .when(!tiling.top, |element| element.pt_px())
                    .when(!tiling.bottom, |element| element.pb_px())
                    .when(on_left, |element| {
                        element
                            .right(px(0.0))
                            .left(if tiling.left { px(0.0) } else { px(-1.0) })
                            .when(!tiling.left, |element| element.pl(px(1.0)))
                            .when(!(tiling.top || tiling.left), |element| {
                                element.rounded_tl(CLIENT_SIDE_DECORATION_ROUNDING)
                            })
                            .when(!(tiling.bottom || tiling.left), |element| {
                                element.rounded_bl(CLIENT_SIDE_DECORATION_ROUNDING)
                            })
                    })
                    .when(!on_left, |element| {
                        element
                            .left(px(0.0))
                            .right(if tiling.right { px(0.0) } else { px(-1.0) })
                            .when(!tiling.right, |element| element.pr(px(1.0)))
                            .when(!(tiling.top || tiling.right), |element| {
                                element.rounded_tr(CLIENT_SIDE_DECORATION_ROUNDING)
                            })
                            .when(!(tiling.bottom || tiling.right), |element| {
                                element.rounded_br(CLIENT_SIDE_DECORATION_ROUNDING)
                            })
                    }),
            })
            .bg(background)
            .when(on_left, |element| element.border_r_1())
            .when(!on_left, |element| element.border_l_1())
            .border_color(colors.border)
            .child(self.render_header(window, cx))
            .child(
                v_flex()
                    .id("project-groups-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .p_1()
                    .children(groups.into_iter().zip(labels).enumerate().map(
                        |(index, (group, label))| {
                            let is_active = active_key
                                .as_ref()
                                .is_some_and(|active_key| *active_key == group.key);
                            let is_selected = self
                                .selected_group
                                .as_ref()
                                .is_some_and(|selected_group| *selected_group == group.key);
                            ListItem::new(("project-group", index))
                                .inset(true)
                                .spacing(ListItemSpacing::Sparse)
                                .toggle_state(is_active)
                                .focused(is_selected)
                                .start_slot(
                                    Icon::new(IconName::Folder).size(IconSize::Small).color(
                                        if is_active {
                                            Color::Accent
                                        } else {
                                            Color::Muted
                                        },
                                    ),
                                )
                                .child(Label::new(label).truncate())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.activate_or_open_group(
                                        group.key.clone(),
                                        true,
                                        window,
                                        cx,
                                    );
                                }))
                        },
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use db::AppDatabase;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use workspace::dock::{DockPosition, test::TestPanel};

    use super::*;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_global(AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    #[test]
    fn test_project_sidebar_surface_role() {
        assert_eq!(
            <ProjectSidebar as Sidebar>::SURFACE_ROLE,
            workspace::SurfaceRole::Transient
        );
    }

    #[gpui::test]
    async fn test_picker_navigation_activates_selected_project(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project-a", json!({ "a.rs": "" })).await;
        fs.insert_tree("/project-b", json!({ "b.rs": "" })).await;
        let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
        let project_b = project::Project::test(fs, ["/project-b".as_ref()], cx).await;
        let key_a = project_a.read_with(cx, |project, cx| project.project_group_key(cx));
        let key_b = project_b.read_with(cx, |project, cx| project.project_group_key(cx));

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(project_b, window, cx);
        });

        let multi_workspace_for_sidebar = multi_workspace.clone();
        let sidebar = cx.update(|window, cx| {
            cx.new(|cx| ProjectSidebar::new(multi_workspace_for_sidebar, window, cx))
        });
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.register_sidebar(sidebar.clone(), cx);
        });
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.toggle_sidebar(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            sidebar.read_with(cx, |sidebar, _cx| sidebar.selected_group.clone()),
            Some(key_b.clone()),
            "focusing the picker should select the active project",
        );

        cx.dispatch_action(SelectNext);
        assert_eq!(
            sidebar.read_with(cx, |sidebar, _cx| sidebar.selected_group.clone()),
            Some(key_a.clone()),
            "moving the picker selection must not activate the project",
        );
        multi_workspace.read_with(cx, |multi_workspace, cx| {
            assert_eq!(
                multi_workspace.project_group_key_for_workspace(multi_workspace.workspace(), cx),
                key_b,
            );
        });

        cx.dispatch_action(Confirm);
        cx.run_until_parked();
        multi_workspace.read_with(cx, |multi_workspace, cx| {
            assert_eq!(
                multi_workspace.project_group_key_for_workspace(multi_workspace.workspace(), cx),
                key_a,
                "confirming should activate the selected project's workspace",
            );
            assert!(
                !multi_workspace.sidebar_open(),
                "confirming should close the project picker",
            );
        });
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            let pane = multi_workspace.workspace().read(cx).active_pane().clone();
            assert!(
                pane.read(cx).focus_handle(cx).contains_focused(window, cx),
                "confirming should focus the selected workspace",
            );
        });
    }

    #[gpui::test]
    async fn test_cancel_closes_project_picker_and_restores_previous_focus(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let panel = cx.new(|cx| TestPanel::new(DockPosition::Bottom, 100, cx));
        let previous_focus = panel.read_with(cx, |panel, cx| panel.focus_handle(cx));
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.add_panel(panel, window, cx);
            multi_workspace.focus_panel::<TestPanel>(window, cx);
        });

        let multi_workspace_for_sidebar = multi_workspace.clone();
        let sidebar = cx.update(|window, cx| {
            cx.new(|cx| ProjectSidebar::new(multi_workspace_for_sidebar, window, cx))
        });
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.register_sidebar(sidebar.clone(), cx);
        });
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.toggle_sidebar(window, cx);
        });
        let sidebar_focus = sidebar.read_with(cx, |sidebar, cx| sidebar.focus_handle(cx));
        cx.update(|window, _cx| {
            assert!(sidebar_focus.is_focused(window));
        });

        cx.dispatch_action(Cancel);
        cx.run_until_parked();

        multi_workspace.read_with(cx, |multi_workspace, _cx| {
            assert!(!multi_workspace.sidebar_open());
        });
        cx.update(|window, _cx| {
            assert!(previous_focus.is_focused(window));
        });
    }

    #[gpui::test]
    async fn test_cancel_project_picker_uses_workspace_focus_fallback(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace_focus = multi_workspace.read_with(cx, |multi_workspace, cx| {
            let pane = multi_workspace.workspace().read(cx).active_pane().clone();
            pane.read(cx).focus_handle(cx)
        });
        let previous_focus = cx.update(|window, cx| {
            let previous_focus = cx.focus_handle();
            window.focus(&previous_focus, cx);
            previous_focus
        });

        let multi_workspace_for_sidebar = multi_workspace.clone();
        let sidebar = cx.update(|window, cx| {
            cx.new(|cx| ProjectSidebar::new(multi_workspace_for_sidebar, window, cx))
        });
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.register_sidebar(sidebar, cx);
        });
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.toggle_sidebar(window, cx);
        });
        drop(previous_focus);

        cx.dispatch_action(Cancel);
        cx.run_until_parked();

        multi_workspace.read_with(cx, |multi_workspace, _cx| {
            assert!(!multi_workspace.sidebar_open());
        });
        cx.update(|window, cx| {
            assert!(workspace_focus.contains_focused(window, cx));
        });
    }
}
