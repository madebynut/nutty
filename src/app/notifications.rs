//! Compact success feedback with bounded lifetime and replacement-safe transitions.

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, Context, IntoElement, Window, div, prelude::*, px,
    rgb,
};
use uuid::Uuid;

use super::{WorkspaceView, icon_button};

const VISIBLE_DURATION: Duration = Duration::from_secs(3);
const TRANSITION_DURATION: Duration = Duration::from_millis(180);

struct Notice {
    id: Uuid,
    message: String,
    closing: bool,
}

#[derive(Default)]
pub(super) struct SuccessNotification {
    notice: Option<Notice>,
}

impl SuccessNotification {
    fn show(&mut self, message: String) -> Uuid {
        let id = Uuid::new_v4();
        self.notice = Some(Notice {
            id,
            message,
            closing: false,
        });
        id
    }

    fn begin_close(&mut self, id: Uuid) -> bool {
        if let Some(notice) = &mut self.notice
            && notice.id == id
            && !notice.closing
        {
            notice.closing = true;
            return true;
        }
        false
    }

    fn dismiss(&mut self, id: Uuid) -> bool {
        if self.notice.as_ref().is_some_and(|notice| notice.id == id) {
            self.notice = None;
            return true;
        }
        false
    }
}

impl WorkspaceView {
    pub(super) fn notify_success(&mut self, message: &str, cx: &mut Context<Self>) {
        self.status = None;
        let id = self.success_notification.show(message.to_owned());
        self._success_notification_timer = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(VISIBLE_DURATION).await;
            let closing = view.update(cx, |view, cx| {
                let closing = view.success_notification.begin_close(id);
                if closing {
                    cx.notify();
                }
                closing
            });
            if !matches!(closing, Ok(true)) {
                return;
            }
            cx.background_executor().timer(TRANSITION_DURATION).await;
            let _ = view.update(cx, |view, cx| {
                if view.success_notification.dismiss(id) {
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    pub(super) fn render_success_notification(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let notice = self.success_notification.notice.as_ref()?;
        let p = self.palette();
        let id = notice.id;
        let closing = notice.closing;
        let width = (f32::from(window.viewport_size().width) - 32.).clamp(1., 360.);
        Some(
            div()
                .absolute()
                .top(px(56.))
                .w(px(width))
                .occlude()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(p.success))
                .bg(rgb(p.surface))
                .text_color(rgb(p.success))
                .text_size(px(13.))
                .child(Icon::new(IconName::Check).size_4().flex_shrink_0())
                .child(div().flex_1().min_w_0().child(notice.message.clone()))
                .child(
                    icon_button(
                        format!("dismiss-success-{id}"),
                        IconName::X,
                        crate::i18n::text("关闭消息"),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if view.success_notification.dismiss(id) {
                            view._success_notification_timer = None;
                            cx.notify();
                        }
                    })),
                )
                .with_animation(
                    format!("success-notification-{id}-{closing}"),
                    Animation::new(TRANSITION_DURATION),
                    move |notice, progress| {
                        let progress = 1. - (1. - progress).powi(3);
                        let visible = if closing { 1. - progress } else { progress };
                        notice
                            .right(px(16. - (width + 32.) * (1. - visible)))
                            .opacity(visible)
                    },
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_expiry_and_dismissal_cannot_remove_a_new_success() {
        let mut notifications = SuccessNotification::default();
        let first = notifications.show("first save".into());
        assert!(notifications.begin_close(first));
        let second = notifications.show("second save".into());
        assert!(!notifications.begin_close(first));
        assert!(!notifications.dismiss(first));
        let visible = notifications
            .notice
            .as_ref()
            .expect("second save is visible");
        assert_eq!(visible.message, "second save");
        assert!(!visible.closing, "replacement starts a fresh entrance");
        assert!(notifications.begin_close(second));
        assert!(notifications.dismiss(second));
        assert!(notifications.notice.is_none());
        assert!(
            !notifications.begin_close(second),
            "manual dismissal can precede expiry"
        );
    }
}
