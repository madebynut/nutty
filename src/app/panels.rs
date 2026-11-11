//! Bounded, collapsible sidebar and pane dock sizing.

use std::{cell::Cell, rc::Rc};

use gpui_kit::component::resizable::{HandleEdge, resize_handle};
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, Context, DragMoveEvent, IntoElement, Pixels, Render,
    WeakEntity, Window, canvas, div, prelude::*,
};
use uuid::Uuid;

use super::{Tool, WorkspaceView};
use crate::assistant_session::Mode;

/// Common baseline for sidebar settings, folded docks and composer actions.
pub(super) const FOOTER_BAR: f32 = 40.;
pub(super) const DOCK_BAR: f32 = FOOTER_BAR;
/// Shared bottom edge for the application title, terminal tabs and tool sidebar title.
pub(super) const HEADER_BAR: f32 = 48.;
const SIDEBAR_COLLAPSE: f32 = 152.;
const DOCK_COLLAPSE: f32 = 112.;

#[derive(Debug)]
pub(super) struct PanelSize {
    pub open: bool,
    pub(super) preferred: f32,
    collapse: f32,
}

impl PanelSize {
    pub fn sidebar(width: f32) -> Self {
        Self {
            open: true,
            preferred: width,
            collapse: SIDEBAR_COLLAPSE,
        }
    }

    pub fn dock() -> Self {
        Self {
            open: false,
            preferred: 240.,
            collapse: DOCK_COLLAPSE,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    pub fn extent(&self, limit: f32) -> f32 {
        if self.open && limit >= self.collapse {
            self.preferred.min(limit)
        } else {
            0.
        }
    }

    pub fn resize(&mut self, requested: f32, limit: f32) {
        if !requested.is_finite() || !limit.is_finite() {
            return;
        }
        let extent = requested.clamp(0., limit.max(0.));
        self.open = extent >= self.collapse;
        // Remember a useful expanded size when folding the panel.
        if self.open {
            self.preferred = extent;
        }
    }
}

/// Switching the visible surface never changes either panel's size or expansion.
pub(super) struct PaneDock {
    pub(super) mode: Mode,
    pub(super) shell: PanelSize,
    pub(super) agent: PanelSize,
}

impl Default for PaneDock {
    fn default() -> Self {
        let mut agent = PanelSize::dock();
        agent.open = true;
        Self {
            mode: Mode::Shell,
            shell: PanelSize::dock(),
            agent,
        }
    }
}

impl PaneDock {
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }
    pub fn size(&self) -> &PanelSize {
        match self.mode {
            Mode::Shell => &self.shell,
            Mode::Agent => &self.agent,
        }
    }
    pub fn size_mut(&mut self) -> &mut PanelSize {
        match self.mode {
            Mode::Shell => &mut self.shell,
            Mode::Agent => &mut self.agent,
        }
    }
}

pub(super) type PanelBounds = Rc<Cell<Bounds<Pixels>>>;

pub(super) fn measure(
    bounds: PanelBounds,
    workspace: WeakEntity<WorkspaceView>,
    pane: Option<Uuid>,
) -> AnyElement {
    canvas(
        move |new, window, cx| {
            let previous = bounds.replace(new);
            if previous.size != new.size {
                let _ = workspace.update(cx, |view, cx| {
                    if pane == view.active_pane
                        && let Some(pane) = pane.and_then(|id| view.panes.get(&id))
                        && pane.dock().extent(f32::from(previous.size.height) * 0.5) > 0.
                        && !pane.dock_expanded()
                    {
                        pane.view
                            .update(cx, |terminal, cx| terminal.focus(window, cx));
                    }
                    cx.notify();
                });
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .size_full()
    .into_any_element()
}

#[derive(Clone, Copy)]
pub(super) enum PanelEdge {
    Left,
    Right,
    Dock(Uuid),
}

#[derive(Clone)]
pub(super) struct ResizePanel(pub PanelEdge);

impl Render for ResizePanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub(super) fn handle(id: String, edge: PanelEdge) -> AnyElement {
    let axis = if matches!(edge, PanelEdge::Dock(_)) {
        gpui_kit::Axis::Vertical
    } else {
        gpui_kit::Axis::Horizontal
    };
    let inside = if matches!(edge, PanelEdge::Left) {
        HandleEdge::Trailing
    } else {
        HandleEdge::Leading
    };
    resize_handle(id, axis)
        .inside(inside)
        .on_drag(ResizePanel(edge), |drag, _, _, cx| cx.new(|_| drag.clone()))
        .into_any_element()
}

impl WorkspaceView {
    pub(super) fn sidebar_limit(&self, edge: PanelEdge) -> f32 {
        let width = f32::from(self.panel_bounds.get().size.width);
        let maximum = if matches!(edge, PanelEdge::Left) {
            420.
        } else {
            520.
        };
        if width > 0. {
            (width * 0.32).min(maximum)
        } else {
            maximum
        }
    }

    pub(super) fn resize_panel(
        &mut self,
        event: &DragMoveEvent<ResizePanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = self.panel_bounds.get();
        match event.drag(cx).0 {
            PanelEdge::Left => {
                let requested = f32::from(event.event.position.x - bounds.left());
                self.sidebar
                    .resize(requested, self.sidebar_limit(PanelEdge::Left));
                if !self.sidebar.open {
                    self.focus_terminal(window, cx);
                }
            }
            PanelEdge::Right => {
                let requested = f32::from(bounds.right() - event.event.position.x);
                self.tool_size
                    .resize(requested, self.sidebar_limit(PanelEdge::Right));
                if !self.tool_size.open {
                    self.tool = Tool::Hidden;
                    self.focus_terminal(window, cx);
                }
            }
            PanelEdge::Dock(id) => {
                self.activate_pane(id, cx);
                if let Some(pane) = self.panes.get_mut(&id) {
                    let bounds = pane.bounds.get();
                    let requested = f32::from(bounds.bottom() - event.event.position.y);
                    pane.dock_mut()
                        .resize(requested, f32::from(bounds.size.height) * 0.5);
                    if !pane.dock().open {
                        pane.view
                            .update(cx, |terminal, cx| terminal.focus(window, cx));
                    }
                }
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_shell_and_agent_keeps_each_panels_expansion_and_size() {
        let mut dock = PaneDock::default();
        assert!(!dock.size().open, "new command senders start collapsed");
        dock.set_mode(Mode::Agent);
        dock.size_mut().resize(350., 400.);
        dock.set_mode(Mode::Shell);
        assert!(
            !dock.size().open,
            "returning from Agent cannot open the command sender"
        );
        dock.size_mut().resize(190., 400.);
        dock.set_mode(Mode::Agent);
        assert_eq!(dock.size().extent(400.), 350.);
        dock.size_mut().resize(80., 400.);
        dock.set_mode(Mode::Shell);
        assert_eq!(
            dock.size().extent(400.),
            190.,
            "Agent resizing cannot change the sender"
        );
        dock.set_mode(Mode::Agent);
        assert!(
            !dock.size().open,
            "AI expansion is remembered independently too"
        );
        assert!(
            !PaneDock::default().size().open,
            "new split panes have their own state"
        );
    }

    #[test]
    fn collapsing_restores_last_size_and_window_resize_never_exceeds_limit() {
        let mut panel = PanelSize::sidebar(238.);
        panel.resize(700., 400.);
        assert_eq!(panel.extent(600.), 400.);
        panel.resize(120., 400.);
        assert!(!panel.open);
        panel.toggle();
        assert_eq!(panel.extent(300.), 300.);
        assert_eq!(
            panel.extent(600.),
            400.,
            "temporary bounds must not lose the user's preferred width"
        );
    }

    #[test]
    fn dock_leaves_half_the_pane_visible_even_after_resize_and_reopens_after_collapse() {
        let mut dock = PanelSize::dock();
        dock.resize(999., 600. * 0.5);
        assert_eq!(dock.extent(600. * 0.5), 300.);
        assert_eq!(dock.extent(280. * 0.5), 140.);
        dock.resize(-30., 140.);
        assert_eq!(dock.extent(300.), 0.);
        dock.toggle();
        assert_eq!(dock.extent(300.), 300.);
        assert_eq!(dock.extent(80.), 0., "tiny split panes fold their dock");
        dock.resize(f32::NAN, 300.);
        assert_eq!(dock.extent(300.), 300.);
    }
}
