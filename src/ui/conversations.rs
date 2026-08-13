use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::model::ConversationsViewState;

use super::sanitize_terminal_text;

pub(crate) fn render(state: &ConversationsViewState, area: Rect, buffer: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let filter = state.filter().to_lowercase();
    for (offset, conversation) in state
        .items()
        .iter()
        .filter(|conversation| {
            filter.is_empty()
                || conversation
                    .title()
                    .is_some_and(|title| title.to_lowercase().contains(&filter))
                || conversation
                    .tool()
                    .as_str()
                    .to_lowercase()
                    .contains(&filter)
        })
        .skip(state.scroll())
        .take(usize::from(area.height))
        .enumerate()
    {
        let title = conversation
            .title()
            .unwrap_or_else(|| conversation.session_reference().id());
        let mut line = Line::from(vec![
            Span::raw(sanitize_terminal_text(conversation.tool().as_str())),
            Span::raw(": "),
            Span::raw(sanitize_terminal_text(title)),
        ]);
        if state.selection() == Some(conversation.session_reference()) {
            line = line.style(Style::new().add_modifier(Modifier::REVERSED));
        }
        line.render(
            Rect::new(area.x, area.y.saturating_add(offset as u16), area.width, 1),
            buffer,
        );
    }
}
