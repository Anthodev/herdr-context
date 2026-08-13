mod support;

use std::time::SystemTime;

use herdr_context::conversations::sources::{
    ConversationCandidate, ConversationSource, ConversationSourceError,
    ConversationSourceErrorKind, DiscoveryBatch, DiscoveryLimit, MetadataBudget,
    ProjectAssociationEvidence, ProjectEvidenceKind, SourceId, SourceWatermark, StorageProbe,
};
use herdr_context::conversations::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    ResumeReference, SessionReference, ToolIdentity,
};
use herdr_context::project::{CanonicalPath, ProjectIdentity};
use support::conversation_source::{
    SourceConformanceCase, assert_discovery_failure_is_scoped, assert_source_conforms,
};
use tempfile::TempDir;

struct FixtureSource {
    id: SourceId,
    malformed: bool,
}

impl FixtureSource {
    fn new(id: &str, malformed: bool) -> Self {
        Self {
            id: SourceId::new(id).expect("valid source id"),
            malformed,
        }
    }

    fn error(&self, kind: ConversationSourceErrorKind, message: &str) -> ConversationSourceError {
        ConversationSourceError::new(self.id.clone(), kind, message)
    }
}

impl ConversationSource for FixtureSource {
    fn source_id(&self) -> &SourceId {
        &self.id
    }

    fn probe(&self) -> Result<StorageProbe, ConversationSourceError> {
        Ok(StorageProbe::Available)
    }

    fn discover_raw(
        &self,
        project: &ProjectIdentity,
        after: Option<&SourceWatermark>,
        _limit: DiscoveryLimit,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        if self.malformed {
            return Err(self.error(
                ConversationSourceErrorKind::MalformedData,
                "malformed fixture record",
            ));
        }
        let watermark = SourceWatermark::new(self.id.clone(), "fixture-offset-2")?;
        let candidates = if after.is_some() {
            Vec::new()
        } else {
            vec![ConversationCandidate::new(
                self.id.clone(),
                project.clone(),
                "fixture-session",
                None,
                Some(256),
                Some(SystemTime::UNIX_EPOCH),
                Some("fixture-fingerprint".to_owned()),
            )?]
        };
        DiscoveryBatch::new(&self.id, project, candidates, Some(watermark), Vec::new())
    }

    fn extract_metadata_raw(
        &self,
        candidate: &ConversationCandidate,
        budget: MetadataBudget,
    ) -> Result<Conversation, ConversationSourceError> {
        if budget.max_bytes() < 128 {
            return Err(self.error(
                ConversationSourceErrorKind::InvalidData,
                "metadata budget is too small",
            ));
        }
        let provenance = ConversationProvenance::new(
            self.id.clone(),
            ProvenanceKind::ExternalLocal,
            candidate.source_path().map(ToOwned::to_owned),
        );
        Conversation::new(
            ToolIdentity::new("fixture-tool").map_err(|error| {
                self.error(ConversationSourceErrorKind::InvalidData, &error.to_string())
            })?,
            SessionReference::new("fixture-tool", candidate.source_reference()).map_err(
                |error| self.error(ConversationSourceErrorKind::InvalidData, &error.to_string()),
            )?,
            candidate.project_identity().clone(),
            Some("Sanitized fixture".to_owned()),
            Some(SystemTime::UNIX_EPOCH),
            SystemTime::UNIX_EPOCH,
            ConversationState::Archived,
            vec![provenance],
            ResumeCapability::Supported(ResumeReference::new("fixture-session").map_err(
                |error| self.error(ConversationSourceErrorKind::InvalidData, &error.to_string()),
            )?),
        )
        .map_err(|error| self.error(ConversationSourceErrorKind::InvalidData, &error.to_string()))
    }

    fn project_evidence_raw(
        &self,
        _candidate: &ConversationCandidate,
        project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        let path = CanonicalPath::new(project.root().to_path_buf()).map_err(|error| {
            self.error(ConversationSourceErrorKind::InvalidData, &error.to_string())
        })?;
        Ok(vec![ProjectAssociationEvidence::new(
            ProjectEvidenceKind::CanonicalWorkingDirectory,
            path,
            Some("sanitized fixture cwd".to_owned()),
        )])
    }
}

#[test]
fn reusable_harness_covers_the_complete_source_contract() -> Result<(), Box<dyn std::error::Error>>
{
    let project_dir = TempDir::new()?;
    let foreign_dir = TempDir::new()?;
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())?;
    let foreign_project = ProjectIdentity::from_canonical_root(foreign_dir.path().to_path_buf())?;
    let source = FixtureSource::new("fixture-source", false);
    let expected_resume = ResumeCapability::Supported(ResumeReference::new("fixture-session")?);

    assert_source_conforms(SourceConformanceCase {
        source: &source,
        project: &project,
        foreign_project: &foreign_project,
        metadata_budget: MetadataBudget::new(4096).expect("non-zero metadata budget"),
        expected_resume: &expected_resume,
        expected_evidence_kind: ProjectEvidenceKind::CanonicalWorkingDirectory,
        small_budget_error: ConversationSourceErrorKind::InvalidData,
    });
    Ok(())
}

#[test]
fn reusable_harness_checks_source_scoped_failures() -> Result<(), Box<dyn std::error::Error>> {
    let project_dir = TempDir::new()?;
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())?;
    let source = FixtureSource::new("malformed-fixture", true);

    assert_discovery_failure_is_scoped(
        &source,
        &project,
        ConversationSourceErrorKind::MalformedData,
    );
    Ok(())
}
