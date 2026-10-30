//! Style dispatch for toasts, the clipboard feedback, the config diagnostic and
//! the mode bar: modern draws through `modern::draw::toast`, classic keeps its
//! existing functions untouched.

use super::modern::draw::toast;
use super::*;
use crate::ui::chrome::state::StateKind;

impl ClientShellState {
    /// The modern mode bar's extras, `None` under `ui.style = "classic"`.
    pub(super) fn modern_mode_bar(&self) -> Option<render::ModernModeBar<'_>> {
        self.config
            .chrome
            .is_modern()
            .then_some(render::ModernModeBar {
                config: &self.config,
                summary: self.attention_summary,
            })
    }

    fn visible_attention(&self) -> Option<&modern::model::team::PaneAttention> {
        self.team_sidebar
            .as_ref()
            .map(|team| &team.modern.attention)
    }

    pub(super) fn draw_notification(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        notification: &ClientVisibleNotification,
        top_offset: u16,
        phase: super::animation::NotificationTransitionPhase,
    ) -> Rect {
        if !self.config.chrome.is_modern() {
            return notifications::render_visible_notification_with_phase(
                buffer,
                area,
                notification,
                self.config.toast_position,
                top_offset,
                &self.config.palette,
                phase,
            );
        }
        let event = &notification.event;
        toast::draw_card(
            buffer,
            area,
            toast::visible_state(event, self.visible_attention()),
            &event.title,
            event.body.as_deref().unwrap_or_default(),
            event.position.unwrap_or(self.config.toast_position),
            top_offset,
            &self.config,
            phase,
        )
    }

    pub(super) fn draw_mobile_notification(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        notification: &ClientVisibleNotification,
        offset_for_warning: bool,
        phase: super::animation::NotificationTransitionPhase,
    ) -> Rect {
        if !self.config.chrome.is_modern() {
            return notifications::render_mobile_notification_banner_with_phase(
                buffer,
                area,
                notification,
                offset_for_warning,
                &self.config.palette,
                phase,
            );
        }
        let event = &notification.event;
        toast::draw_banner(
            buffer,
            area,
            toast::visible_state(event, self.visible_attention()),
            &event.title,
            event.body.as_deref(),
            offset_for_warning,
            &self.config,
            phase,
        )
    }

    fn endpoint_notice_state(notice: &ClientVisibleEndpointNotice) -> StateKind {
        match notice.key.kind {
            ClientEndpointNoticeKind::Unsupported | ClientEndpointNoticeKind::Rejected => {
                StateKind::Failed
            }
            ClientEndpointNoticeKind::Timeout | ClientEndpointNoticeKind::Unavailable => {
                StateKind::Blocked
            }
        }
    }

    pub(super) fn draw_endpoint_notice(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        notice: &ClientVisibleEndpointNotice,
        top_offset: u16,
        phase: super::animation::NotificationTransitionPhase,
    ) -> Rect {
        if !self.config.chrome.is_modern() {
            return endpoint_notices::render_notice_with_phase(
                buffer,
                area,
                notice,
                top_offset,
                &self.config.palette,
                phase,
            );
        }
        toast::draw_card(
            buffer,
            area,
            Self::endpoint_notice_state(notice),
            &notice.title,
            &notice.body,
            crate::config::ToastAsterPosition::TopRight,
            top_offset,
            &self.config,
            phase,
        )
    }

    pub(super) fn draw_mobile_endpoint_notice(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        notice: &ClientVisibleEndpointNotice,
        offset_for_warning: bool,
        phase: super::animation::NotificationTransitionPhase,
    ) -> Rect {
        if !self.config.chrome.is_modern() {
            return endpoint_notices::render_mobile_banner_with_phase(
                buffer,
                area,
                notice,
                offset_for_warning,
                &self.config.palette,
                phase,
            );
        }
        toast::draw_banner(
            buffer,
            area,
            Self::endpoint_notice_state(notice),
            &notice.title,
            Some(&notice.body),
            offset_for_warning,
            &self.config,
            phase,
        )
    }

    pub(super) fn draw_config_diagnostic(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        message: &str,
        covered: impl FnMut(Rect),
    ) -> u16 {
        if self.config.chrome.is_modern() {
            toast::draw_config_diagnostic(buffer, area, message, &self.config, covered)
        } else {
            crate::ui::render_config_diagnostic_buffer(
                buffer,
                area,
                message,
                &self.config.palette,
                covered,
            )
        }
    }

    pub(super) fn feedback_offset(
        &self,
        area: Rect,
        feedback: &crate::app::state::CopyFeedback,
        base_offset: u16,
        toast_rect: Rect,
    ) -> u16 {
        if self.config.chrome.is_modern() {
            toast::feedback_offset(
                area,
                feedback,
                base_offset,
                self.config.clipboard_toast_position,
                toast_rect,
            )
        } else {
            crate::ui::copy_feedback_offset_for_toast(
                area,
                feedback,
                base_offset,
                self.config.clipboard_toast_position,
                toast_rect,
            )
        }
    }

    pub(super) fn draw_copy_feedback(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        feedback: &crate::app::state::CopyFeedback,
        offset: u16,
    ) -> Rect {
        if self.config.chrome.is_modern() {
            toast::draw_feedback(
                buffer,
                area,
                feedback,
                offset,
                self.config.clipboard_toast_position,
                &self.config,
            )
        } else {
            crate::ui::render_copy_feedback_buffer(
                buffer,
                area,
                feedback,
                offset,
                self.config.clipboard_toast_position,
                &self.config.palette,
            )
        }
    }
}
