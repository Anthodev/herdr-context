//! Pure terminal-event to intent mapping. No I/O is performed here.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

use crate::config::KeyBindings;
use crate::intent::{Intent, PointerAction, View};
use crate::model::UiGeometry;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InputMode {
    #[default]
    Normal,
    FileSearch,
    FileSearchActive,
}

#[must_use]
pub fn map_event(event: Event, mode: InputMode, geometry: &UiGeometry) -> Option<Intent> {
    map_event_with_keybindings(event, mode, geometry, None)
}

#[must_use]
pub fn map_event_with_keybindings(
    event: Event,
    mode: InputMode,
    geometry: &UiGeometry,
    keybindings: Option<&KeyBindings>,
) -> Option<Intent> {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            match mode {
                InputMode::Normal => {
                    keybindings.map_or_else(|| map_key(key, mode), |bindings| bindings.map_key(key))
                }
                InputMode::FileSearchActive if key.code == KeyCode::Esc => {
                    Some(Intent::FileSearchCancel)
                }
                InputMode::FileSearchActive => {
                    keybindings.map_or_else(|| map_key(key, mode), |bindings| bindings.map_key(key))
                }
                InputMode::FileSearch => map_key(key, mode),
            }
        }
        Event::Paste(value) if mode == InputMode::FileSearch => {
            Some(Intent::FileSearchInput(value))
        }
        Event::Mouse(mouse) => {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                && let Some(view) = geometry.tab_at(mouse.column, mouse.row)
            {
                return Some(Intent::SwitchView(view));
            }
            if !geometry.content_contains(mouse.column, mouse.row) {
                return None;
            }
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => Some(Intent::Pointer {
                    column: mouse.column,
                    row: mouse.row,
                    action: PointerAction::Select,
                }),
                MouseEventKind::Down(MouseButton::Right) => Some(Intent::Pointer {
                    column: mouse.column,
                    row: mouse.row,
                    action: PointerAction::Toggle,
                }),
                MouseEventKind::ScrollUp => Some(Intent::Scroll(-1)),
                MouseEventKind::ScrollDown => Some(Intent::Scroll(1)),
                _ => None,
            }
        }
        Event::Resize(_, _) => Some(Intent::Resize),
        _ => None,
    }
}

fn map_key(key: KeyEvent, mode: InputMode) -> Option<Intent> {
    match mode {
        InputMode::Normal | InputMode::FileSearchActive => map_normal_key(key),
        InputMode::FileSearch => map_file_search_key(key),
    }
}

fn map_normal_key(key: KeyEvent) -> Option<Intent> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Intent::Quit);
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => Some(Intent::Quit),
        KeyCode::Tab => Some(Intent::NextView),
        KeyCode::BackTab => Some(Intent::PreviousView),
        KeyCode::Char('1') => Some(Intent::SwitchView(View::Files)),
        KeyCode::Char('2') => Some(Intent::SwitchView(View::Conversations)),
        KeyCode::Up | KeyCode::Char('k') => Some(Intent::SelectPrevious),
        KeyCode::Down | KeyCode::Char('j') => Some(Intent::SelectNext),
        KeyCode::Home => Some(Intent::SelectFirst),
        KeyCode::End => Some(Intent::SelectLast),
        KeyCode::Right | KeyCode::Char('l') => Some(Intent::ExpandOrDescend),
        KeyCode::Left | KeyCode::Char('h') => Some(Intent::CollapseOrAscend),
        KeyCode::Enter | KeyCode::Char(' ') => Some(Intent::ToggleSelected),
        KeyCode::Char('r') => Some(Intent::Refresh),
        KeyCode::Char('w') => Some(Intent::SwitchFilesPane),
        KeyCode::Char('/') => Some(Intent::BeginFileSearch),
        _ => None,
    }
}

fn map_file_search_key(key: KeyEvent) -> Option<Intent> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(Intent::Quit),
            KeyCode::Char('u') => Some(Intent::FileSearchClear),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Esc => Some(Intent::FileSearchCancel),
        KeyCode::Enter => Some(Intent::FileSearchCommit),
        KeyCode::Backspace => Some(Intent::FileSearchBackspace),
        KeyCode::Tab => Some(Intent::NextView),
        KeyCode::BackTab => Some(Intent::PreviousView),
        KeyCode::Char(character) => Some(Intent::FileSearchInput(character.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::{InputMode, map_event};
    use crate::intent::Intent;
    use crate::model::UiGeometry;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn file_search_mode_treats_printable_shortcuts_as_query_text() {
        let geometry = UiGeometry::default();

        assert_eq!(
            map_event(
                key(KeyCode::Char('q'), KeyModifiers::NONE),
                InputMode::FileSearch,
                &geometry,
            ),
            Some(Intent::FileSearchInput("q".to_owned()))
        );
        assert_eq!(
            map_event(
                key(KeyCode::Char('u'), KeyModifiers::CONTROL),
                InputMode::FileSearch,
                &geometry,
            ),
            Some(Intent::FileSearchClear)
        );
        assert_eq!(
            map_event(
                Event::Paste("src/lib.rs".to_owned()),
                InputMode::FileSearch,
                &geometry
            ),
            Some(Intent::FileSearchInput("src/lib.rs".to_owned()))
        );
    }

    #[test]
    fn committed_file_search_uses_escape_to_clear_before_quitting() {
        let geometry = UiGeometry::default();

        assert_eq!(
            map_event(
                key(KeyCode::Esc, KeyModifiers::NONE),
                InputMode::FileSearchActive,
                &geometry,
            ),
            Some(Intent::FileSearchCancel)
        );
        assert_eq!(
            map_event(
                key(KeyCode::Char('q'), KeyModifiers::NONE),
                InputMode::FileSearchActive,
                &geometry,
            ),
            Some(Intent::Quit)
        );
    }

    #[test]
    fn w_maps_to_the_files_pane_focus_toggle() {
        let geometry = UiGeometry::default();

        assert_eq!(
            map_event(
                key(KeyCode::Char('w'), KeyModifiers::NONE),
                InputMode::Normal,
                &geometry,
            ),
            Some(Intent::SwitchFilesPane)
        );
        assert_eq!(
            crate::config::KeyBindings::default()
                .map_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            Some(Intent::SwitchFilesPane)
        );
    }
}
