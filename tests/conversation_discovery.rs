use std::collections::HashMap;

use herdr_context::conversations::Conversation;
use herdr_context::conversations::discovery::{
    discover_conversations, discover_sources, probe_sources,
};
use herdr_context::conversations::sources::{
    ConversationCandidate, ConversationSource, ConversationSourceError,
    ConversationSourceErrorKind, DiscoveryBatch, DiscoveryLimit, MetadataBudget,
    ProjectAssociationEvidence, SourceId, SourceRegistry, SourceWatermark, StorageProbe,
};
use herdr_context::project::ProjectIdentity;
use tempfile::TempDir;

#[derive(Clone, Copy)]
enum Behavior {
    Healthy,
    Failing,
}

struct FakeSource {
    id: SourceId,
    behavior: Behavior,
    expected_watermark: Option<&'static str>,
    has_more: bool,
}

impl FakeSource {
    fn healthy(id: &str, expected_watermark: Option<&'static str>) -> Self {
        Self {
            id: SourceId::new(id).expect("valid source id"),
            behavior: Behavior::Healthy,
            expected_watermark,
            has_more: false,
        }
    }

    fn failing(id: &str) -> Self {
        Self {
            id: SourceId::new(id).expect("valid source id"),
            behavior: Behavior::Failing,
            expected_watermark: None,
            has_more: false,
        }
    }
    const fn with_more(mut self) -> Self {
        self.has_more = true;
        self
    }

    fn failure(&self, operation: &str) -> ConversationSourceError {
        ConversationSourceError::new(
            self.id.clone(),
            ConversationSourceErrorKind::Io,
            format!("{operation} failed"),
        )
    }
}

impl ConversationSource for FakeSource {
    fn source_id(&self) -> &SourceId {
        &self.id
    }

    fn probe(&self) -> Result<StorageProbe, ConversationSourceError> {
        match self.behavior {
            Behavior::Healthy => Ok(StorageProbe::Available),
            Behavior::Failing => Err(self.failure("probe")),
        }
    }

    fn discover_raw(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        _limit: DiscoveryLimit,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        if matches!(self.behavior, Behavior::Failing) {
            return Err(self.failure("discovery"));
        }
        if after.map(SourceWatermark::token) != self.expected_watermark {
            return Err(ConversationSourceError::new(
                self.id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "unexpected watermark",
            ));
        }
        let candidate = ConversationCandidate::new(
            self.id.clone(),
            project.clone(),
            format!("{}-session", self.id.as_str()),
            None,
            None,
            None,
            None,
        )?;
        DiscoveryBatch::new(
            &self.id,
            project,
            vec![candidate],
            None,
            Vec::new(),
            self.has_more,
            Vec::new(),
        )
    }

    fn extract_metadata_raw(
        &self,
        _candidate: &ConversationCandidate,
        _budget: MetadataBudget,
    ) -> Result<Conversation, ConversationSourceError> {
        Err(self.failure("metadata"))
    }

    fn project_evidence_raw(
        &self,
        _candidate: &ConversationCandidate,
        _project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        Err(self.failure("evidence"))
    }
}

#[test]
fn registry_sorts_sources_and_rejects_duplicate_ids() {
    let registry = SourceRegistry::new(vec![
        Box::new(FakeSource::healthy("source-z", None)),
        Box::new(FakeSource::healthy("source-a", None)),
        Box::new(FakeSource::healthy("source-m", None)),
    ])
    .expect("unique source ids");

    let ids: Vec<_> = registry
        .iter()
        .map(|source| source.source_id().as_str())
        .collect();
    assert_eq!(ids, ["source-a", "source-m", "source-z"]);

    let error = SourceRegistry::new(vec![
        Box::new(FakeSource::healthy("duplicate", None)),
        Box::new(FakeSource::failing("duplicate")),
    ])
    .expect_err("duplicate ids must fail");
    assert_eq!(error.source_id().as_str(), "duplicate");
}

#[test]
fn probe_and_discovery_keep_each_source_outcome_independent()
-> Result<(), Box<dyn std::error::Error>> {
    let registry = SourceRegistry::new(vec![
        Box::new(FakeSource::healthy("source-z", Some("cursor-0"))),
        Box::new(FakeSource::failing("source-a")),
    ])?;
    let project_dir = TempDir::new()?;
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())?;

    let probes = probe_sources(&registry);
    assert_eq!(probes.len(), 2);
    assert_eq!(probes[0].source_id().as_str(), "source-a");
    assert_eq!(
        probes[0].result().expect_err("failing probe").kind(),
        ConversationSourceErrorKind::Io
    );
    assert_eq!(
        probes[1].result().expect("healthy probe"),
        &StorageProbe::Available
    );

    let mut watermarks = HashMap::new();
    let healthy_id = SourceId::new("source-z")?;
    watermarks.insert(
        healthy_id.clone(),
        SourceWatermark::new(healthy_id, "cursor-0")?,
    );
    let discoveries = discover_sources(
        &registry,
        &project,
        &watermarks,
        DiscoveryLimit::new(4).expect("non-zero limit"),
    );

    assert_eq!(discoveries.len(), 2);
    assert_eq!(discoveries[0].source_id().as_str(), "source-a");
    assert_eq!(
        discoveries[0]
            .result()
            .expect_err("failing discovery")
            .kind(),
        ConversationSourceErrorKind::Io
    );
    let healthy = discoveries[1].result().expect("healthy discovery");
    assert_eq!(healthy.candidates().len(), 1);
    assert_eq!(
        healthy.candidates()[0].source_id(),
        &SourceId::new("source-z")?
    );
    Ok(())
}

#[test]
fn failed_candidate_extraction_does_not_request_the_same_page_again() {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("canonical project");
    let registry = SourceRegistry::new(vec![Box::new(
        FakeSource::healthy("source-a", None).with_more(),
    )])
    .expect("registry");
    let discovery = discover_conversations(
        &registry,
        &project,
        &HashMap::new(),
        DiscoveryLimit::new(1).expect("limit"),
        MetadataBudget::new(1).expect("budget"),
    );
    assert!(!discovery.has_more());
    assert!(discovery.conversations().is_empty());
    assert_eq!(discovery.errors().len(), 1);
}
