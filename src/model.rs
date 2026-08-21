//! Global application state and independent per-view state.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ratatui::layout::Rect;

use crate::config::DisplayMode;
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
    search_editing: bool,
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
            search_editing: false,
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
    pub const fn search_editing(&self) -> bool {
        self.search_editing
    }

    pub const fn set_search_editing(&mut self, editing: bool) {
        self.search_editing = editing;
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

enum ConversationRowTarget {
    Provider(String),
    Session {
        provider: String,
        reference: SessionReference,
    },
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
    live_error: Option<String>,
    launch_error: Option<String>,
    visible_errors: Vec<String>,
    live_loading: bool,
    requested_generation: u64,
    applied_generation: u64,
    live_requested_generation: u64,
    live_applied_generation: u64,
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
            live_error: None,
            launch_error: None,
            visible_errors: Vec::new(),
            live_loading: false,
            requested_generation: 0,
            applied_generation: 0,
            live_requested_generation: 0,
            live_applied_generation: 0,
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
        self.applied_generation = generation;
        self.loading = LoadingState::Ready;
        self.replace_visible_items(items)
    }

    pub fn replace_live_items(&mut self, items: Vec<Conversation>, generation: u64) -> bool {
        if generation < self.live_requested_generation {
            return false;
        }
        self.live_applied_generation = generation;
        self.live_loading = false;
        self.replace_visible_items(items)
    }

    fn replace_visible_items(&mut self, items: Vec<Conversation>) -> bool {
        let migrated_selection = self.selection.as_ref().and_then(|selection| {
            if items
                .iter()
                .any(|item| item.session_reference() == selection)
            {
                return None;
            }
            let selected = self
                .items
                .iter()
                .find(|item| item.session_reference() == selection)?;
            let mut candidates = items.iter().filter(|candidate| {
                selected
                    .provenance()
                    .iter()
                    .filter(|provenance| provenance.path().is_some())
                    .any(|provenance| candidate.provenance().contains(provenance))
            });
            let candidate = candidates.next()?;
            candidates
                .next()
                .is_none()
                .then(|| candidate.session_reference().clone())
        });
        let providers = items
            .iter()
            .map(|conversation| conversation.tool().as_str().to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        self.items = items;
        self.providers = providers;
        if self.selection.as_ref().is_some_and(|selection| {
            !self
                .items
                .iter()
                .any(|item| item.session_reference() == selection)
        }) {
            self.selection = migrated_selection;
        }
        if let Some(selected) = self.selection.as_ref().and_then(|selection| {
            self.items
                .iter()
                .find(|item| item.session_reference() == selection)
        }) {
            self.selected_provider = Some(selected.tool().as_str().to_owned());
        } else if self
            .selected_provider
            .as_ref()
            .is_none_or(|selected| !self.providers.contains(selected))
        {
            self.selected_provider = self.providers.first().cloned();
        }
        true
    }

    #[must_use]
    pub fn source_errors(&self) -> &[String] {
        &self.source_errors
    }

    #[must_use]
    pub fn visible_errors(&self) -> &[String] {
        &self.visible_errors
    }

    pub fn set_source_errors(&mut self, source_errors: Vec<String>) {
        self.source_errors = source_errors;
        self.rebuild_visible_errors();
    }

    pub fn set_live_error(&mut self, live_error: Option<String>) {
        self.live_error = live_error;
        self.rebuild_visible_errors();
    }

    pub fn set_launch_error(&mut self, launch_error: Option<String>) {
        self.launch_error = launch_error;
        self.rebuild_visible_errors();
    }

    fn rebuild_visible_errors(&mut self) {
        self.visible_errors.clone_from(&self.source_errors);
        for error in [&self.live_error, &self.launch_error].into_iter().flatten() {
            if !self.visible_errors.contains(error) {
                self.visible_errors.push(error.clone());
            }
        }
    }

    #[must_use]
    pub const fn selection(&self) -> Option<&SessionReference> {
        self.selection.as_ref()
    }

    #[must_use]
    pub(crate) fn selected_conversation(&self) -> Option<&Conversation> {
        let selection = self.selection.as_ref()?;
        self.items
            .iter()
            .find(|conversation| conversation.session_reference() == selection)
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
        self.selection
            .is_none()
            .then_some(self.selected_provider.as_deref())
            .flatten()
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
        let warning_height =
            u16::from(!self.visible_errors.is_empty() || self.live_loading).min(area.height);
        let viewport_height = usize::from(area.height.saturating_sub(warning_height));
        match intent {
            Intent::SelectPrevious => self.move_row_selection(-1, viewport_height),
            Intent::SelectNext => self.move_row_selection(1, viewport_height),
            Intent::SelectFirst => self.select_row(0, viewport_height),
            Intent::SelectLast => {
                self.select_row(self.visible_row_count().saturating_sub(1), viewport_height)
            }
            Intent::ExpandOrDescend => self.set_selected_provider_collapsed(false, viewport_height),
            Intent::CollapseOrAscend => self.set_selected_provider_collapsed(true, viewport_height),
            Intent::ToggleSelected => self.toggle_selected_provider(viewport_height),
            Intent::Pointer {
                column,
                row,
                action,
            } => self.handle_pointer(*column, *row, *action, area, warning_height),
            Intent::Scroll(delta) => self.move_row_selection(isize::from(*delta), viewport_height),
            Intent::Quit
            | Intent::SwitchView(_)
            | Intent::NextView
            | Intent::PreviousView
            | Intent::Refresh
            | Intent::SwitchFilesPane
            | Intent::BeginFileSearch
            | Intent::FileSearchInput(_)
            | Intent::FileSearchBackspace
            | Intent::FileSearchClear
            | Intent::FileSearchCommit
            | Intent::FileSearchCancel
            | Intent::Resize => false,
        }
    }

    pub(crate) fn reconcile_viewport(&mut self, area: Rect) {
        let warning_height =
            u16::from(!self.visible_errors.is_empty() || self.live_loading).min(area.height);
        self.ensure_selected_row_visible(usize::from(area.height.saturating_sub(warning_height)));
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

    fn move_row_selection(&mut self, delta: isize, viewport_height: usize) -> bool {
        let count = self.visible_row_count();
        if count == 0 {
            return false;
        }
        let next = self.selected_row().map_or_else(
            || {
                if delta.is_negative() {
                    count.saturating_sub(1)
                } else {
                    0
                }
            },
            |current| {
                if delta.is_negative() {
                    current.saturating_sub(delta.unsigned_abs())
                } else {
                    current
                        .saturating_add(delta as usize)
                        .min(count.saturating_sub(1))
                }
            },
        );
        self.select_row(next, viewport_height)
    }

    fn select_row(&mut self, row: usize, viewport_height: usize) -> bool {
        let Some(target) = self.target_at_row(row) else {
            return false;
        };
        let (provider, selection) = match target {
            ConversationRowTarget::Provider(provider) => (provider, None),
            ConversationRowTarget::Session {
                provider,
                reference,
            } => (provider, Some(reference)),
        };
        let changed = self.selected_provider.as_deref() != Some(provider.as_str())
            || self.selection != selection;
        self.selected_provider = Some(provider);
        self.selection = selection;
        self.ensure_selected_row_visible(viewport_height);
        changed
    }

    fn target_at_row(&self, target: usize) -> Option<ConversationRowTarget> {
        let mut row = 0_usize;
        for provider in &self.providers {
            if !self.provider_is_visible(provider) {
                continue;
            }
            if row == target {
                return Some(ConversationRowTarget::Provider(provider.clone()));
            }
            row = row.saturating_add(1);
            if self.provider_is_collapsed(provider) {
                continue;
            }
            let provider_matches = self.provider_matches_filter(provider);
            for conversation in self.items.iter().filter(|conversation| {
                conversation.tool().as_str() == provider
                    && self.conversation_matches_filter(conversation, provider_matches)
            }) {
                if row == target {
                    return Some(ConversationRowTarget::Session {
                        provider: provider.clone(),
                        reference: conversation.session_reference().clone(),
                    });
                }
                row = row.saturating_add(1);
            }
        }
        None
    }

    fn set_selected_provider_collapsed(&mut self, collapsed: bool, viewport_height: usize) -> bool {
        let Some(provider) = self.selected_provider.clone() else {
            return false;
        };
        if self.selection.is_some() {
            if !collapsed {
                return false;
            }
            self.selection = None;
            self.ensure_selected_row_visible(viewport_height);
            return true;
        }
        let changed = if collapsed {
            self.collapsed_providers.insert(provider)
        } else {
            self.collapsed_providers.remove(&provider)
        };
        self.ensure_selected_row_visible(viewport_height);
        changed
    }

    fn toggle_selected_provider(&mut self, viewport_height: usize) -> bool {
        if self.selection.is_some() {
            return false;
        }
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
        let is_provider = matches!(
            self.target_at_row(absolute_row),
            Some(ConversationRowTarget::Provider(_))
        );
        let mut changed = self.select_row(absolute_row, usize::from(content_height));
        if is_provider
            && (action == PointerAction::Toggle
                || (action == PointerAction::Select && column == area.x))
        {
            changed |= self.toggle_selected_provider(usize::from(content_height));
        }
        changed
    }

    fn selected_row(&self) -> Option<usize> {
        let selected_provider = self.selected_provider.as_deref()?;
        let mut row = 0_usize;
        for provider in &self.providers {
            if !self.provider_is_visible(provider) {
                continue;
            }
            if provider == selected_provider && self.selection.is_none() {
                return Some(row);
            }
            row = row.saturating_add(1);
            if self.provider_is_collapsed(provider) {
                continue;
            }
            let provider_matches = self.provider_matches_filter(provider);
            for conversation in self.items.iter().filter(|conversation| {
                conversation.tool().as_str() == provider
                    && self.conversation_matches_filter(conversation, provider_matches)
            }) {
                if provider == selected_provider
                    && self.selection.as_ref() == Some(conversation.session_reference())
                {
                    return Some(row);
                }
                row = row.saturating_add(1);
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

    fn ensure_selected_row_visible(&mut self, viewport_height: usize) {
        let Some(row) = self.selected_row() else {
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

    #[must_use]
    pub const fn live_generations(&self) -> (u64, u64) {
        (self.live_requested_generation, self.live_applied_generation)
    }

    pub const fn set_live_generations(&mut self, requested: u64, applied: u64) {
        self.live_requested_generation = requested;
        self.live_applied_generation = applied;
    }

    #[must_use]
    pub const fn live_loading(&self) -> bool {
        self.live_loading
    }

    pub const fn set_live_loading(&mut self, loading: bool) {
        self.live_loading = loading;
    }
}

#[derive(Debug)]
pub struct AppModel {
    launch_context: LaunchContext,
    active_view: View,
    geometry: UiGeometry,
    files: FilesViewState,
    conversations: ConversationsViewState,
    config_warnings: Vec<String>,
    display_mode: DisplayMode,
    search_hint: String,
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
            config_warnings: Vec::new(),
            display_mode: DisplayMode::Ascii,
            search_hint: "/ search".to_owned(),
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
    pub fn config_warnings(&self) -> &[String] {
        &self.config_warnings
    }

    pub(crate) fn set_config_warnings(&mut self, warnings: Vec<String>) {
        self.config_warnings = warnings;
    }

    #[must_use]
    pub const fn display_mode(&self) -> DisplayMode {
        self.display_mode
    }

    pub const fn set_display_mode(&mut self, display_mode: DisplayMode) {
        self.display_mode = display_mode;
    }

    #[must_use]
    pub fn search_hint(&self) -> &str {
        &self.search_hint
    }

    pub fn set_search_hint(&mut self, hint: String) {
        self.search_hint = hint;
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
