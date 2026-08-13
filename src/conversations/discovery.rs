//! Deterministic orchestration across registered conversation sources.

use std::collections::HashMap;

use crate::conversations::sources::{
    ConversationSourceError, DiscoveryBatch, DiscoveryLimit, SourceId, SourceRegistry,
    SourceWatermark, StorageProbe,
};
use crate::project::ProjectIdentity;

/// One source-scoped operation result.
///
/// Keeping the source identity beside both success and failure values lets
/// callers retain healthy results without flattening or short-circuiting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceOutcome<T> {
    source_id: SourceId,
    result: Result<T, ConversationSourceError>,
}

impl<T> SourceOutcome<T> {
    const fn new(source_id: SourceId, result: Result<T, ConversationSourceError>) -> Self {
        Self { source_id, result }
    }

    #[must_use]
    pub const fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    pub const fn result(&self) -> Result<&T, &ConversationSourceError> {
        self.result.as_ref()
    }

    pub fn into_result(self) -> Result<T, ConversationSourceError> {
        self.result
    }
}

/// Probes every registered source in deterministic registry order.
#[must_use]
pub fn probe_sources(registry: &SourceRegistry) -> Vec<SourceOutcome<StorageProbe>> {
    registry
        .iter()
        .map(|source| SourceOutcome::new(source.source_id().clone(), source.probe()))
        .collect()
}

/// Discovers every registered source in deterministic registry order.
///
/// Watermarks are source-scoped. Missing entries request initial discovery;
/// foreign watermark values are rejected by the unchanged HDC-5 checked
/// `ConversationSource::discover` contract and remain isolated to that source.
#[must_use]
pub fn discover_sources(
    registry: &SourceRegistry,
    project: &ProjectIdentity,
    watermarks: &HashMap<SourceId, SourceWatermark>,
    limit: DiscoveryLimit,
) -> Vec<SourceOutcome<DiscoveryBatch>> {
    registry
        .iter()
        .map(|source| {
            let result = source.discover(project, watermarks.get(source.source_id()), limit);
            SourceOutcome::new(source.source_id().clone(), result)
        })
        .collect()
}
