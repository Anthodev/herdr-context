//! Deterministic orchestration across registered conversation sources.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

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
    removals: Vec<crate::conversations::sources::ConversationRemoval>,
    purged_sources: Vec<SourceId>,
    errors: Vec<ConversationSourceError>,
    has_more: bool,
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
    pub fn removals(&self) -> &[crate::conversations::sources::ConversationRemoval] {
        &self.removals
    }
    #[must_use]
    pub fn purged_sources(&self) -> &[SourceId] {
        &self.purged_sources
    }

    #[must_use]
    pub fn errors(&self) -> &[ConversationSourceError] {
        &self.errors
    }
    #[must_use]
    pub const fn has_more(&self) -> bool {
        self.has_more
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
    discover_conversations_cancellable(
        registry,
        project,
        watermarks,
        limit,
        metadata_budget,
        &AtomicBool::new(false),
    )
}

#[must_use]
pub fn discover_conversations_cancellable(
    registry: &SourceRegistry,
    project: &ProjectIdentity,
    watermarks: &HashMap<SourceId, SourceWatermark>,
    limit: DiscoveryLimit,
    metadata_budget: MetadataBudget,
    cancelled: &AtomicBool,
) -> ConversationDiscovery {
    let mut conversations = Vec::new();
    let mut next_watermarks = watermarks.clone();
    let mut errors = Vec::new();
    let mut removals = Vec::new();
    let mut purged_sources = Vec::new();
    let mut has_more = false;
    let unregistered = watermarks
        .keys()
        .filter(|source_id| !registry.retains(source_id))
        .cloned()
        .collect::<Vec<_>>();
    for source_id in unregistered {
        next_watermarks.remove(&source_id);
        purged_sources.push(source_id);
    }

    for source in registry.iter() {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        match source.probe() {
            Ok(StorageProbe::Available) => {}
            Ok(StorageProbe::Unavailable { reason }) => {
                errors.push(ConversationSourceError::unavailable(
                    source.source_id().clone(),
                    reason,
                ));
                continue;
            }
            Err(error) => {
                errors.push(error);
                continue;
            }
        }

        let batch = match source.discover_cancellable(
            project,
            watermarks.get(source.source_id()),
            limit,
            cancelled,
        ) {
            Ok(batch) => batch,
            Err(error) => {
                if error.kind() == ConversationSourceErrorKind::InvalidData
                    && watermarks.contains_key(source.source_id())
                {
                    next_watermarks.remove(source.source_id());
                    purged_sources.push(source.source_id().clone());
                    has_more = true;
                }
                errors.push(error);
                continue;
            }
        };
        errors.extend(batch.errors().iter().cloned());
        let mut candidate_failed = false;
        for candidate in batch.candidates() {
            if cancelled.load(Ordering::Relaxed) {
                candidate_failed = true;
                break;
            }
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
            removals.extend(batch.removals().iter().cloned());
            has_more |= batch.has_more();
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
        removals,
        purged_sources,
        errors,
        has_more,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    use tempfile::TempDir;

    use super::discover_conversations_cancellable;
    use crate::conversations::sources::{
        DiscoveryLimit, MetadataBudget, SourceId, SourceRegistry, SourceWatermark,
    };
    use crate::project::ProjectIdentity;

    #[test]
    fn desired_source_without_a_runtime_adapter_retains_its_watermark() {
        let source_id = SourceId::new("temporarily-unavailable").expect("source ID");
        let registry =
            SourceRegistry::new_with_desired_source_ids(Vec::new(), vec![source_id.clone()])
                .expect("registry");
        let watermark =
            SourceWatermark::new(source_id.clone(), "cached-cursor").expect("watermark");
        let watermarks = HashMap::from([(source_id.clone(), watermark)]);
        let project = TempDir::new().expect("project");
        let project = ProjectIdentity::from_canonical_root(project.path().to_path_buf())
            .expect("project identity");

        let discovery = discover_conversations_cancellable(
            &registry,
            &project,
            &watermarks,
            DiscoveryLimit::new(1).expect("limit"),
            MetadataBudget::new(1).expect("budget"),
            &AtomicBool::new(false),
        );

        assert!(discovery.watermarks().contains_key(&source_id));
        assert!(discovery.purged_sources().is_empty());
    }
}
