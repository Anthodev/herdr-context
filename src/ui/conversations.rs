use std::time::UNIX_EPOCH;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use time::OffsetDateTime;

use crate::config::DisplayMode;
use crate::conversations::{Conversation, ConversationState, ProvenanceKind, ResumeCapability};
use crate::model::ConversationsViewState;

use super::{conversation_display, sanitize_terminal_text, theme};

pub(crate) fn render(
    state: &ConversationsViewState,
    display_mode: DisplayMode,
    area: Rect,
    buffer: &mut Buffer,
) {
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
            let header = Line::from(vec![
                Span::raw(conversation_display::provider(display_mode, collapsed)),
                Span::raw(" "),
                Span::raw(sanitize_terminal_text(provider)),
                Span::raw(format!(" ({})", state.provider_count(provider))),
            ]);
            let row = Rect::new(area.x, area.y.saturating_add(rendered_rows), area.width, 1);
            header.render(row, buffer);
            if state.selected_provider() == Some(provider.as_str()) {
                buffer.set_style(row, theme::selection_band());
            }
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
            if absolute_row >= state.scroll() {
                let line = conversation_line(conversation, display_mode, area.width >= 96);
                let row = Rect::new(area.x, area.y.saturating_add(rendered_rows), area.width, 1);
                line.render(row, buffer);
                if state.selection() == Some(conversation.session_reference()) {
                    buffer.set_style(row, theme::selection_band());
                }
                rendered_rows = rendered_rows.saturating_add(1);
            }
            absolute_row = absolute_row.saturating_add(1);
        }
        if rendered_rows == area.height {
            return;
        }
    }
}

fn conversation_line(
    conversation: &Conversation,
    display_mode: DisplayMode,
    wide: bool,
) -> Line<'static> {
    let title = sanitize_terminal_text(
        conversation
            .title()
            .unwrap_or_else(|| conversation.session_reference().id()),
    )
    .into_owned();
    let updated = display_timestamp(conversation.updated_at());
    let tool = sanitize_terminal_text(conversation.tool().as_str()).into_owned();
    let provenance = provenance_label(conversation, wide);
    let resumable = matches!(
        conversation.resume_capability(),
        ResumeCapability::Supported(_)
    );
    let conversation_state = conversation.state();
    let state = match conversation_state {
        ConversationState::Live => "live",
        ConversationState::Archived => "archived",
        ConversationState::Unknown => "unknown",
    };
    let metadata = if wide {
        format!(
            "  tool={} updated={updated} source={provenance} resume={} state={state}",
            tool,
            if resumable { "yes" } else { "no" },
        )
    } else {
        format!(
            " · {} · {updated} · {provenance} · {} · {state}",
            tool,
            if resumable { "R" } else { "-" },
        )
    };
    Line::from(vec![
        Span::raw("  "),
        Span::raw(conversation_display::conversation(
            display_mode,
            conversation_state,
        )),
        Span::raw(" "),
        Span::raw(title),
        Span::styled(metadata, theme::inactive()),
    ])
}

fn display_timestamp(value: std::time::SystemTime) -> String {
    let Some(seconds) = value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
    else {
        return "before-epoch".to_owned();
    };
    let Ok(value) = OffsetDateTime::from_unix_timestamp(seconds) else {
        return "out-of-range".to_owned();
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}Z",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
    )
}

fn provenance_label(conversation: &Conversation, wide: bool) -> String {
    let mut project = false;
    let mut external = false;
    let mut live = false;
    for provenance in conversation.provenance() {
        match provenance.kind() {
            ProvenanceKind::ProjectLocal => project = true,
            ProvenanceKind::ExternalLocal => external = true,
            ProvenanceKind::HostRuntime => live = true,
        }
    }
    let labels = if wide {
        [
            project.then_some("project"),
            external.then_some("external"),
            live.then_some("live"),
        ]
    } else {
        [
            project.then_some("P"),
            external.then_some("E"),
            live.then_some("L"),
        ]
    };
    labels.into_iter().flatten().collect::<Vec<_>>().join("+")
}
