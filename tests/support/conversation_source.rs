use herdr_context::conversations::ResumeCapability;
use herdr_context::conversations::sources::{
    ConversationCandidate, ConversationSource, ConversationSourceErrorKind, DiscoveryLimit,
    MetadataBudget, ProjectEvidenceKind, SourceId, SourceWatermark, StorageProbe,
};
use herdr_context::project::ProjectIdentity;

pub struct SourceConformanceCase<'a> {
    pub source: &'a dyn ConversationSource,
    pub project: &'a ProjectIdentity,
    pub foreign_project: &'a ProjectIdentity,
    pub metadata_budget: MetadataBudget,
    pub expected_resume: &'a ResumeCapability,
    pub expected_evidence_kind: ProjectEvidenceKind,
    pub small_budget_error: ConversationSourceErrorKind,
}

/// Exercises the behavior every filesystem-backed source adapter must expose.
///
/// Adapter integration tests can reuse this against a sanitized fixture store;
/// the fixture must contain one discoverable conversation and enough metadata
/// that a one-byte extraction budget is insufficient.
pub fn assert_source_conforms(case: SourceConformanceCase<'_>) {
    let source_id = case.source.source_id();
    assert_eq!(
        case.source.probe().expect("fixture store must be probed"),
        StorageProbe::Available
    );

    let initial = case
        .source
        .discover(
            case.project,
            None,
            DiscoveryLimit::new(1).expect("non-zero discovery limit"),
        )
        .expect("fixture discovery must succeed");
    assert_eq!(initial.candidates().len(), 1);
    let candidate = &initial.candidates()[0];
    assert_eq!(candidate.source_id(), source_id);
    assert_eq!(candidate.project_identity(), case.project);

    let watermark = initial
        .next_watermark()
        .expect("fixture discovery must publish a watermark");
    assert_eq!(watermark.source_id(), source_id);
    let resumed = case
        .source
        .discover(
            case.project,
            Some(watermark),
            DiscoveryLimit::new(1).expect("non-zero discovery limit"),
        )
        .expect("source-owned watermark must be accepted");
    assert!(
        resumed.candidates().is_empty(),
        "unchanged fixture must not replay candidates after its watermark"
    );
    assert!(
        resumed.errors().is_empty(),
        "unchanged fixture must not add entry errors after its watermark"
    );
    if let Some(next_watermark) = resumed.next_watermark() {
        assert_eq!(next_watermark.source_id(), source_id);
    }

    let foreign_watermark = SourceWatermark::new(
        SourceId::new(format!("{}-foreign", source_id.as_str())).expect("valid foreign source id"),
        "foreign-cursor",
    )
    .expect("valid foreign watermark");
    let watermark_error = case
        .source
        .discover(
            case.project,
            Some(&foreign_watermark),
            DiscoveryLimit::new(1).expect("non-zero discovery limit"),
        )
        .expect_err("foreign watermark must fail");
    assert_eq!(
        watermark_error.kind(),
        ConversationSourceErrorKind::SourceMismatch
    );
    assert_eq!(watermark_error.source_id(), source_id);

    let conversation = case
        .source
        .extract_metadata(candidate, case.metadata_budget)
        .expect("bounded fixture metadata must be extracted");
    assert_eq!(conversation.project_identity(), case.project);
    assert_eq!(conversation.resume_capability(), case.expected_resume);

    let budget_error = case
        .source
        .extract_metadata(
            candidate,
            MetadataBudget::new(1).expect("non-zero metadata budget"),
        )
        .expect_err("one-byte metadata budget must be enforced");
    assert_eq!(budget_error.kind(), case.small_budget_error);
    assert_eq!(budget_error.source_id(), source_id);

    let evidence = case
        .source
        .project_evidence(candidate, case.project)
        .expect("fixture project evidence must be extracted");
    assert!(evidence.iter().any(|item| {
        item.kind() == case.expected_evidence_kind && item.canonical_path() == case.project.root()
    }));

    let project_error = case
        .source
        .project_evidence(candidate, case.foreign_project)
        .expect_err("candidate must not produce evidence for another project");
    assert_eq!(
        project_error.kind(),
        ConversationSourceErrorKind::SourceMismatch
    );
    assert_eq!(project_error.source_id(), source_id);

    let foreign_candidate = ConversationCandidate::new(
        SourceId::new(format!("{}-foreign", source_id.as_str())).expect("valid foreign source id"),
        case.project.clone(),
        "foreign-candidate",
        None,
        None,
        None,
        None,
    )
    .expect("valid foreign candidate");
    let candidate_error = case
        .source
        .extract_metadata(&foreign_candidate, case.metadata_budget)
        .expect_err("foreign candidate must fail");
    assert_eq!(
        candidate_error.kind(),
        ConversationSourceErrorKind::SourceMismatch
    );
    assert_eq!(candidate_error.source_id(), source_id);
}

pub fn assert_discovery_failure_is_scoped(
    source: &dyn ConversationSource,
    project: &ProjectIdentity,
    expected_kind: ConversationSourceErrorKind,
) {
    let error = source
        .discover(
            project,
            None,
            DiscoveryLimit::new(1).expect("non-zero discovery limit"),
        )
        .expect_err("malformed fixture must fail discovery");
    assert_eq!(error.source_id(), source.source_id());
    assert_eq!(error.kind(), expected_kind);
}
