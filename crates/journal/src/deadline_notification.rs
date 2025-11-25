use gpui::{
    App, Context, EventEmitter, FontWeight, IntoElement, PlatformDisplay, Size, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowKind, WindowOptions,
    linear_color_stop, linear_gradient, point, px,
};
use std::path::PathBuf;
use std::rc::Rc;
use theme;
use ui::{Render, prelude::*};
use workspace::Workspace;

pub struct DeadlineNotification {
    title: SharedString,
    task_name: SharedString,
    deadline_date: SharedString,
    urgency: SharedString,
    pub file_path: PathBuf,
    pub line_number: u32,
    pub workspace: WeakEntity<Workspace>,
}

impl DeadlineNotification {
    pub fn new(
        task_name: impl Into<SharedString>,
        deadline_date: impl Into<SharedString>,
        urgency: impl Into<SharedString>,
        file_path: PathBuf,
        line_number: u32,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        Self {
            title: "Org-Mode Deadline Reminder".into(),
            task_name: task_name.into(),
            deadline_date: deadline_date.into(),
            urgency: urgency.into(),
            file_path,
            line_number,
            workspace,
        }
    }

    pub fn window_options(screen: Rc<dyn PlatformDisplay>, _cx: &App) -> WindowOptions {
        let size = Size {
            width: px(450.),
            height: px(120.),
        };

        let notification_margin_width = px(16.);
        let notification_margin_height = px(-48.);

        let bounds = gpui::Bounds::<Pixels> {
            origin: screen.bounds().top_right()
                - point(
                    size.width + notification_margin_width,
                    notification_margin_height,
                ),
            size,
        };

        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            display_id: Some(screen.id()),
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: None,
            window_min_size: None,
            window_decorations: Some(WindowDecorations::Client),
            tabbing_identifier: None,
            ..Default::default()
        }
    }
}

pub enum DeadlineNotificationEvent {
    Accepted,
    Dismissed,
}

impl EventEmitter<DeadlineNotificationEvent> for DeadlineNotification {}

impl Render for DeadlineNotification {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = theme::setup_ui_font(window, cx);
        let line_height = window.line_height();

        let bg = cx.theme().colors().elevated_surface_background;
        let gradient_overflow = || {
            div()
                .h_full()
                .absolute()
                .w_8()
                .bottom_0()
                .right_0()
                .bg(linear_gradient(
                    90.,
                    linear_color_stop(bg, 1.),
                    linear_color_stop(bg.opacity(0.2), 0.),
                ))
        };

        h_flex()
            .id("deadline-notification")
            .size_full()
            .p_3()
            .gap_4()
            .justify_between()
            .elevation_3(cx)
            .text_ui(cx)
            .font(ui_font)
            .border_color(cx.theme().colors().border)
            .rounded_xl()
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .flex_1()
                    .child(
                        h_flex().h(line_height).justify_center().child(
                            Icon::new(IconName::Bell)
                                .color(Color::Warning)
                                .size(IconSize::Medium),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .max_w(px(280.))
                            .child(
                                div()
                                    .relative()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(cx.theme().colors().text)
                                    .truncate()
                                    .child(self.title.clone())
                                    .child(gradient_overflow()),
                            )
                            .child(
                                div()
                                    .relative()
                                    .text_size(px(13.))
                                    .text_color(cx.theme().colors().text)
                                    .truncate()
                                    .child(self.task_name.clone())
                                    .child(gradient_overflow()),
                            )
                            .child(
                                h_flex()
                                    .relative()
                                    .gap_1p5()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().colors().text_muted)
                                    .child(div().child(format!(
                                        "Deadline {}: {}",
                                        self.urgency, self.deadline_date
                                    )))
                                    .child(gradient_overflow()),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Button::new("open", "View Task")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .full_width()
                            .on_click({
                                cx.listener(move |_this, _event, _window, cx| {
                                    // Emit event to signal that user wants to view the task
                                    cx.emit(DeadlineNotificationEvent::Accepted);
                                })
                            }),
                    )
                    .child(Button::new("dismiss", "Dismiss").full_width().on_click({
                        cx.listener(move |_, _event, _window, cx| {
                            // Emit event to signal dismissal
                            cx.emit(DeadlineNotificationEvent::Dismissed);
                        })
                    })),
            )
    }
}
