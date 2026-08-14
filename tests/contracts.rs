use std::path::{Path, PathBuf};
use std::time::SystemTime;

use herdr_context::conversations::sources::{
    ConversationCandidate, ConversationSource, ConversationSourceError,
    ConversationSourceErrorKind, DiscoveryBatch, DiscoveryLimit, MetadataBudget,
    ProjectAssociationEvidence, SourceId, SourceWatermark, StorageProbe,
};
use herdr_context::conversations::{
    Conversation, ConversationProvenance, ConversationState, ProvenanceKind, ResumeCapability,
    SessionReference, ToolIdentity,
};
use herdr_context::host::{
    DockWidth, HostClient, HostError, HostPane, OpenDockRequest, PaneId, TabId,
};
use herdr_context::project::{ProjectIdentity, ProjectResolutionError};
use herdr_context::vcs::{
    VcsBackendMetadata, VcsError, VcsService, VcsStatusSnapshot, VcsWorkspace,
};
use tempfile::TempDir;

struct FakeHost {
    pane: HostPane,
}

impl HostClient for FakeHost {
    fn pane(&self, pane_id: &PaneId) -> Result<Option<HostPane>, HostError> {
        Ok((self.pane.pane_id() == pane_id).then(|| self.pane.clone()))
    }

    fn panes_in_tab(
        &self,
        _workspace_id: &herdr_context::host::WorkspaceId,
        _tab_id: &TabId,
    ) -> Result<Vec<HostPane>, HostError> {
        Ok(vec![self.pane.clone()])
    }

    fn live_sessions(&self) -> Result<Vec<herdr_context::host::HostAgentSession>, HostError> {
        Ok(Vec::new())
    }

    fn verified_dock_identity(
        &mut self,
        pane: &HostPane,
    ) -> Result<Option<herdr_context::host::DockIdentity>, HostError> {
        Ok(pane.dock_identity())
    }

    fn open_dock(&mut self, _request: &OpenDockRequest) -> Result<PaneId, HostError> {
        Ok(self.pane.pane_id().clone())
    }

    fn focus_pane(&mut self, _pane_id: &PaneId) -> Result<(), HostError> {
        Ok(())
    }

    fn close_pane(&mut self, _pane_id: &PaneId) -> Result<(), HostError> {
        Ok(())
    }

    fn move_to_right_edge(&mut self, _pane_id: &PaneId) -> Result<(), HostError> {
        Ok(())
    }

    fn resize_pane(&mut self, _pane_id: &PaneId, _width: DockWidth) -> Result<(), HostError> {
        Ok(())
    }
}

fn inspect_host(client: &impl HostClient, pane_id: &PaneId) -> Result<PathBuf, HostError> {
    client
        .pane(pane_id)?
        .and_then(|pane| pane.cwd().map(Path::to_path_buf))
        .ok_or_else(|| HostError::new(herdr_context::host::HostErrorKind::NotFound, "missing pane"))
}

struct FakeVcs;

impl VcsService for FakeVcs {
    fn detect(&self, start: &Path) -> Result<Option<VcsWorkspace>, VcsError> {
        Ok(Some(VcsWorkspace::new(
            start.to_path_buf(),
            VcsBackendMetadata::new("future-vcs", "Future VCS", true)?,
        )?))
    }

    fn refresh_status(&mut self, _workspace: &VcsWorkspace) -> Result<VcsStatusSnapshot, VcsError> {
        Ok(VcsStatusSnapshot::new(Vec::new(), false))
    }
}

fn refresh_vcs(service: &mut impl VcsService, root: &Path) -> Result<bool, VcsError> {
    let workspace = service
        .detect(root)?
        .ok_or_else(|| VcsError::new(herdr_context::vcs::VcsErrorKind::Unavailable, "no VCS"))?;
    service
        .refresh_status(&workspace)
        .map(|snapshot| snapshot.is_stale())
}

struct FakeSource {
    id: SourceId,
}

impl ConversationSource for FakeSource {
    fn source_id(&self) -> &SourceId {
        &self.id
    }

    fn probe(&self) -> Result<StorageProbe, ConversationSourceError> {
        Ok(StorageProbe::Available)
    }

    fn discover_raw(
        &self,
        project: &ProjectIdentity,
        _after: Option<&SourceWatermark>,
        _limit: DiscoveryLimit,
    ) -> Result<DiscoveryBatch, ConversationSourceError> {
        let candidate = ConversationCandidate::new(
            self.id.clone(),
            project.clone(),
            "session-1",
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
            false,
            Vec::new(),
        )
    }

    fn extract_metadata_raw(
        &self,
        candidate: &ConversationCandidate,
        _budget: MetadataBudget,
    ) -> Result<Conversation, ConversationSourceError> {
        let provenance =
            ConversationProvenance::new(self.id.clone(), ProvenanceKind::ProjectLocal, None);
        Conversation::new(
            ToolIdentity::new("future-tool").map_err(domain_error(&self.id))?,
            SessionReference::new("future-tool", "session-1").map_err(domain_error(&self.id))?,
            candidate.project_identity().clone(),
            Some("Contract test".to_owned()),
            None,
            None,
            SystemTime::UNIX_EPOCH,
            ConversationState::Archived,
            vec![provenance],
            ResumeCapability::Unsupported,
        )
        .map_err(domain_error(&self.id))
    }

    fn project_evidence_raw(
        &self,
        _candidate: &ConversationCandidate,
        _project: &ProjectIdentity,
    ) -> Result<Vec<ProjectAssociationEvidence>, ConversationSourceError> {
        Ok(Vec::new())
    }
}

fn domain_error(
    source_id: &SourceId,
) -> impl FnOnce(herdr_context::conversations::ConversationError) -> ConversationSourceError + '_ {
    move |error| {
        ConversationSourceError::new(
            source_id.clone(),
            ConversationSourceErrorKind::InvalidData,
            error.to_string(),
        )
    }
}

fn first_conversation(
    source: &impl ConversationSource,
    project: &ProjectIdentity,
) -> Result<Conversation, ConversationSourceError> {
    let limit = DiscoveryLimit::new(1).expect("non-zero limit");
    let batch = source.discover(project, None, limit)?;
    let candidate = batch.candidates().first().ok_or_else(|| {
        ConversationSourceError::unavailable(source.source_id().clone(), "no conversations")
    })?;
    source.extract_metadata(
        candidate,
        MetadataBudget::new(4096).expect("non-zero metadata budget"),
    )
}

#[test]
fn consumers_use_normalized_contracts_only() -> Result<(), Box<dyn std::error::Error>> {
    let project = TempDir::new()?;
    let project_root = project.path().to_path_buf();
    let pane_id = PaneId::new("pane")?;
    let tab_id = TabId::new("tab")?;
    let host = FakeHost {
        pane: HostPane::new(
            pane_id.clone(),
            tab_id,
            Some(project_root.clone()),
            None,
            true,
        ),
    };
    assert_eq!(inspect_host(&host, &pane_id)?, project_root);

    let mut vcs = FakeVcs;
    assert!(!refresh_vcs(&mut vcs, project.path())?);

    let identity = ProjectIdentity::from_canonical_root(project.path().to_path_buf())?;
    let source = FakeSource {
        id: SourceId::new("future-source")?,
    };
    let conversation = first_conversation(&source, &identity)?;
    assert_eq!(conversation.tool().as_str(), "future-tool");
    Ok(())
}

#[test]
fn project_identity_rejects_relative_roots() {
    let error = ProjectIdentity::from_canonical_root(PathBuf::from("relative"));
    assert_eq!(
        error,
        Err(ProjectResolutionError::NonAbsoluteIdentity(PathBuf::from(
            "relative"
        )))
    );
}

#[test]
fn source_contract_rejects_foreign_candidate_and_watermark()
-> Result<(), Box<dyn std::error::Error>> {
    let project = TempDir::new()?;
    let identity = ProjectIdentity::from_canonical_root(project.path().to_path_buf())?;
    let source = FakeSource {
        id: SourceId::new("source-a")?,
    };
    let foreign_id = SourceId::new("source-b")?;
    let foreign_candidate = ConversationCandidate::new(
        foreign_id.clone(),
        identity.clone(),
        "session",
        None,
        None,
        None,
        None,
    )?;
    let foreign_watermark = SourceWatermark::new(foreign_id, "cursor")?;

    assert_eq!(
        source
            .extract_metadata(
                &foreign_candidate,
                MetadataBudget::new(64).expect("non-zero budget")
            )
            .expect_err("foreign candidate must fail")
            .kind(),
        ConversationSourceErrorKind::SourceMismatch
    );
    assert_eq!(
        source
            .discover(
                &identity,
                Some(&foreign_watermark),
                DiscoveryLimit::new(1).expect("non-zero limit")
            )
            .expect_err("foreign watermark must fail")
            .kind(),
        ConversationSourceErrorKind::SourceMismatch
    );
    let other_project = TempDir::new()?;
    let other_identity = ProjectIdentity::from_canonical_root(other_project.path().to_path_buf())?;
    let wrong_project_candidate = ConversationCandidate::new(
        source.id.clone(),
        other_identity,
        "session",
        None,
        None,
        None,
        None,
    )?;
    assert_eq!(
        DiscoveryBatch::new(
            &source.id,
            &identity,
            vec![wrong_project_candidate],
            None,
            Vec::new(),
            false,
            Vec::new()
        )
        .expect_err("foreign project candidate must fail")
        .kind(),
        ConversationSourceErrorKind::ProjectMismatch
    );
    Ok(())
}
