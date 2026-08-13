//! Herdr process boundary and normalized launch context.

pub mod client;
pub mod launch;

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use serde_json::Value;

macro_rules! host_id {
    ($name:ident, $field:literal) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, LaunchContextError> {
                crate::normalize_nonempty(value)
                    .map(Self)
                    .ok_or(LaunchContextError::InvalidIdentifier($field))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

host_id!(WorkspaceId, "workspace id");
host_id!(TabId, "tab id");
host_id!(PaneId, "pane id");

/// Immutable context captured before dock startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchContext {
    workspace_id: WorkspaceId,
    tab_id: TabId,
    focused_pane_id: PaneId,
    cwd: PathBuf,
    foreground_cwd: Option<PathBuf>,
}

impl LaunchContext {
    /// Reads Herdr's public plugin context and authoritative injected id variables.
    pub fn from_env() -> Result<Self, LaunchContextError> {
        Self::from_lookup(|name| match env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => {
                Err(LaunchContextError::NonUnicodeVariable(name.to_owned()))
            }
        })
    }

    /// Deterministic seam for tests and launchers that already captured environment.
    pub fn from_vars<I, K, V>(vars: I) -> Result<Self, LaunchContextError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let vars = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<BTreeMap<_, _>>();
        Self::from_lookup(|name| Ok(vars.get(name).cloned()))
    }

    fn from_lookup<F>(mut lookup: F) -> Result<Self, LaunchContextError>
    where
        F: FnMut(&'static str) -> Result<Option<String>, LaunchContextError>,
    {
        let raw_json = lookup("HERDR_PLUGIN_CONTEXT_JSON")?.ok_or(
            LaunchContextError::MissingVariable("HERDR_PLUGIN_CONTEXT_JSON"),
        )?;
        let context: Value = serde_json::from_str(&raw_json)
            .map_err(|error| LaunchContextError::MalformedContext(error.to_string()))?;
        if !context.is_object() {
            return Err(LaunchContextError::MalformedContext(
                "plugin context must be a JSON object".to_owned(),
            ));
        }

        let workspace_id = authoritative_id(
            lookup("HERDR_WORKSPACE_ID")?,
            &context,
            &["/workspace_id", "/workspace/id", "/workspace/workspace_id"],
            "HERDR_WORKSPACE_ID",
            WorkspaceId::new,
        )?;
        let tab_id = authoritative_id(
            lookup("HERDR_TAB_ID")?,
            &context,
            &["/tab_id", "/tab/id", "/tab/tab_id"],
            "HERDR_TAB_ID",
            TabId::new,
        )?;
        let focused_pane_id = authoritative_id(
            lookup("HERDR_PANE_ID")?,
            &context,
            &[
                "/focused_pane_id",
                "/pane_id",
                "/focused_pane/pane_id",
                "/focused_pane/id",
                "/pane/pane_id",
                "/pane/id",
            ],
            "HERDR_PANE_ID",
            PaneId::new,
        )?;
        let cwd = required_path(
            &context,
            &[
                "/cwd",
                "/focused_pane_cwd",
                "/focused_pane/cwd",
                "/pane/cwd",
                "/workspace_cwd",
            ],
            "cwd",
        )?;
        let foreground_cwd = optional_path(
            &context,
            &[
                "/foreground_cwd",
                "/focused_pane/foreground_cwd",
                "/pane/foreground_cwd",
            ],
            "foreground_cwd",
        )?;

        Ok(Self {
            workspace_id,
            tab_id,
            focused_pane_id,
            cwd,
            foreground_cwd,
        })
    }

    #[must_use]
    pub const fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    #[must_use]
    pub const fn tab_id(&self) -> &TabId {
        &self.tab_id
    }

    #[must_use]
    pub const fn focused_pane_id(&self) -> &PaneId {
        &self.focused_pane_id
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn foreground_cwd(&self) -> Option<&Path> {
        self.foreground_cwd.as_deref()
    }
}

fn authoritative_id<T, F>(
    injected: Option<String>,
    context: &Value,
    pointers: &[&'static str],
    missing_name: &'static str,
    constructor: F,
) -> Result<T, LaunchContextError>
where
    F: FnOnce(String) -> Result<T, LaunchContextError>,
{
    let value = match injected {
        Some(value) => value,
        None => first_json_string(context, pointers)?
            .ok_or(LaunchContextError::MissingIdentifier(missing_name))?,
    };
    constructor(value)
}

fn required_path(
    context: &Value,
    pointers: &[&'static str],
    field: &'static str,
) -> Result<PathBuf, LaunchContextError> {
    optional_path(context, pointers, field)?.ok_or(LaunchContextError::MissingField(field))
}

fn optional_path(
    context: &Value,
    pointers: &[&'static str],
    field: &'static str,
) -> Result<Option<PathBuf>, LaunchContextError> {
    let Some(value) = first_json_string(context, pointers)? else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(LaunchContextError::InvalidPath(field));
    }
    Ok(Some(path))
}

fn first_json_string(
    context: &Value,
    pointers: &[&'static str],
) -> Result<Option<String>, LaunchContextError> {
    for pointer in pointers {
        match context.pointer(pointer) {
            None | Some(Value::Null) => {}
            Some(Value::String(value)) => return Ok(Some(value.clone())),
            Some(_) => return Err(LaunchContextError::InvalidContextField(pointer)),
        }
    }
    Ok(None)
}

pub const MIN_DOCK_WIDTH: u16 = 24;
pub const DEFAULT_DOCK_WIDTH: u16 = 40;
pub const MAX_DOCK_WIDTH: u16 = 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DockWidth(NonZeroU16);

impl DockWidth {
    pub fn new(columns: u16) -> Option<Self> {
        NonZeroU16::new(columns).map(Self)
    }

    #[must_use]
    pub fn clamped(columns: u16) -> Self {
        let columns = columns.clamp(MIN_DOCK_WIDTH, MAX_DOCK_WIDTH);
        Self(NonZeroU16::new(columns).unwrap_or(NonZeroU16::MIN))
    }

    #[must_use]
    pub const fn columns(self) -> u16 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DockIdentity {
    PluginMetadata,
    OscTitle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPane {
    pane_id: PaneId,
    tab_id: TabId,
    cwd: Option<PathBuf>,
    foreground_cwd: Option<PathBuf>,
    focused: bool,
    dock_identity: Option<DockIdentity>,
}

impl HostPane {
    #[must_use]
    pub const fn new(
        pane_id: PaneId,
        tab_id: TabId,
        cwd: Option<PathBuf>,
        foreground_cwd: Option<PathBuf>,
        focused: bool,
    ) -> Self {
        Self {
            pane_id,
            tab_id,
            cwd,
            foreground_cwd,
            focused,
            dock_identity: None,
        }
    }

    #[must_use]
    pub const fn pane_id(&self) -> &PaneId {
        &self.pane_id
    }

    #[must_use]
    pub const fn tab_id(&self) -> &TabId {
        &self.tab_id
    }

    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    #[must_use]
    pub fn foreground_cwd(&self) -> Option<&Path> {
        self.foreground_cwd.as_deref()
    }

    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    #[must_use]
    pub const fn with_dock_identity(mut self, identity: DockIdentity) -> Self {
        self.dock_identity = Some(identity);
        self
    }

    #[must_use]
    pub const fn dock_identity(&self) -> Option<DockIdentity> {
        self.dock_identity
    }

    #[must_use]
    pub const fn is_dock(&self) -> bool {
        self.dock_identity.is_some()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenDockRequest {
    origin_pane_id: PaneId,
    tab_id: TabId,
    cwd: PathBuf,
    width: DockWidth,
}

impl OpenDockRequest {
    #[must_use]
    pub const fn new(
        origin_pane_id: PaneId,
        tab_id: TabId,
        cwd: PathBuf,
        width: DockWidth,
    ) -> Self {
        Self {
            origin_pane_id,
            tab_id,
            cwd,
            width,
        }
    }

    #[must_use]
    pub const fn origin_pane_id(&self) -> &PaneId {
        &self.origin_pane_id
    }

    #[must_use]
    pub const fn tab_id(&self) -> &TabId {
        &self.tab_id
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub const fn width(&self) -> DockWidth {
        self.width
    }
}

/// Herdr boundary required by launcher work. Implementations own CLI/socket details.
pub trait HostClient: Send {
    fn pane(&self, pane_id: &PaneId) -> Result<Option<HostPane>, HostError>;
    fn panes_in_tab(
        &self,
        workspace_id: &WorkspaceId,
        tab_id: &TabId,
    ) -> Result<Vec<HostPane>, HostError>;
    fn verified_dock_identity(
        &mut self,
        pane: &HostPane,
    ) -> Result<Option<DockIdentity>, HostError>;
    fn open_dock(&mut self, request: &OpenDockRequest) -> Result<PaneId, HostError>;
    fn focus_pane(&mut self, pane_id: &PaneId) -> Result<(), HostError>;
    fn close_pane(&mut self, pane_id: &PaneId) -> Result<(), HostError>;
    fn move_to_right_edge(&mut self, pane_id: &PaneId) -> Result<(), HostError>;
    fn resize_pane(&mut self, pane_id: &PaneId, width: DockWidth) -> Result<(), HostError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostErrorKind {
    Unavailable,
    NotFound,
    PermissionDenied,
    InvalidResponse,
    OperationFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostError {
    kind: HostErrorKind,
    message: String,
}

impl HostError {
    #[must_use]
    pub fn new(kind: HostErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> HostErrorKind {
        self.kind
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl Error for HostError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchContextError {
    MissingVariable(&'static str),
    NonUnicodeVariable(String),
    MalformedContext(String),
    MissingIdentifier(&'static str),
    InvalidIdentifier(&'static str),
    MissingField(&'static str),
    InvalidContextField(&'static str),
    InvalidPath(&'static str),
}

impl fmt::Display for LaunchContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingVariable(name) => write!(formatter, "missing required variable {name}"),
            Self::NonUnicodeVariable(name) => write!(formatter, "variable {name} is not Unicode"),
            Self::MalformedContext(reason) => {
                write!(formatter, "malformed Herdr context: {reason}")
            }
            Self::MissingIdentifier(name) => {
                write!(formatter, "missing required identifier {name}")
            }
            Self::InvalidIdentifier(field) => write!(formatter, "{field} must be non-empty"),
            Self::MissingField(field) => {
                write!(formatter, "missing required context field {field}")
            }
            Self::InvalidContextField(pointer) => {
                write!(formatter, "context field {pointer} must be a string")
            }
            Self::InvalidPath(field) => write!(formatter, "{field} must be an absolute path"),
        }
    }
}

impl Error for LaunchContextError {}

#[cfg(test)]
mod tests {
    use super::{LaunchContext, LaunchContextError};

    fn valid_vars() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "HERDR_PLUGIN_CONTEXT_JSON",
                r#"{"workspace_id":"json-workspace","tab_id":"json-tab","pane_id":"json-pane","cwd":"/project","foreground_cwd":"/project/subdir"}"#,
            ),
            ("HERDR_WORKSPACE_ID", "env-workspace"),
            ("HERDR_TAB_ID", "env-tab"),
            ("HERDR_PANE_ID", "env-pane"),
        ]
    }

    #[test]
    fn parses_context_and_prefers_authoritative_ids() -> Result<(), Box<dyn std::error::Error>> {
        let context = LaunchContext::from_vars(valid_vars())?;

        assert_eq!(context.workspace_id().as_str(), "env-workspace");
        assert_eq!(context.tab_id().as_str(), "env-tab");
        assert_eq!(context.focused_pane_id().as_str(), "env-pane");
        assert_eq!(context.cwd().to_string_lossy(), "/project");
        assert_eq!(
            context.foreground_cwd().map(Path::to_string_lossy),
            Some("/project/subdir".into())
        );
        Ok(())
    }

    #[test]
    fn supports_nested_focused_pane_context() -> Result<(), Box<dyn std::error::Error>> {
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"workspace":{"id":"workspace"},"tab":{"id":"tab"},"focused_pane":{"id":"pane","cwd":"/project"}}"#,
        )])?;

        assert_eq!(context.focused_pane_id().as_str(), "pane");
        assert_eq!(context.foreground_cwd(), None);
        Ok(())
    }

    #[test]
    fn supports_public_plugin_invocation_context_shape() -> Result<(), Box<dyn std::error::Error>> {
        let context = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"workspace_id":"workspace","tab_id":"tab","focused_pane_id":"pane","focused_pane_cwd":"/project","workspace_cwd":"/workspace"}"#,
        )])?;

        assert_eq!(context.cwd(), Path::new("/project"));
        Ok(())
    }

    #[test]
    fn malformed_json_is_typed_error() {
        let error = LaunchContext::from_vars([("HERDR_PLUGIN_CONTEXT_JSON", "{")]);
        assert!(matches!(
            error,
            Err(LaunchContextError::MalformedContext(_))
        ));
    }

    #[test]
    fn missing_identifier_is_typed_error() {
        let error = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"tab_id":"tab","pane_id":"pane","cwd":"/project"}"#,
        )]);
        assert_eq!(
            error,
            Err(LaunchContextError::MissingIdentifier("HERDR_WORKSPACE_ID"))
        );
    }

    #[test]
    fn relative_cwd_is_rejected() {
        let error = LaunchContext::from_vars([(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"workspace_id":"workspace","tab_id":"tab","pane_id":"pane","cwd":"relative"}"#,
        )]);
        assert_eq!(error, Err(LaunchContextError::InvalidPath("cwd")));
    }

    use std::path::Path;
}
