use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, App, ClickEvent, ElementId, IntoElement, MouseButton,
    Pixels, RenderOnce, Window, prelude::*, pulsating_between, px,
};
use ui::{
    AgentThreadStatus, Color, CommonAnimationExt, HighlightedLabel, Icon, IconName, IconSize, Label,
    LabelSize, SharedString, Tab, prelude::*,
};

/// A single, compact row in the agent sidebar's main thread list.
///
/// Deliberately minimal compared to [`ui::ThreadItem`] (used by the history /
/// archive view): one line, no worktree chips, diff stats, or project footer.
/// The leading slot only renders a glyph while the thread is in an active
/// state (running / waiting / error / just-finished) or for an explicit
/// `idle_icon` (drafts); otherwise the slot is kept empty but sized so titles
/// never shift horizontally as state changes.
#[derive(IntoElement)]
pub struct ChatItem {
    id: ElementId,
    title: SharedString,
    /// Glyph shown in the idle state. `None` keeps the leading slot empty.
    idle_icon: Option<IconName>,
    idle_icon_color: Option<Color>,
    status: AgentThreadStatus,
    notified: bool,
    title_color: Color,
    title_generating: bool,
    highlight_positions: Vec<usize>,
    timestamp: SharedString,
    selected: bool,
    focused: bool,
    hovered: bool,
    indent: Pixels,
    /// Replaces the title label entirely (e.g. an inline rename editor).
    title_slot: Option<AnyElement>,
    /// Shown on the trailing edge while hovered, in place of the timestamp.
    action_slot: Option<AnyElement>,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    on_hover: Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>,
}

impl ChatItem {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            idle_icon: None,
            idle_icon_color: None,
            status: AgentThreadStatus::default(),
            notified: false,
            title_color: Color::Default,
            title_generating: false,
            highlight_positions: Vec::new(),
            timestamp: SharedString::default(),
            selected: false,
            focused: false,
            hovered: false,
            indent: px(0.),
            title_slot: None,
            action_slot: None,
            on_click: None,
            on_hover: Box::new(|_, _, _| {}),
        }
    }

    pub fn idle_icon(mut self, icon: Option<IconName>) -> Self {
        self.idle_icon = icon;
        self
    }

    pub fn idle_icon_color(mut self, color: Color) -> Self {
        self.idle_icon_color = Some(color);
        self
    }

    pub fn status(mut self, status: AgentThreadStatus) -> Self {
        self.status = status;
        self
    }

    pub fn notified(mut self, notified: bool) -> Self {
        self.notified = notified;
        self
    }

    pub fn title_color(mut self, color: Color) -> Self {
        self.title_color = color;
        self
    }

    pub fn title_generating(mut self, generating: bool) -> Self {
        self.title_generating = generating;
        self
    }

    pub fn highlight_positions(mut self, positions: Vec<usize>) -> Self {
        self.highlight_positions = positions;
        self
    }

    pub fn timestamp(mut self, timestamp: impl Into<SharedString>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn indent(mut self, indent: Pixels) -> Self {
        self.indent = indent;
        self
    }

    pub fn title_slot(mut self, slot: impl IntoElement) -> Self {
        self.title_slot = Some(slot.into_any_element());
        self
    }

    pub fn action_slot(mut self, slot: impl IntoElement) -> Self {
        self.action_slot = Some(slot.into_any_element());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Box::new(handler);
        self
    }
}

impl RenderOnce for ChatItem {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors();

        let hover_color = colors
            .element_active
            .blend(colors.element_background.opacity(0.2));

        // The leading slot is always 16px wide so titles stay aligned whether
        // or not a glyph is present.
        let icon_slot = h_flex().size_4().flex_none().justify_center();
        let icon_slot = if self.status == AgentThreadStatus::Running {
            icon_slot.child(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .with_rotate_animation(2),
            )
        } else if self.status == AgentThreadStatus::Error {
            icon_slot.child(
                Icon::new(IconName::Close)
                    .size(IconSize::Small)
                    .color(Color::Error),
            )
        } else if self.status == AgentThreadStatus::WaitingForConfirmation {
            icon_slot.child(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            )
        } else if self.notified {
            icon_slot.child(
                Icon::new(IconName::Circle)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
        } else if let Some(idle_icon) = self.idle_icon {
            icon_slot.child(
                Icon::new(idle_icon)
                    .size(IconSize::Small)
                    .color(self.idle_icon_color.unwrap_or(Color::Muted)),
            )
        } else {
            icon_slot
        };

        let title_label = if let Some(title_slot) = self.title_slot {
            title_slot
        } else if self.title_generating {
            Label::new(self.title)
                .color(Color::Muted)
                .truncate()
                .with_animation(
                    "generating-title",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 0.8)),
                    |label, delta| label.alpha(delta),
                )
                .into_any_element()
        } else if self.highlight_positions.is_empty() {
            Label::new(self.title)
                .color(self.title_color)
                .truncate()
                .into_any_element()
        } else {
            HighlightedLabel::new(self.title, self.highlight_positions)
                .color(self.title_color)
                .truncate()
                .into_any_element()
        };

        let trailing = if self.hovered {
            self.action_slot
        } else if self.timestamp.is_empty() {
            None
        } else {
            Some(
                Label::new(self.timestamp)
                    .size(LabelSize::Small)
                    .color(Color::Disabled)
                    .into_any_element(),
            )
        };

        h_flex()
            .id(self.id)
            .group("chat-item")
            .relative()
            .flex_shrink_0()
            .w_full()
            .h(Tab::content_height(cx))
            .px_1p5()
            .pr_2()
            .gap_2()
            .when(self.indent > px(0.), |this| this.pl(self.indent))
            .justify_between()
            .cursor_pointer()
            .border_1()
            .border_color(gpui::transparent_black())
            .when(self.selected, |this| this.bg(colors.element_active))
            .when(self.focused, |this| this.border_color(colors.border_focused))
            .hover(|this| this.bg(hover_color))
            .when_some(self.on_click, |this, on_click| this.on_click(on_click))
            .on_hover(self.on_hover)
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(icon_slot)
                    .child(title_label),
            )
            .when_some(trailing, |this, trailing| {
                this.child(
                    h_flex()
                        .flex_none()
                        .child(trailing)
                        // Don't let clicks on hover actions activate the row.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                )
            })
    }
}
