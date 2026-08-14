//! Global application state and independent per-view state.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ratatui::layout::Rect;

use crate::conversations::{Conversation, SessionReference};
use crate::host::LaunchContext;
use crate::intent::{Intent, PointerAction, View};

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
    providers: Vec<String>,
    collapsed_providers: BTreeSet<String>,
    selected_provider: Option<String>,
    selection: Option<SessionReference>,
    scroll: usize,
    filter: String,
    loading: LoadingState,
    source_errors: Vec<String>,
    requested_generation: u64,
    applied_generation: u64,
}

impl Default for ConversationsViewState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            providers: Vec::new(),
            collapsed_providers: BTreeSet::new(),
            selected_provider: None,
            selection: None,
            scroll: 0,
            filter: String::new(),
            loading: LoadingState::Loading,
            source_errors: Vec::new(),
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
        let providers = items
            .iter()
            .map(|conversation| conversation.tool().as_str().to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        self.items = items;
        self.providers = providers;
        if self
            .selected_provider
            .as_ref()
            .is_none_or(|selected| !self.providers.contains(selected))
        {
            self.selected_provider = self.providers.first().cloned();
        }
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
    pub fn source_errors(&self) -> &[String] {
        &self.source_errors
    }

    pub fn set_source_errors(&mut self, source_errors: Vec<String>) {
        self.source_errors = source_errors;
    }

    #[must_use]
    pub const fn selection(&self) -> Option<&SessionReference> {
        self.selection.as_ref()
    }

    pub fn set_selection(&mut self, selection: Option<SessionReference>) {
        self.selection = selection;
    }

    #[must_use]
    pub(crate) fn providers(&self) -> &[String] {
        &self.providers
    }

    #[must_use]
    pub(crate) fn selected_provider(&self) -> Option<&str> {
        self.selected_provider.as_deref()
    }

    #[must_use]
    pub(crate) fn provider_is_collapsed(&self, provider: &str) -> bool {
        self.collapsed_providers.contains(provider)
    }

    #[must_use]
    pub(crate) fn provider_count(&self, provider: &str) -> usize {
        self.items
            .iter()
            .filter(|conversation| conversation.tool().as_str() == provider)
            .count()
    }

    #[must_use]
    pub(crate) fn provider_matches_filter(&self, provider: &str) -> bool {
        let filter = self.filter.to_lowercase();
        filter.is_empty() || provider.to_lowercase().contains(&filter)
    }

    #[must_use]
    pub(crate) fn conversation_matches_filter(
        &self,
        conversation: &Conversation,
        provider_matches: bool,
    ) -> bool {
        if provider_matches {
            return true;
        }
        let filter = self.filter.to_lowercase();
        conversation
            .title()
            .is_some_and(|title| title.to_lowercase().contains(&filter))
    }

    pub(crate) fn handle_intent(&mut self, intent: &Intent, area: Rect) -> bool {
        let warning_height = u16::from(!self.source_errors.is_empty()).min(area.height);
        let viewport_height = usize::from(area.height.saturating_sub(warning_height));
        match intent {
            Intent::SelectPrevious => self.move_provider_selection(-1, viewport_height),
            Intent::SelectNext => self.move_provider_selection(1, viewport_height),
            Intent::SelectFirst => self.select_provider_index(0, viewport_height),
            Intent::SelectLast => {
                let last = self.visible_provider_indices().len().saturating_sub(1);
                self.select_provider_index(last, viewport_height)
            }
            Intent::ExpandOrDescend => self.set_selected_provider_collapsed(false, viewport_height),
            Intent::CollapseOrAscend => self.set_selected_provider_collapsed(true, viewport_height),
            Intent::ToggleSelected => self.toggle_selected_provider(viewport_height),
            Intent::Pointer {
                column,
                row,
                action,
            } => self.handle_pointer(*column, *row, *action, area, warning_height),
            Intent::Scroll(delta) => {
                self.move_provider_selection(isize::from(*delta), viewport_height)
            }
            Intent::Quit
            | Intent::SwitchView(_)
            | Intent::NextView
            | Intent::PreviousView
            | Intent::Refresh
            | Intent::Resize => false,
        }
    }

    pub(crate) fn reconcile_viewport(&mut self, area: Rect) {
        let warning_height = u16::from(!self.source_errors.is_empty()).min(area.height);
        self.ensure_selected_provider_visible(usize::from(
            area.height.saturating_sub(warning_height),
        ));
    }

    fn provider_is_visible(&self, provider: &str) -> bool {
        let provider_matches = self.provider_matches_filter(provider);
        provider_matches
            || self.items.iter().any(|conversation| {
                conversation.tool().as_str() == provider
                    && self.conversation_matches_filter(conversation, false)
            })
    }

    fn filtered_provider_count(&self, provider: &str) -> usize {
        let provider_matches = self.provider_matches_filter(provider);
        self.items
            .iter()
            .filter(|conversation| {
                conversation.tool().as_str() == provider
                    && self.conversation_matches_filter(conversation, provider_matches)
            })
            .count()
    }

    fn visible_provider_indices(&self) -> Vec<usize> {
        self.providers
            .iter()
            .enumerate()
            .filter_map(|(index, provider)| self.provider_is_visible(provider).then_some(index))
            .collect()
    }

    fn move_provider_selection(&mut self, delta: isize, viewport_height: usize) -> bool {
        let visible = self.visible_provider_indices();
        let Some(first) = visible.first().copied() else {
            return false;
        };
        let current = self
            .selected_provider
            .as_ref()
            .and_then(|selected| {
                visible
                    .iter()
                    .position(|index| self.providers[*index] == *selected)
            })
            .unwrap_or(0);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(visible.len().saturating_sub(1))
        };
        self.select_provider(visible.get(next).copied().unwrap_or(first), viewport_height)
    }

    fn select_provider_index(&mut self, index: usize, viewport_height: usize) -> bool {
        let visible = self.visible_provider_indices();
        let Some(provider_index) = visible.get(index).copied() else {
            return false;
        };
        self.select_provider(provider_index, viewport_height)
    }

    fn select_provider(&mut self, provider_index: usize, viewport_height: usize) -> bool {
        let provider = self.providers[provider_index].clone();
        let changed = self.selected_provider.as_deref() != Some(provider.as_str())
            || self.selection.is_some();
        self.selected_provider = Some(provider);
        self.selection = None;
        self.ensure_selected_provider_visible(viewport_height);
        changed
    }

    fn set_selected_provider_collapsed(&mut self, collapsed: bool, viewport_height: usize) -> bool {
        let Some(provider) = self.selected_provider.clone() else {
            return false;
        };
        let changed = if collapsed {
            self.collapsed_providers.insert(provider.clone())
        } else {
            self.collapsed_providers.remove(&provider)
        };
        if collapsed
            && self.selection.as_ref().is_some_and(|selection| {
                self.items.iter().any(|conversation| {
                    conversation.tool().as_str() == provider
                        && conversation.session_reference() == selection
                })
            })
        {
            self.selection = None;
        }
        self.ensure_selected_provider_visible(viewport_height);
        changed
    }

    fn toggle_selected_provider(&mut self, viewport_height: usize) -> bool {
        let Some(provider) = self.selected_provider.as_deref() else {
            return false;
        };
        let collapse = !self.collapsed_providers.contains(provider);
        self.set_selected_provider_collapsed(collapse, viewport_height)
    }

    fn handle_pointer(
        &mut self,
        column: u16,
        row: u16,
        action: PointerAction,
        area: Rect,
        warning_height: u16,
    ) -> bool {
        let content_y = area.y.saturating_add(warning_height);
        let content_height = area.height.saturating_sub(warning_height);
        if row < content_y || row >= content_y.saturating_add(content_height) {
            return false;
        }
        let absolute_row = self
            .scroll
            .saturating_add(usize::from(row.saturating_sub(content_y)));
        let Some(provider) = self.provider_at_row(absolute_row).map(str::to_owned) else {
            return false;
        };
        let provider_index = self
            .providers
            .iter()
            .position(|candidate| candidate == &provider)
            .expect("visible provider belongs to provider inventory");
        let mut changed = self.select_provider(provider_index, usize::from(content_height));
        if action == PointerAction::Toggle || (action == PointerAction::Select && column == area.x)
        {
            changed |= self.toggle_selected_provider(usize::from(content_height));
        }
        changed
    }

    fn provider_at_row(&self, target: usize) -> Option<&str> {
        let mut row = 0_usize;
        for provider in &self.providers {
            if !self.provider_is_visible(provider) {
                continue;
            }
            if row == target {
                return Some(provider);
            }
            row = row.saturating_add(1);
            if !self.provider_is_collapsed(provider) {
                row = row.saturating_add(self.filtered_provider_count(provider));
            }
        }
        None
    }

    fn selected_provider_row(&self) -> Option<usize> {
        let selected = self.selected_provider.as_deref()?;
        let mut row = 0_usize;
        for provider in &self.providers {
            if !self.provider_is_visible(provider) {
                continue;
            }
            if provider == selected {
                return Some(row);
            }
            row = row.saturating_add(1);
            if !self.provider_is_collapsed(provider) {
                row = row.saturating_add(self.filtered_provider_count(provider));
            }
        }
        None
    }

    fn visible_row_count(&self) -> usize {
        self.providers
            .iter()
            .filter(|provider| self.provider_is_visible(provider))
            .map(|provider| {
                1 + if self.provider_is_collapsed(provider) {
                    0
                } else {
                    self.filtered_provider_count(provider)
                }
            })
            .sum()
    }

    fn ensure_selected_provider_visible(&mut self, viewport_height: usize) {
        let Some(row) = self.selected_provider_row() else {
            self.scroll = 0;
            return;
        };
        if row < self.scroll {
            self.scroll = row;
        } else if viewport_height != 0 && row >= self.scroll.saturating_add(viewport_height) {
            self.scroll = row.saturating_add(1).saturating_sub(viewport_height);
        }
        self.scroll = self
            .scroll
            .min(self.visible_row_count().saturating_sub(viewport_height));
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
