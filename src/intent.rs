//! Closed user-intent set consumed by the controller.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum View {
    #[default]
    Files,
    Conversations,
}

impl View {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Files => Self::Conversations,
            Self::Conversations => Self::Files,
        }
    }

    #[must_use]
    pub const fn previous(self) -> Self {
        self.next()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerAction {
    Select,
    Toggle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Intent {
    Quit,
    SwitchView(View),
    NextView,
    PreviousView,
    SelectPrevious,
    SelectNext,
    SelectFirst,
    SelectLast,
    ExpandOrDescend,
    CollapseOrAscend,
    ToggleSelected,
    Refresh,
    Pointer {
        column: u16,
        row: u16,
        action: PointerAction,
    },
    Scroll(i8),
    Resize,
}
