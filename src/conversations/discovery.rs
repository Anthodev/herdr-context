//! Deterministic orchestration across registered conversation sources.

use std::collections::HashMap;

use crate::conversations::Conversation;
use crate::conversations::sources::{
    ConversationSourceError, ConversationSourceErrorKind, DiscoveryBatch, DiscoveryLimit,
    MetadataBudget, SourceId, SourceRegistry, SourceWatermark, StorageProbe,
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

/// Display-safe metadata and source-scoped diagnostics from one bounded run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationDiscovery {
    conversations: Vec<Conversation>,
    watermarks: HashMap<SourceId, SourceWatermark>,
    errors: Vec<ConversationSourceError>,
}

impl ConversationDiscovery {
    #[must_use]
    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    #[must_use]
    pub const fn watermarks(&self) -> &HashMap<SourceId, SourceWatermark> {
        &self.watermarks
    }

    #[must_use]
    pub fn errors(&self) -> &[ConversationSourceError] {
        &self.errors
    }

    pub fn into_conversations(self) -> Vec<Conversation> {
        self.conversations
    }
}

/// Probes and extracts bounded metadata from every healthy registered source.
///
/// Source, candidate, and evidence failures remain diagnostics for that source;
/// valid metadata from other files and sources is retained.
#[must_use]
pub fn discover_conversations(
    registry: &SourceRegistry,
    project: &ProjectIdentity,
    watermarks: &HashMap<SourceId, SourceWatermark>,
    limit: DiscoveryLimit,
    metadata_budget: MetadataBudget,
) -> ConversationDiscovery {
    let mut conversations = Vec::new();
    let mut next_watermarks = watermarks.clone();
    let mut errors = Vec::new();

    for source in registry.iter() {
        match source.probe() {
            Ok(StorageProbe::Available) => {}
            Ok(StorageProbe::Unavailable { .. }) => {
                next_watermarks.remove(source.source_id());
                continue;
            }
            Err(error) => {
                errors.push(error);
                continue;
            }
        }

        let batch = match source.discover(project, watermarks.get(source.source_id()), limit) {
            Ok(batch) => batch,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        errors.extend(batch.errors().iter().cloned());
        let mut candidate_failed = false;
        for candidate in batch.candidates() {
            let conversation = match source.extract_metadata(candidate, metadata_budget) {
                Ok(conversation) => conversation,
                Err(error) => {
                    candidate_failed = true;
                    errors.push(error);
                    continue;
                }
            };
            match source.project_evidence(candidate, project) {
                Ok(evidence)
                    if evidence
                        .iter()
                        .any(|item| item.canonical_path() == project.root()) =>
                {
                    conversations.push(conversation);
                }
                Ok(_) => {
                    candidate_failed = true;
                    errors.push(ConversationSourceError::new(
                        source.source_id().clone(),
                        ConversationSourceErrorKind::InvalidData,
                        "conversation candidate has no canonical project evidence",
                    ));
                }
                Err(error) => {
                    candidate_failed = true;
                    errors.push(error);
                }
            }
        }
        if !candidate_failed {
            if let Some(watermark) = batch.next_watermark() {
                next_watermarks.insert(source.source_id().clone(), watermark.clone());
            } else {
                next_watermarks.remove(source.source_id());
            }
        }
    }
    conversations.sort_unstable_by(|left, right| {
        right
            .updated_at()
            .cmp(&left.updated_at())
            .then_with(|| {
                left.session_reference()
                    .namespace()
                    .cmp(right.session_reference().namespace())
            })
            .then_with(|| {
                left.session_reference()
                    .id()
                    .cmp(right.session_reference().id())
            })
    });
    ConversationDiscovery {
        conversations,
        watermarks: next_watermarks,
        errors,
    }
}
