//! Global application state and independent per-view state.

use std::path::{Path, PathBuf};

use ratatui::layout::Rect;

use crate::conversations::{Conversation, SessionReference};
use crate::host::LaunchContext;
use crate::intent::View;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadingState {
    Loading,
    Ready,
    Error(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UiGeometry {
    files_tab: Rect,
    conversations_tab: Rect,
    content: Rect,
}

impl UiGeometry {
    #[must_use]
    pub const fn new(files_tab: Rect, conversations_tab: Rect, content: Rect) -> Self {
        Self {
            files_tab,
            conversations_tab,
            content,
        }
    }

    #[must_use]
    pub const fn files_tab(&self) -> Rect {
        self.files_tab
    }

    #[must_use]
    pub const fn conversations_tab(&self) -> Rect {
        self.conversations_tab
    }

    #[must_use]
    pub const fn content(&self) -> Rect {
        self.content
    }

    #[must_use]
    pub const fn tab_at(&self, column: u16, row: u16) -> Option<View> {
        if contains(self.files_tab, column, row) {
            Some(View::Files)
        } else if contains(self.conversations_tab, column, row) {
            Some(View::Conversations)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn content_contains(&self, column: u16, row: u16) -> bool {
        contains(self.content, column, row)
    }
}

const fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

#[derive(Debug)]
pub struct FilesViewState {
    selection: Option<PathBuf>,
    scroll: usize,
    filter: String,
    loading: LoadingState,
    requested_generation: u64,
    applied_generation: u64,
}

impl Default for FilesViewState {
    fn default() -> Self {
        Self {
            selection: None,
            scroll: 0,
            filter: String::new(),
            loading: LoadingState::Loading,
            requested_generation: 0,
            applied_generation: 0,
        }
    }
}

impl FilesViewState {
    #[must_use]
    pub fn selection(&self) -> Option<&Path> {
        self.selection.as_deref()
    }

    pub fn set_selection(&mut self, selection: Option<PathBuf>) {
        self.selection = selection;
    }

    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    pub const fn set_scroll(&mut self, scroll: usize) {
        self.scroll = scroll;
    }

    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = filter.into();
    }

    #[must_use]
    pub const fn loading(&self) -> &LoadingState {
        &self.loading
    }

    pub fn set_loading(&mut self, loading: LoadingState) {
        self.loading = loading;
    }

    #[must_use]
    pub const fn generations(&self) -> (u64, u64) {
        (self.requested_generation, self.applied_generation)
    }

    pub const fn set_generations(&mut self, requested: u64, applied: u64) {
        self.requested_generation = requested;
        self.applied_generation = applied;
    }
}

#[derive(Debug)]
pub struct ConversationsViewState {
    items: Vec<Conversation>,
    selection: Option<SessionReference>,
    scroll: usize,
    filter: String,
    loading: LoadingState,
    requested_generation: u64,
    applied_generation: u64,
}

impl Default for ConversationsViewState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selection: None,
            scroll: 0,
            filter: String::new(),
            loading: LoadingState::Loading,
            requested_generation: 0,
            applied_generation: 0,
        }
    }
}

impl ConversationsViewState {
    #[must_use]
    pub fn items(&self) -> &[Conversation] {
        &self.items
    }

    pub fn replace_items(&mut self, items: Vec<Conversation>, generation: u64) -> bool {
        if generation < self.requested_generation {
            return false;
        }
        self.items = items;
        self.applied_generation = generation;
        self.loading = LoadingState::Ready;
        if self.selection.as_ref().is_some_and(|selection| {
            !self
                .items
                .iter()
                .any(|item| item.session_reference() == selection)
        }) {
            self.selection = None;
        }
        true
    }

    #[must_use]
    pub const fn selection(&self) -> Option<&SessionReference> {
        self.selection.as_ref()
    }

    pub fn set_selection(&mut self, selection: Option<SessionReference>) {
        self.selection = selection;
    }

    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    pub const fn set_scroll(&mut self, scroll: usize) {
        self.scroll = scroll;
    }

    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = filter.into();
    }

    #[must_use]
    pub const fn loading(&self) -> &LoadingState {
        &self.loading
    }

    pub fn set_loading(&mut self, loading: LoadingState) {
        self.loading = loading;
    }

    #[must_use]
    pub const fn generations(&self) -> (u64, u64) {
        (self.requested_generation, self.applied_generation)
    }

    pub const fn set_generations(&mut self, requested: u64, applied: u64) {
        self.requested_generation = requested;
        self.applied_generation = applied;
    }
}

#[derive(Debug)]
pub struct AppModel {
    launch_context: LaunchContext,
    active_view: View,
    geometry: UiGeometry,
    files: FilesViewState,
    conversations: ConversationsViewState,
}

impl AppModel {
    #[must_use]
    pub fn new(launch_context: LaunchContext) -> Self {
        Self {
            launch_context,
            active_view: View::Files,
            geometry: UiGeometry::default(),
            files: FilesViewState::default(),
            conversations: ConversationsViewState::default(),
        }
    }

    #[must_use]
    pub const fn launch_context(&self) -> &LaunchContext {
        &self.launch_context
    }

    #[must_use]
    pub const fn active_view(&self) -> View {
        self.active_view
    }

    pub const fn set_active_view(&mut self, view: View) {
        self.active_view = view;
    }

    #[must_use]
    pub const fn geometry(&self) -> &UiGeometry {
        &self.geometry
    }

    pub(crate) const fn set_geometry(&mut self, geometry: UiGeometry) {
        self.geometry = geometry;
    }

    #[must_use]
    pub const fn files(&self) -> &FilesViewState {
        &self.files
    }

    pub const fn files_mut(&mut self) -> &mut FilesViewState {
        &mut self.files
    }

    #[must_use]
    pub const fn conversations(&self) -> &ConversationsViewState {
        &self.conversations
    }

    pub const fn conversations_mut(&mut self) -> &mut ConversationsViewState {
        &mut self.conversations
    }
}
