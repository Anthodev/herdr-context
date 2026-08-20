//! Semantic UI styles and deterministic VCS status colors.
//!
//! General UI roles use ANSI slots resolved through the user's active terminal
//! palette. Primary VCS states use truecolor overrides so added, modified, and
//! deleted nodes cannot be remapped to unrelated terminal colors.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Magenta;
pub const MUTED: Color = Color::DarkGray;
pub const WARNING: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
pub const VCS_ADDED: Color = Color::Rgb(63, 185, 80);
pub const VCS_MODIFIED: Color = Color::Rgb(210, 153, 34);
pub const VCS_DELETED: Color = Color::Rgb(248, 81, 73);
pub const MOVED: Color = Color::Cyan;
pub const UNTRACKED: Color = Color::Blue;

#[must_use]
pub const fn selected() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::REVERSED)
}

#[must_use]
pub const fn selected_neutral() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

#[must_use]
pub const fn inactive() -> Style {
    Style::new().fg(MUTED)
}

#[must_use]
pub const fn warning() -> Style {
    Style::new().fg(WARNING)
}

#[must_use]
pub const fn error() -> Style {
    Style::new().fg(ERROR)
}
