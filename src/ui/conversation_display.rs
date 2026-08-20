use crate::config::DisplayMode;
use crate::conversations::ConversationState;

pub const fn provider(mode: DisplayMode, collapsed: bool) -> &'static str {
    match (mode, collapsed) {
        (DisplayMode::Ascii, true) => "+",
        (DisplayMode::Ascii, false) => "-",
        (DisplayMode::Unicode, true) => "▸",
        (DisplayMode::Unicode, false) => "▾",
        (DisplayMode::Nerd, true) => "",
        (DisplayMode::Nerd, false) => "",
    }
}

pub const fn conversation(mode: DisplayMode, state: ConversationState) -> &'static str {
    match (mode, state) {
        (DisplayMode::Ascii, ConversationState::Live) => "*",
        (DisplayMode::Ascii, ConversationState::Archived) => "-",
        (DisplayMode::Ascii, ConversationState::Unknown) => "?",
        (DisplayMode::Unicode, ConversationState::Live) => "●",
        (DisplayMode::Unicode, ConversationState::Archived) => "○",
        (DisplayMode::Unicode, ConversationState::Unknown) => "•",
        (DisplayMode::Nerd, ConversationState::Live) => "",
        (DisplayMode::Nerd, ConversationState::Archived) => "",
        (DisplayMode::Nerd, ConversationState::Unknown) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::{conversation, provider};
    use crate::config::DisplayMode;
    use crate::conversations::ConversationState;

    #[test]
    fn exposes_stable_glyphs_for_every_mode_and_state() {
        assert_eq!(provider(DisplayMode::Ascii, true), "+");
        assert_eq!(provider(DisplayMode::Unicode, false), "▾");
        assert_eq!(provider(DisplayMode::Nerd, false), "");
        assert_eq!(
            conversation(DisplayMode::Ascii, ConversationState::Live),
            "*"
        );
        assert_eq!(
            conversation(DisplayMode::Unicode, ConversationState::Archived),
            "○"
        );
        assert_eq!(
            conversation(DisplayMode::Nerd, ConversationState::Unknown),
            ""
        );
    }
}
