use gpui::{App, FocusHandle, WeakFocusHandle, Window};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SurfaceRole {
    NormalBuffer,
    SpecialBuffer,
    PersistentPanel,
    Transient,
}

pub(crate) struct SurfaceFocusRestore {
    previous_focus_handle: Option<WeakFocusHandle>,
}

impl SurfaceFocusRestore {
    pub(crate) fn capture(window: &Window, cx: &App) -> Self {
        Self {
            previous_focus_handle: window.focused(cx).map(|handle| handle.downgrade()),
        }
    }

    pub(crate) fn restore(
        &self,
        surface_focus_handle: &FocusHandle,
        fallback_focus_handle: Option<&FocusHandle>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !surface_focus_handle.contains_focused(window, cx) {
            return;
        }

        let focus_handle = self
            .previous_focus_handle
            .as_ref()
            .and_then(WeakFocusHandle::upgrade)
            .filter(|focus_handle| focus_handle != surface_focus_handle)
            .or_else(|| {
                fallback_focus_handle
                    .filter(|focus_handle| *focus_handle != surface_focus_handle)
                    .cloned()
            });
        if let Some(focus_handle) = focus_handle {
            focus_handle.focus(window, cx);
        }
    }
}
