//! Ratatui shell and view rendering.

use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::config::DisplayMode;
use crate::intent::View;
use crate::model::{AppModel, ConversationsViewState, LoadingState, UiGeometry};

mod conversation_display;
pub mod conversations;
mod file_display;
pub mod files;
mod theme;

pub fn render_shell(model: &mut AppModel, area: Rect, buffer: &mut Buffer) {
    let header_height = area.height.min(1);
    let files_width = area.width.min(7);
    let files_tab = Rect::new(area.x, area.y, files_width, header_height);
    let conversations_tab = Rect::new(
        area.x.saturating_add(files_width),
        area.y,
        area.width.saturating_sub(files_width),
        header_height,
    );
    let warning_height = u16::from(!model.config_warnings().is_empty())
        .min(area.height.saturating_sub(header_height));
    let warning = Rect::new(
        area.x,
        area.y.saturating_add(header_height),
        area.width,
        warning_height,
    );
    let content = Rect::new(
        area.x,
        warning.y.saturating_add(warning_height),
        area.width,
        area.height
            .saturating_sub(header_height)
            .saturating_sub(warning_height),
    );
    model.set_geometry(UiGeometry::new(files_tab, conversations_tab, content));
    if header_height != 0 {
        let active = model.active_view();
        Line::from(vec![
            Span::styled(" Files ", tab_style(active == View::Files)),
            Span::styled(" Conversations ", tab_style(active == View::Conversations)),
        ])
        .render(Rect::new(area.x, area.y, area.width, 1), buffer);
    }
    if warning_height != 0 {
        let warnings = model.config_warnings();
        let remaining = warnings.len().saturating_sub(1);
        let count = if remaining > 0 {
            format!("(+{remaining} more) ")
        } else {
            String::new()
        };
        let warning_text = warnings[0].strip_prefix("Config: ").unwrap_or(&warnings[0]);
        Paragraph::new(Line::from(vec![
            Span::styled("Config: ", theme::warning().bold()),
            Span::styled(count, theme::warning()),
            Span::styled(sanitize_terminal_text(warning_text), theme::warning()),
        ]))
        .render(warning, buffer);
    }
    if content.is_empty() {
        return;
    }

    let display_mode = model.display_mode();
    match model.active_view() {
        View::Files => match model.files().loading() {
            LoadingState::Loading => render_message("Loading Files…", content, buffer),
            LoadingState::Ready => {}
            LoadingState::Error(error) => render_error(error, content, buffer),
        },
        View::Conversations => {
            let loading = model.conversations().loading().clone();
            match loading {
                LoadingState::Loading => {
                    render_message("Loading conversations…", content, buffer);
                }
                LoadingState::Ready => {
                    render_conversations_state(
                        model.conversations_mut(),
                        display_mode,
                        content,
                        buffer,
                    );
                }
                LoadingState::Error(error) => render_error(&error, content, buffer),
            }
        }
    }
}

const fn tab_style(active: bool) -> Style {
    if active {
        theme::selected()
    } else {
        theme::inactive()
    }
}

fn render_message(message: &str, area: Rect, buffer: &mut Buffer) {
    Paragraph::new(message).render(area, buffer);
}

fn render_error(error: &str, area: Rect, buffer: &mut Buffer) {
    Paragraph::new(Line::from(vec![
        Span::styled("Error: ", theme::error().bold()),
        Span::styled(sanitize_terminal_text(error), theme::error()),
    ]))
    .render(area, buffer);
}

fn render_conversations_state(
    state: &mut ConversationsViewState,
    display_mode: DisplayMode,
    area: Rect,
    buffer: &mut Buffer,
) {
    state.reconcile_viewport(area);
    let errors = state.visible_errors();
    let content = if let Some(error) = errors.first() {
        let remaining = errors.len().saturating_sub(1);
        let count = if remaining > 0 {
            format!("(+{remaining} more) ")
        } else {
            String::new()
        };
        Paragraph::new(Line::from(vec![
            Span::styled("Warning: ", theme::warning().bold()),
            Span::styled(count, theme::warning()),
            Span::styled(sanitize_terminal_text(error), theme::warning()),
        ]))
        .render(Rect::new(area.x, area.y, area.width, 1), buffer);
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        )
    } else if state.live_loading() {
        render_message(
            "Loading live sessions…",
            Rect::new(area.x, area.y, area.width, 1),
            buffer,
        );
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        )
    } else {
        area
    };
    if content.is_empty() {
        return;
    }
    if state.items().is_empty() {
        render_message("No conversations", content, buffer);
    } else {
        conversations::render(state, display_mode, content, buffer);
    }
}

#[must_use]
pub fn sanitize_terminal_text(value: &str) -> Cow<'_, str> {
    sanitize_terminal_cow(Cow::Borrowed(value))
}

pub(crate) fn sanitize_terminal_cow(value: Cow<'_, str>) -> Cow<'_, str> {
    if !value.chars().any(char::is_control) {
        return value;
    }
    Cow::Owned(
        value
            .chars()
            .map(|character| {
                if character.is_control() {
                    '�'
                } else {
                    character
                }
            })
            .collect(),
    )
}
