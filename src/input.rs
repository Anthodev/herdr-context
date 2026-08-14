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
            keybindings.map_or_else(|| map_key(key, mode), |bindings| bindings.map_key(key))
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
        InputMode::Normal => map_normal_key(key),
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
        _ => None,
    }
}
