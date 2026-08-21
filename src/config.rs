//! Read-only, bounded plugin configuration with field-local fallback.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::intent::{Intent, View};
pub use crate::project::VcsBackendSelection;
use crate::vcs::jj::JujutsuMode;

const CONFIG_FILE: &str = "config.toml";
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MIN_DOCK_WIDTH: u16 = 24;
const MAX_DOCK_WIDTH: u16 = 60;
const DEFAULT_DOCK_WIDTH: u16 = 40;
const DEFAULT_HISTORY_PAGE_SIZE: usize = 128;
const MAX_HISTORY_PAGE_SIZE: usize = 512;
const DEFAULT_CACHE_ENTRIES: usize = 4_096;
const MAX_PROJECT_ROOTS: usize = 13;
const MAX_EXTERNAL_ROOTS: usize = 16;
const MAX_EXCLUSIONS: usize = 128;
const MAX_KEY_CHORDS_PER_ACTION: usize = 16;
const MAX_RELATIVE_PATH_BYTES: usize = 1_024;
const MAX_ABSOLUTE_PATH_BYTES: usize = 4_096;
const MIN_CADENCE_MS: u64 = 250;
const MIN_PASSIVE_JJ_CADENCE_MS: u64 = 1_000;
const MAX_CADENCE_MS: u64 = 300_000;
const DEFAULT_GIT_MIN_MS: u64 = 2_000;
const DEFAULT_GIT_MAX_MS: u64 = 30_000;
const KNOWN_SOURCE_IDS: [&str; 6] = [
    "claude-code",
    "codex-cli",
    "omp",
    "opencode",
    "pi",
    "project-local-generic-jsonl",
];
const EXTERNAL_SOURCE_IDS: [&str; 5] = ["claude-code", "codex-cli", "omp", "opencode", "pi"];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginConfig {
    dock: DockConfig,
    ui: UiConfig,
    files: FilesConfig,
    conversations: ConversationsConfig,
    vcs: VcsConfig,
    keybindings: KeyBindings,
}

impl PluginConfig {
    #[must_use]
    pub fn load_from_env() -> ConfigLoad {
        let Some(directory) =
            env::var_os("HERDR_PLUGIN_CONFIG_DIR").filter(|value| !value.is_empty())
        else {
            return ConfigLoad::with_warning(
                Self::default(),
                "Config: HERDR_PLUGIN_CONFIG_DIR is unavailable; using defaults",
            );
        };
        Self::load_from_dir(Path::new(&directory))
    }

    #[must_use]
    pub fn load_from_dir(directory: &Path) -> ConfigLoad {
        let path = directory.join(CONFIG_FILE);
        let bytes = match read_bounded_regular_file(&path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return ConfigLoad {
                    config: Self::default(),
                    warnings: Vec::new(),
                };
            }
            Err(_) => {
                return ConfigLoad::with_warning(
                    Self::default(),
                    "Config: config.toml is unreadable; using defaults",
                );
            }
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return ConfigLoad::with_warning(
                Self::default(),
                "Config: config.toml is not UTF-8; using defaults",
            );
        };
        let Ok(value) = toml::from_str::<toml::Value>(text) else {
            return ConfigLoad::with_warning(
                Self::default(),
                "Config: config.toml is malformed; using defaults",
            );
        };
        parse_config(&value)
    }

    #[must_use]
    pub const fn dock(&self) -> &DockConfig {
        &self.dock
    }

    #[must_use]
    pub const fn ui(&self) -> &UiConfig {
        &self.ui
    }

    #[must_use]
    pub const fn files(&self) -> &FilesConfig {
        &self.files
    }

    #[must_use]
    pub const fn conversations(&self) -> &ConversationsConfig {
        &self.conversations
    }

    #[must_use]
    pub const fn vcs(&self) -> &VcsConfig {
        &self.vcs
    }

    #[must_use]
    pub const fn keybindings(&self) -> &KeyBindings {
        &self.keybindings
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigLoad {
    config: PluginConfig,
    warnings: Vec<String>,
}

impl ConfigLoad {
    fn with_warning(config: PluginConfig, warning: &str) -> Self {
        Self {
            config,
            warnings: vec![warning.to_owned()],
        }
    }
    pub(crate) fn with_runtime_warning(warning: &str) -> Self {
        Self::with_warning(PluginConfig::default(), warning)
    }

    #[must_use]
    pub const fn config(&self) -> &PluginConfig {
        &self.config
    }

    #[must_use]
    pub fn into_config(self) -> PluginConfig {
        self.config
    }

    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    #[must_use]
    pub fn into_parts(self) -> (PluginConfig, Vec<String>) {
        (self.config, self.warnings)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DockConfig {
    initial_width: u16,
}

impl DockConfig {
    #[must_use]
    pub const fn initial_width(self) -> u16 {
        self.initial_width
    }
}

impl Default for DockConfig {
    fn default() -> Self {
        Self {
            initial_width: DEFAULT_DOCK_WIDTH,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DisplayMode {
    #[default]
    Ascii,
    Unicode,
    Nerd,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UiConfig {
    display_mode: DisplayMode,
}

impl UiConfig {
    #[must_use]
    pub const fn display_mode(self) -> DisplayMode {
        self.display_mode
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilesConfig {
    show_hidden: bool,
    exclusions: Vec<PathBuf>,
}

impl FilesConfig {
    #[must_use]
    pub const fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    #[must_use]
    pub fn exclusions(&self) -> &[PathBuf] {
        &self.exclusions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationsConfig {
    enabled_sources: BTreeSet<String>,
    project_roots: Vec<PathBuf>,
    external_roots: Vec<ExternalHistoryRoot>,
    page_size: NonZeroUsize,
    cache_entries: NonZeroUsize,
}

impl ConversationsConfig {
    #[must_use]
    pub const fn enabled_sources(&self) -> &BTreeSet<String> {
        &self.enabled_sources
    }

    #[must_use]
    pub fn source_enabled(&self, source: &str) -> bool {
        self.enabled_sources.contains(source)
    }

    #[must_use]
    pub fn project_roots(&self) -> &[PathBuf] {
        &self.project_roots
    }

    #[must_use]
    pub fn external_roots(&self) -> &[ExternalHistoryRoot] {
        &self.external_roots
    }

    #[must_use]
    pub const fn page_size(&self) -> NonZeroUsize {
        self.page_size
    }

    #[must_use]
    pub const fn cache_entries(&self) -> NonZeroUsize {
        self.cache_entries
    }
}

impl Default for ConversationsConfig {
    fn default() -> Self {
        Self {
            enabled_sources: KNOWN_SOURCE_IDS.into_iter().map(str::to_owned).collect(),
            project_roots: Vec::new(),
            external_roots: Vec::new(),
            page_size: NonZeroUsize::new(DEFAULT_HISTORY_PAGE_SIZE).expect("non-zero default"),
            cache_entries: NonZeroUsize::new(DEFAULT_CACHE_ENTRIES).expect("non-zero default"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalHistoryRoot {
    source: String,
    path: PathBuf,
}

impl ExternalHistoryRoot {
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VcsConfig {
    backend: VcsBackendSelection,
    jujutsu_mode: JujutsuMode,
    refresh: RefreshPolicy,
}

impl VcsConfig {
    #[must_use]
    pub const fn backend(self) -> VcsBackendSelection {
        self.backend
    }

    #[must_use]
    pub const fn jujutsu_mode(self) -> JujutsuMode {
        self.jujutsu_mode
    }

    #[must_use]
    pub const fn refresh(self) -> RefreshPolicy {
        self.refresh
    }
}

impl Default for VcsConfig {
    fn default() -> Self {
        Self {
            backend: VcsBackendSelection::Auto,
            jujutsu_mode: JujutsuMode::Fresh,
            refresh: RefreshPolicy::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GitCadence {
    #[default]
    Manual,
    Adaptive {
        minimum: Duration,
        maximum: Duration,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefreshPolicy {
    git: GitCadence,
    passive_jujutsu: Option<Duration>,
}

impl RefreshPolicy {
    #[must_use]
    pub const fn git(self) -> GitCadence {
        self.git
    }

    #[must_use]
    pub const fn passive_jujutsu(self) -> Option<Duration> {
        self.passive_jujutsu
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum KeyAction {
    Quit,
    NextView,
    PreviousView,
    FilesView,
    ConversationsView,
    SelectPrevious,
    SelectNext,
    SelectFirst,
    SelectLast,
    Expand,
    Collapse,
    Toggle,
    Refresh,
    ToggleFilesFocus,
    Search,
}

impl KeyAction {
    const ALL: [Self; 15] = [
        Self::Quit,
        Self::NextView,
        Self::PreviousView,
        Self::FilesView,
        Self::ConversationsView,
        Self::SelectPrevious,
        Self::SelectNext,
        Self::SelectFirst,
        Self::SelectLast,
        Self::Expand,
        Self::Collapse,
        Self::Toggle,
        Self::Refresh,
        Self::ToggleFilesFocus,
        Self::Search,
    ];

    const fn field(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::NextView => "next_view",
            Self::PreviousView => "previous_view",
            Self::FilesView => "files_view",
            Self::ConversationsView => "conversations_view",
            Self::SelectPrevious => "select_previous",
            Self::SelectNext => "select_next",
            Self::SelectFirst => "select_first",
            Self::SelectLast => "select_last",
            Self::Expand => "expand",
            Self::Collapse => "collapse",
            Self::Toggle => "toggle",
            Self::Refresh => "refresh",
            Self::ToggleFilesFocus => "toggle_files_focus",
            Self::Search => "search",
        }
    }

    const fn intent(self) -> Intent {
        match self {
            Self::Quit => Intent::Quit,
            Self::NextView => Intent::NextView,
            Self::PreviousView => Intent::PreviousView,
            Self::FilesView => Intent::SwitchView(View::Files),
            Self::ConversationsView => Intent::SwitchView(View::Conversations),
            Self::SelectPrevious => Intent::SelectPrevious,
            Self::SelectNext => Intent::SelectNext,
            Self::SelectFirst => Intent::SelectFirst,
            Self::SelectLast => Intent::SelectLast,
            Self::Expand => Intent::ExpandOrDescend,
            Self::Collapse => Intent::CollapseOrAscend,
            Self::Toggle => Intent::ToggleSelected,
            Self::Refresh => Intent::Refresh,
            Self::ToggleFilesFocus => Intent::SwitchFilesPane,
            Self::Search => Intent::BeginFileSearch,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyBindings {
    bindings: BTreeMap<KeyAction, Vec<KeyChord>>,
}

impl KeyBindings {
    #[must_use]
    pub fn map_key(&self, key: KeyEvent) -> Option<Intent> {
        let chord = KeyChord::from_event(key)?;
        self.bindings
            .iter()
            .find_map(|(action, chords)| chords.contains(&chord).then(|| action.intent()))
    }

    #[must_use]
    pub fn intent_for(&self, chord: &str) -> Option<Intent> {
        let chord = KeyChord::parse(chord)?;
        self.bindings
            .iter()
            .find_map(|(action, chords)| chords.contains(&chord).then(|| action.intent()))
    }

    #[must_use]
    pub fn bindings_for(&self, action: KeyAction) -> Vec<&str> {
        self.bindings
            .get(&action)
            .into_iter()
            .flatten()
            .map(|chord| chord.label.as_str())
            .collect()
    }
}

impl Default for KeyBindings {
    fn default() -> Self {
        let defaults = [
            (KeyAction::Quit, &["q", "esc", "ctrl+c"][..]),
            (KeyAction::NextView, &["tab"]),
            (KeyAction::PreviousView, &["backtab"]),
            (KeyAction::FilesView, &["1"]),
            (KeyAction::ConversationsView, &["2"]),
            (KeyAction::SelectPrevious, &["up", "k"]),
            (KeyAction::SelectNext, &["down", "j"]),
            (KeyAction::SelectFirst, &["home"]),
            (KeyAction::SelectLast, &["end"]),
            (KeyAction::Expand, &["right", "l"]),
            (KeyAction::Collapse, &["left", "h"]),
            (KeyAction::Toggle, &["enter", "space"]),
            (KeyAction::Refresh, &["r"]),
            (KeyAction::ToggleFilesFocus, &["w"]),
            (KeyAction::Search, &["/"]),
        ];
        Self {
            bindings: defaults
                .into_iter()
                .map(|(action, values)| {
                    (
                        action,
                        values
                            .iter()
                            .map(|value| KeyChord::parse(value).expect("valid default key"))
                            .collect(),
                    )
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
    label: String,
}
impl PartialEq for KeyChord {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.modifiers == other.modifiers
    }
}

impl Eq for KeyChord {}

impl Hash for KeyChord {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.code.hash(state);
        self.modifiers.hash(state);
    }
}

impl KeyChord {
    fn parse(value: &str) -> Option<Self> {
        if value.trim() != value || value.is_empty() || value.chars().any(char::is_control) {
            return None;
        }
        let normalized = value.to_ascii_lowercase();
        let mut parts = normalized.split('+').collect::<Vec<_>>();
        let key = parts.pop()?;
        let mut modifiers = KeyModifiers::NONE;
        for modifier in parts {
            let flag = match modifier {
                "ctrl" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => return None,
            };
            if modifiers.contains(flag) {
                return None;
            }
            modifiers.insert(flag);
        }
        let code = match key {
            "esc" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => {
                modifiers.insert(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "enter" => KeyCode::Enter,
            "space" => KeyCode::Char(' '),
            value if value.chars().count() == 1 => KeyCode::Char(value.chars().next()?),
            _ => return None,
        };
        Some(Self {
            code,
            modifiers,
            label: normalized,
        })
    }

    fn from_event(key: KeyEvent) -> Option<Self> {
        let code = match key.code {
            KeyCode::Char(character) => KeyCode::Char(character.to_ascii_lowercase()),
            KeyCode::Esc
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Enter => key.code,
            _ => return None,
        };
        let mut modifiers =
            key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        if code == KeyCode::BackTab {
            modifiers.insert(KeyModifiers::SHIFT);
        }
        Some(Self {
            code,
            modifiers,
            label: String::new(),
        })
    }
}

fn parse_config(value: &toml::Value) -> ConfigLoad {
    let mut config = PluginConfig::default();
    let mut warnings = Vec::new();
    let Some(root) = value.as_table() else {
        return ConfigLoad::with_warning(
            config,
            "Config: config.toml root is invalid; using defaults",
        );
    };
    warn_unknown_fields(
        root,
        &["dock", "ui", "files", "conversations", "vcs", "keybindings"],
        "root.unknown_field",
        &mut warnings,
    );

    if let Some(table) = optional_table(root, "dock", &mut warnings) {
        warn_unknown_fields(
            table,
            &["initial_width"],
            "dock.unknown_field",
            &mut warnings,
        );
        config.dock.initial_width = parse_u16(
            table.get("initial_width"),
            MIN_DOCK_WIDTH,
            MAX_DOCK_WIDTH,
            "dock.initial_width",
            &mut warnings,
        )
        .unwrap_or(DEFAULT_DOCK_WIDTH);
    }

    if let Some(table) = optional_table(root, "ui", &mut warnings) {
        warn_unknown_fields(table, &["display_mode"], "ui.unknown_field", &mut warnings);
        config.ui.display_mode = table
            .get("display_mode")
            .map_or(DisplayMode::Ascii, |value| {
                match parse_string(Some(value)) {
                    Some("ascii") => DisplayMode::Ascii,
                    Some("unicode") => DisplayMode::Unicode,
                    Some("nerd") => DisplayMode::Nerd,
                    Some(_) | None => {
                        invalid("ui.display_mode", &mut warnings);
                        DisplayMode::Ascii
                    }
                }
            });
    }

    if let Some(table) = optional_table(root, "files", &mut warnings) {
        warn_unknown_fields(
            table,
            &["show_hidden", "exclusions"],
            "files.unknown_field",
            &mut warnings,
        );
        config.files.show_hidden =
            parse_bool(table.get("show_hidden"), "files.show_hidden", &mut warnings)
                .unwrap_or(false);
        if let Some(value) = table.get("exclusions") {
            config.files.exclusions =
                parse_relative_paths(value, MAX_EXCLUSIONS, "files.exclusions", &mut warnings);
        }
    }

    if let Some(table) = optional_table(root, "conversations", &mut warnings) {
        warn_unknown_fields(
            table,
            &[
                "enabled_sources",
                "project_roots",
                "page_size",
                "cache_entries",
                "external_roots",
            ],
            "conversations.unknown_field",
            &mut warnings,
        );
        if let Some(value) = table.get("enabled_sources") {
            config.conversations.enabled_sources =
                parse_enabled_sources(value, &mut warnings).unwrap_or_else(default_sources);
        }
        if let Some(value) = table.get("project_roots") {
            config.conversations.project_roots = parse_relative_paths(
                value,
                MAX_PROJECT_ROOTS,
                "conversations.project_roots",
                &mut warnings,
            );
        }
        config.conversations.page_size = parse_nonzero_usize(
            table.get("page_size"),
            1,
            MAX_HISTORY_PAGE_SIZE,
            DEFAULT_HISTORY_PAGE_SIZE,
            "conversations.page_size",
            &mut warnings,
        );
        config.conversations.cache_entries = parse_nonzero_usize(
            table.get("cache_entries"),
            16,
            DEFAULT_CACHE_ENTRIES,
            DEFAULT_CACHE_ENTRIES,
            "conversations.cache_entries",
            &mut warnings,
        );
        if let Some(value) = table.get("external_roots") {
            config.conversations.external_roots = parse_external_roots(value, &mut warnings);
        }
    }

    if let Some(table) = optional_table(root, "vcs", &mut warnings) {
        warn_unknown_fields(
            table,
            &[
                "backend",
                "jujutsu_mode",
                "git_cadence",
                "git_min_interval_ms",
                "git_max_interval_ms",
                "passive_jujutsu_interval_ms",
            ],
            "vcs.unknown_field",
            &mut warnings,
        );
        config.vcs.backend = table
            .get("backend")
            .map_or(VcsBackendSelection::Auto, |value| {
                match parse_string(Some(value)) {
                    Some("auto") => VcsBackendSelection::Auto,
                    Some("git") => VcsBackendSelection::Git,
                    Some("jj") => VcsBackendSelection::Jujutsu,
                    Some(_) | None => {
                        invalid("vcs.backend", &mut warnings);
                        VcsBackendSelection::Auto
                    }
                }
            });
        config.vcs.jujutsu_mode = table
            .get("jujutsu_mode")
            .map_or(JujutsuMode::Fresh, |value| {
                match parse_string(Some(value)) {
                    Some("fresh") => JujutsuMode::Fresh,
                    Some("passive") => JujutsuMode::Passive,
                    Some(_) | None => {
                        invalid("vcs.jujutsu_mode", &mut warnings);
                        JujutsuMode::Fresh
                    }
                }
            });
        config.vcs.refresh = parse_refresh_policy(table, &mut warnings);
    }

    if let Some(table) = optional_table(root, "keybindings", &mut warnings) {
        let fields = KeyAction::ALL.map(KeyAction::field);
        warn_unknown_fields(table, &fields, "keybindings.unknown_field", &mut warnings);
        config.keybindings = parse_keybindings(table, &mut warnings);
    }

    ConfigLoad { config, warnings }
}

fn optional_table<'a>(
    root: &'a toml::Table,
    field: &str,
    warnings: &mut Vec<String>,
) -> Option<&'a toml::Table> {
    let value = root.get(field)?;
    value.as_table().map_or_else(
        || {
            invalid(field, warnings);
            None
        },
        Some,
    )
}
fn warn_unknown_fields(
    table: &toml::Table,
    allowed: &[&str],
    warning_field: &str,
    warnings: &mut Vec<String>,
) {
    if table.keys().any(|field| !allowed.contains(&field.as_str())) {
        invalid(warning_field, warnings);
    }
}

fn parse_u16(
    value: Option<&toml::Value>,
    minimum: u16,
    maximum: u16,
    field: &str,
    warnings: &mut Vec<String>,
) -> Option<u16> {
    let value = value?;
    let parsed = value
        .as_integer()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| (minimum..=maximum).contains(value));
    if parsed.is_none() {
        invalid(field, warnings);
    }
    parsed
}

fn parse_bool(
    value: Option<&toml::Value>,
    field: &str,
    warnings: &mut Vec<String>,
) -> Option<bool> {
    let value = value?;
    let parsed = value.as_bool();
    if parsed.is_none() {
        invalid(field, warnings);
    }
    parsed
}

fn parse_string(value: Option<&toml::Value>) -> Option<&str> {
    value?.as_str().filter(|value| {
        value.trim() == *value && !value.is_empty() && !value.chars().any(char::is_control)
    })
}

fn parse_nonzero_usize(
    value: Option<&toml::Value>,
    minimum: usize,
    maximum: usize,
    default: usize,
    field: &str,
    warnings: &mut Vec<String>,
) -> NonZeroUsize {
    let Some(value) = value else {
        return NonZeroUsize::new(default).expect("non-zero default");
    };
    let parsed = value
        .as_integer()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| (minimum..=maximum).contains(value));
    parsed.and_then(NonZeroUsize::new).unwrap_or_else(|| {
        invalid(field, warnings);
        NonZeroUsize::new(default).expect("non-zero default")
    })
}

fn parse_relative_paths(
    value: &toml::Value,
    maximum: usize,
    field: &str,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let Some(values) = value.as_array().filter(|values| values.len() <= maximum) else {
        invalid(field, warnings);
        return Vec::new();
    };
    let mut paths = Vec::with_capacity(values.len());
    let mut rejected = false;
    for value in values {
        let Some(value) = parse_string(Some(value)) else {
            rejected = true;
            continue;
        };
        let path = PathBuf::from(value);
        if !is_normal_relative_path(&path)
            || path.as_os_str().as_encoded_bytes().len() > MAX_RELATIVE_PATH_BYTES
        {
            rejected = true;
            continue;
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    if rejected {
        invalid(field, warnings);
    }
    paths
}

fn parse_enabled_sources(
    value: &toml::Value,
    warnings: &mut Vec<String>,
) -> Option<BTreeSet<String>> {
    let Some(values) = value.as_array() else {
        invalid("conversations.enabled_sources", warnings);
        return None;
    };
    if values.len() > KNOWN_SOURCE_IDS.len() {
        invalid("conversations.enabled_sources", warnings);
        return None;
    }
    let mut sources = BTreeSet::new();
    let mut rejected = false;
    for value in values {
        let Some(source) = parse_string(Some(value)) else {
            rejected = true;
            continue;
        };
        if KNOWN_SOURCE_IDS.contains(&source) {
            sources.insert(source.to_owned());
        } else {
            rejected = true;
        }
    }
    if rejected {
        invalid("conversations.enabled_sources", warnings);
    }
    Some(sources)
}

fn default_sources() -> BTreeSet<String> {
    KNOWN_SOURCE_IDS.into_iter().map(str::to_owned).collect()
}

fn parse_external_roots(
    value: &toml::Value,
    warnings: &mut Vec<String>,
) -> Vec<ExternalHistoryRoot> {
    let Some(table) = value.as_table() else {
        invalid("conversations.external_roots", warnings);
        return Vec::new();
    };
    let mut roots = Vec::new();
    let mut rejected = false;
    for (source, value) in table {
        if !EXTERNAL_SOURCE_IDS.contains(&source.as_str()) {
            rejected = true;
            continue;
        }
        let Some(paths) = value.as_array() else {
            rejected = true;
            continue;
        };
        for path in paths {
            let Some(path) = parse_string(Some(path)).map(PathBuf::from) else {
                rejected = true;
                continue;
            };
            if !path.is_absolute()
                || path.as_os_str().as_encoded_bytes().len() > MAX_ABSOLUTE_PATH_BYTES
                || roots.len() >= MAX_EXTERNAL_ROOTS
            {
                rejected = true;
                continue;
            }
            roots.push(ExternalHistoryRoot {
                source: source.clone(),
                path,
            });
        }
    }
    roots.sort_unstable_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.path.cmp(&right.path))
    });
    roots.dedup_by(|left, right| left.source == right.source && left.path == right.path);
    if rejected {
        invalid("conversations.external_roots", warnings);
    }
    roots
}

fn parse_refresh_policy(table: &toml::Table, warnings: &mut Vec<String>) -> RefreshPolicy {
    let cadence = parse_string(table.get("git_cadence")).unwrap_or("manual");
    let git = match cadence {
        "manual" => GitCadence::Manual,
        "adaptive" => {
            let minimum = parse_duration_ms(
                table.get("git_min_interval_ms"),
                MIN_CADENCE_MS,
                MAX_CADENCE_MS,
                DEFAULT_GIT_MIN_MS,
                "vcs.git_min_interval_ms",
                warnings,
            );
            let maximum = parse_duration_ms(
                table.get("git_max_interval_ms"),
                MIN_CADENCE_MS,
                MAX_CADENCE_MS,
                DEFAULT_GIT_MAX_MS,
                "vcs.git_max_interval_ms",
                warnings,
            );
            if minimum > maximum {
                invalid("vcs.git_cadence", warnings);
                GitCadence::Adaptive {
                    minimum: Duration::from_millis(DEFAULT_GIT_MIN_MS),
                    maximum: Duration::from_millis(DEFAULT_GIT_MAX_MS),
                }
            } else {
                GitCadence::Adaptive { minimum, maximum }
            }
        }
        _ => {
            invalid("vcs.git_cadence", warnings);
            GitCadence::Manual
        }
    };
    if table.contains_key("git_cadence") && parse_string(table.get("git_cadence")).is_none() {
        invalid("vcs.git_cadence", warnings);
    }
    let passive_jujutsu = match table.get("passive_jujutsu_interval_ms") {
        None => None,
        Some(value) if value.as_integer() == Some(0) => None,
        Some(value) => value
            .as_integer()
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| (MIN_PASSIVE_JJ_CADENCE_MS..=MAX_CADENCE_MS).contains(value))
            .map(Duration::from_millis)
            .or_else(|| {
                invalid("vcs.passive_jujutsu_interval_ms", warnings);
                None
            }),
    };
    RefreshPolicy {
        git,
        passive_jujutsu,
    }
}

fn parse_duration_ms(
    value: Option<&toml::Value>,
    minimum: u64,
    maximum: u64,
    default: u64,
    field: &str,
    warnings: &mut Vec<String>,
) -> Duration {
    let Some(value) = value else {
        return Duration::from_millis(default);
    };
    value
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| (minimum..=maximum).contains(value))
        .map_or_else(
            || {
                invalid(field, warnings);
                Duration::from_millis(default)
            },
            Duration::from_millis,
        )
}

fn parse_keybindings(table: &toml::Table, warnings: &mut Vec<String>) -> KeyBindings {
    let defaults = KeyBindings::default();
    let mut bindings = defaults.bindings.clone();
    let mut configured = BTreeSet::new();
    for action in KeyAction::ALL {
        let Some(value) = table.get(action.field()) else {
            continue;
        };
        let parsed = value
            .as_array()
            .filter(|values| (1..=MAX_KEY_CHORDS_PER_ACTION).contains(&values.len()))
            .and_then(|values| {
                let mut chords = Vec::with_capacity(values.len());
                for value in values {
                    let chord = parse_string(Some(value)).and_then(KeyChord::parse)?;
                    if !chords.contains(&chord) {
                        chords.push(chord);
                    }
                }
                (!chords.is_empty()).then_some(chords)
            });
        match parsed {
            Some(chords) => {
                bindings.insert(action, chords);
                configured.insert(action);
            }
            None => invalid(&format!("keybindings.{}", action.field()), warnings),
        }
    }

    loop {
        let mut owners: HashMap<&KeyChord, Vec<KeyAction>> = HashMap::new();
        for (action, chords) in &bindings {
            for chord in chords {
                owners.entry(chord).or_default().push(*action);
            }
        }
        let conflicts = owners
            .values()
            .filter(|owners| owners.len() > 1)
            .flat_map(|owners| owners.iter().copied())
            .filter(|action| configured.contains(action))
            .collect::<BTreeSet<_>>();
        if conflicts.is_empty() {
            break;
        }
        for action in conflicts {
            bindings.insert(
                action,
                defaults
                    .bindings
                    .get(&action)
                    .expect("default action")
                    .clone(),
            );
            configured.remove(&action);
            invalid(&format!("keybindings.{}", action.field()), warnings);
        }
    }
    KeyBindings { bindings }
}

fn is_normal_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn invalid(field: &str, warnings: &mut Vec<String>) {
    let warning = format!("Config: {field} is invalid; using safe fallback");
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

fn read_bounded_regular_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration file is not a bounded regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file: File = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration file changed during open",
        ));
    }
    let capacity = usize::try_from(opened.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    (&mut file)
        .take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration file exceeds the byte limit",
        ));
    }
    Ok(Some(bytes))
}
