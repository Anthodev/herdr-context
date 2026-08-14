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
    let mut absolute_row = 0_usize;
    let mut rendered_rows = 0_u16;
    for provider in state.providers() {
        let provider_matches = state.provider_matches_filter(provider);
        let visible_count = state
            .items()
            .iter()
            .filter(|conversation| {
                conversation.tool().as_str() == provider
                    && state.conversation_matches_filter(conversation, provider_matches)
            })
            .count();
        if visible_count == 0 {
            continue;
        }
        let collapsed = state.provider_is_collapsed(provider);
        if absolute_row >= state.scroll() && rendered_rows < area.height {
            let marker = if collapsed { "▸ " } else { "▾ " };
            let mut header = Line::from(vec![
                Span::raw(marker),
                Span::raw(sanitize_terminal_text(provider)),
                Span::raw(format!(" ({})", state.provider_count(provider))),
            ]);
            if state.selected_provider() == Some(provider.as_str()) {
                header = header.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            header.render(
                Rect::new(area.x, area.y.saturating_add(rendered_rows), area.width, 1),
                buffer,
            );
            rendered_rows = rendered_rows.saturating_add(1);
        }
        absolute_row = absolute_row.saturating_add(1);
        if collapsed {
            continue;
        }
        for conversation in state.items().iter().filter(|conversation| {
            conversation.tool().as_str() == provider
                && state.conversation_matches_filter(conversation, provider_matches)
        }) {
            if rendered_rows == area.height {
                return;
            }
            if absolute_row >= state.scroll() {
                let title = conversation
                    .title()
                    .unwrap_or_else(|| conversation.session_reference().id());
                let mut line = Line::from(vec![
                    Span::raw("  "),
                    Span::raw(sanitize_terminal_text(title)),
                ]);
                if state.selection() == Some(conversation.session_reference()) {
                    line = line.style(Style::new().add_modifier(Modifier::REVERSED));
                }
                line.render(
                    Rect::new(area.x, area.y.saturating_add(rendered_rows), area.width, 1),
                    buffer,
                );
                rendered_rows = rendered_rows.saturating_add(1);
            }
            absolute_row = absolute_row.saturating_add(1);
        }
        if rendered_rows == area.height {
            return;
        }
    }
}
