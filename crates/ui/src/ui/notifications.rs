//! Transient toast helpers shared by every screen.
//!
//! The notification modal lives in this module as well; toasts are delivered
//! through gpui-component's `NotificationList` so stacking, auto-hide, and the
//! enter/exit animations stay consistent across the app.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::*;

use crate::app::App;

#[derive(Clone, Debug)]
pub(crate) struct ToastSpec {
    pub(crate) tone: NotificationType,
    pub(crate) message: SharedString,
}

/// One failure domain that crossed its last-seen count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureNotice {
    Compression { count: i64 },
    Daemon { count: usize },
}

/// Returns the failures that appeared since `previous`. `None` means the
/// baseline has not been seeded yet, so existing failures stay silent.
pub(crate) fn failure_transition(
    previous: Option<(i64, usize)>,
    next: (i64, usize),
) -> Vec<FailureNotice> {
    let Some((previous_compression, previous_daemon)) = previous else {
        return Vec::new();
    };
    let mut notices = Vec::new();
    if next.0 > previous_compression {
        notices.push(FailureNotice::Compression { count: next.0 });
    }
    if next.1 > previous_daemon {
        notices.push(FailureNotice::Daemon { count: next.1 });
    }
    notices
}

impl App {
    pub(crate) fn toast(
        &mut self,
        tone: NotificationType,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.enqueue_toast(
            ToastSpec {
                tone,
                message: message.into(),
            },
            cx,
        );
    }

    pub(crate) fn toast_info(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast(NotificationType::Info, message, cx);
    }

    pub(crate) fn toast_success(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.toast(NotificationType::Success, message, cx);
    }

    pub(crate) fn toast_warning(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.toast(NotificationType::Warning, message, cx);
    }

    pub(crate) fn toast_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast(NotificationType::Error, message, cx);
    }

    fn enqueue_toast(&mut self, spec: ToastSpec, cx: &mut Context<Self>) {
        if !self.toasts_ready {
            self.pending_toasts.push(spec);
            cx.notify();
            return;
        }
        self.push_toast_now(spec, cx);
    }

    fn push_toast_now(&mut self, spec: ToastSpec, cx: &mut Context<Self>) {
        let note = Notification::new()
            .message(spec.message)
            .with_type(spec.tone);
        let handle = self.window_handle;
        let _ = handle.update(cx, |_, window, cx| {
            window.push_notification(note, cx);
        });
    }

    /// Drains toasts produced before the component Root plugin was mounted.
    pub(crate) fn flush_pending_toasts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = std::mem::take(&mut self.pending_toasts);
        for spec in pending {
            let note = Notification::new()
                .message(spec.message)
                .with_type(spec.tone);
            window.push_notification(note, cx);
        }
    }
}
