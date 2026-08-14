use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use herdr_context::conversations::index::{ConversationIndex, IndexStatus};
use herdr_context::conversations::sources::{
    CodexCliSource, DiscoveryLimit, GenericJsonlSource, MetadataBudget, ProjectLocalLocation,
    SourceRegistry,
};
use herdr_context::project::ProjectIdentity;
use tempfile::TempDir;

const SESSION_COUNT: usize = 2_048;
const PAGE_SIZE: usize = 64;

fn setup() -> (TempDir, ProjectIdentity, TempDir, PathBuf, TempDir) {
    let project_dir = TempDir::new().expect("project");
    let project = ProjectIdentity::from_canonical_root(project_dir.path().to_path_buf())
        .expect("canonical project");
    let home = TempDir::new().expect("home");
    let codex_root = home.path().join(".codex/sessions");
    let state = TempDir::new().expect("state parent");
    (project_dir, project, home, codex_root, state)
}

fn uuid(index: usize) -> String {
    format!("019b7c3b-{:04x}-7000-8001-{:012x}", index, index)
}

fn install_codex_sessions(root: &Path, project: &ProjectIdentity, count: usize) -> Vec<PathBuf> {
    let directory = root.join("2026/01/02");
    fs::create_dir_all(&directory).expect("Codex date directory");
    let cwd = project.root().to_str().expect("UTF-8 test project");
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let minute = index / 50;
        let second = index % 50;
        let id = uuid(index);
        let start = format!("2026-01-02T01:{minute:02}:{second:02}.000Z");
        let written = format!("2026-01-02T01:{minute:02}:{:02}.000Z", second + 1);
        let path = directory.join(format!(
            "rollout-2026-01-02T03-{minute:02}-{second:02}-{id}.jsonl"
        ));
        let record = serde_json::json!({
            "timestamp": written,
            "ordinal": 0,
            "type": "session_meta",
            "payload": {
                "id": id,
                "session_id": id,
                "timestamp": start,
                "cwd": cwd,
                "originator": "codex-tui",
                "cli_version": "0.147.0",
                "history_mode": "paginated",
                "source": "cli",
                "base_instructions": { "text": "private prompt sentinel" }
            }
        });
        fs::write(&path, format!("{record}\n")).expect("Codex session");
        paths.push(path);
    }
    paths
}

fn registry(project: &ProjectIdentity, codex_root: &Path) -> SourceRegistry {
    SourceRegistry::new(vec![Box::new(
        CodexCliSource::new(project.clone(), codex_root.to_path_buf()).expect("Codex source"),
    )])
    .expect("registry")
}

fn refresh_all(index: &mut ConversationIndex, registry: &SourceRegistry) -> usize {
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= 32, "bounded scan must converge");
        let refresh = index
            .refresh_page(
                registry,
                DiscoveryLimit::new(PAGE_SIZE).expect("limit"),
                MetadataBudget::new(512 * 1024).expect("budget"),
            )
            .expect("index refresh");
        assert!(refresh.errors().is_empty(), "{:?}", refresh.errors());
        if !refresh.has_more() {
            break;
        }
    }
    pages
}

fn cache_files(state_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for project_entry in fs::read_dir(state_dir.join("conversations")).expect("index root") {
        let project_dir = project_entry.expect("project entry").path();
        for entry in fs::read_dir(project_dir).expect("project index") {
            let path = entry.expect("cache entry").path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn thousands_of_sessions_are_published_recent_first_in_bounded_pages() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    install_codex_sessions(&codex_root, &project, SESSION_COUNT);
    let registry = registry(&project, &codex_root);
    let state_dir = state.path().join("plugin-state");
    let mut index = ConversationIndex::open(&state_dir, project).expect("index");
    assert_eq!(index.status(), IndexStatus::RebuiltMissing);

    let first = index
        .refresh_page(
            &registry,
            DiscoveryLimit::new(PAGE_SIZE).expect("limit"),
            MetadataBudget::new(512 * 1024).expect("budget"),
        )
        .expect("first page");
    assert!(first.has_more());
    assert_eq!(first.added_or_updated(), PAGE_SIZE);
    let recent = index.page(0, PAGE_SIZE);
    assert_eq!(recent.conversations().len(), PAGE_SIZE);
    assert!(recent.has_more());
    assert!(
        recent
            .conversations()
            .windows(2)
            .all(|pair| { pair[0].updated_at() >= pair[1].updated_at() })
    );

    let remaining_pages = refresh_all(&mut index, &registry);
    assert!(remaining_pages >= 15);
    assert_eq!(index.len(), SESSION_COUNT);
    let older = index.page(PAGE_SIZE, PAGE_SIZE);
    assert_eq!(older.conversations().len(), PAGE_SIZE);
    assert!(
        recent.conversations().last().expect("recent").updated_at()
            >= older.conversations().first().expect("older").updated_at()
    );
}

#[test]
fn deleted_sessions_are_removed_after_a_successful_source_refresh() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    let paths = install_codex_sessions(&codex_root, &project, 2);
    let registry = registry(&project, &codex_root);
    let mut index =
        ConversationIndex::open(state.path().join("plugin-state"), project).expect("index");
    refresh_all(&mut index, &registry);
    assert_eq!(index.len(), 2);

    fs::remove_file(&paths[0]).expect("delete indexed session");
    refresh_all(&mut index, &registry);
    assert_eq!(index.len(), 1);
    assert_eq!(
        index.page(0, 10).conversations()[0]
            .session_reference()
            .id(),
        uuid(1)
    );
}

#[test]
fn completed_watermarks_revisit_only_the_appended_session() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    let paths = install_codex_sessions(&codex_root, &project, 4);
    let registry = registry(&project, &codex_root);
    let mut index =
        ConversationIndex::open(state.path().join("plugin-state"), project).expect("index");
    refresh_all(&mut index, &registry);
    let before = index.page(0, 4).conversations()[0].updated_at();

    let newest = paths.last().expect("newest path");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(newest)
        .expect("append session");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2026-01-02T01:20:00.000Z",
            "ordinal": 1,
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "private append sentinel"}
        })
    )
    .expect("append record");

    let refresh = index
        .refresh_page(
            &registry,
            DiscoveryLimit::new(PAGE_SIZE).expect("limit"),
            MetadataBudget::new(512 * 1024).expect("budget"),
        )
        .expect("incremental refresh");
    assert!(!refresh.has_more());
    assert_eq!(refresh.added_or_updated(), 1);
    assert_eq!(index.len(), 4);
    assert!(index.page(0, 4).conversations()[0].updated_at() > before);
}

#[test]
fn cache_is_private_atomically_replaced_and_contains_only_allowlisted_metadata() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    install_codex_sessions(&codex_root, &project, 2);
    let registry = registry(&project, &codex_root);
    let state_dir = state.path().join("plugin-state");
    let mut index = ConversationIndex::open(&state_dir, project).expect("index");
    refresh_all(&mut index, &registry);
    let first_files = cache_files(&state_dir);
    assert_eq!(first_files.len(), 1);

    refresh_all(&mut index, &registry);
    let second_files = cache_files(&state_dir);
    assert_eq!(second_files.len(), 1, "obsolete generations are removed");
    assert_ne!(
        first_files, second_files,
        "replacement publishes a new generation"
    );
    let current = second_files[0]
        .parent()
        .expect("project cache directory")
        .join("current");
    assert_eq!(
        fs::read_to_string(&current).expect("current pointer"),
        second_files[0]
            .file_name()
            .and_then(|value| value.to_str())
            .expect("cache filename")
    );
    let raw = fs::read_to_string(&second_files[0]).expect("cache");
    assert!(!raw.contains("private prompt sentinel"));
    assert!(!raw.contains("private append sentinel"));
    let json: serde_json::Value = serde_json::from_str(&raw).expect("cache JSON");
    let keys = json
        .as_object()
        .expect("cache object")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from([
            "entries",
            "generation",
            "project_root",
            "schema_version",
            "watermarks",
        ])
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&state_dir)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&second_files[0])
                .expect("cache metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(current)
                .expect("pointer metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn corrupt_and_incompatible_caches_rebuild_without_blocking_discovery() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    install_codex_sessions(&codex_root, &project, 1);
    let registry = registry(&project, &codex_root);
    let state_dir = state.path().join("plugin-state");
    let mut index = ConversationIndex::open(&state_dir, project.clone()).expect("index");
    refresh_all(&mut index, &registry);
    let cache = cache_files(&state_dir).remove(0);
    fs::write(&cache, b"{not-json").expect("corrupt cache");

    let mut rebuilt = ConversationIndex::open(&state_dir, project.clone()).expect("rebuild");
    assert_eq!(rebuilt.status(), IndexStatus::RebuiltCorrupt);
    assert!(rebuilt.is_empty());
    refresh_all(&mut rebuilt, &registry);
    assert_eq!(rebuilt.len(), 1);

    let cache = cache_files(&state_dir).remove(0);
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache).expect("cache")).expect("cache JSON");
    json["schema_version"] = serde_json::json!(999);
    fs::write(&cache, serde_json::to_vec(&json).expect("encoded cache"))
        .expect("incompatible cache");
    let incompatible = ConversationIndex::open(&state_dir, project).expect("rebuild");
    assert_eq!(incompatible.status(), IndexStatus::RebuiltIncompatible);
    assert!(incompatible.is_empty());
}

#[test]
fn cache_selection_rejects_filename_lookalikes_and_generation_mismatches() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    install_codex_sessions(&codex_root, &project, 1);
    let registry = registry(&project, &codex_root);
    let state_dir = state.path().join("plugin-state");
    let mut index = ConversationIndex::open(&state_dir, project.clone()).expect("index");
    refresh_all(&mut index, &registry);
    let cache = cache_files(&state_dir).remove(0);
    let lookalike = cache
        .parent()
        .expect("project cache directory")
        .join("cache-99999999999999999999-ffffffff-ffffffffffffffff.json.bak");
    fs::write(&lookalike, b"{not-json").expect("lookalike cache");
    let temporary = cache
        .parent()
        .expect("project cache directory")
        .join(".cache-00000000000000000001-ffffffff-ffffffffffffffff.tmp");
    fs::write(&temporary, b"interrupted publication").expect("temporary cache");
    let loaded = ConversationIndex::open(&state_dir, project.clone()).expect("valid cache");
    assert_eq!(loaded.status(), IndexStatus::Loaded);
    assert_eq!(loaded.len(), 1);
    assert!(!temporary.exists(), "stale temporary is removed");
    let cache_directory = cache.parent().expect("project cache directory");
    let garbage = (0..65)
        .map(|index| cache_directory.join(format!("garbage-{index:03}")))
        .collect::<Vec<_>>();
    for path in &garbage {
        fs::write(path, b"unrelated").expect("garbage entry");
    }
    ConversationIndex::open(&state_dir, project.clone())
        .expect("bounded garbage inventory does not block startup");
    for path in garbage {
        fs::remove_file(path).expect("remove garbage entry");
    }

    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache).expect("cache")).expect("cache JSON");
    let generation = json["generation"].as_u64().expect("generation");
    json["generation"] = serde_json::json!(generation + 1);
    fs::write(&cache, serde_json::to_vec(&json).expect("encoded cache")).expect("mismatched cache");
    let rebuilt = ConversationIndex::open(&state_dir, project).expect("rebuild");
    assert_eq!(rebuilt.status(), IndexStatus::RebuiltCorrupt);
    assert!(rebuilt.is_empty());
}

#[test]
fn malformed_source_watermark_is_purged_and_rescanned() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    install_codex_sessions(&codex_root, &project, 1);
    let registry = registry(&project, &codex_root);
    let state_dir = state.path().join("plugin-state");
    let mut index = ConversationIndex::open(&state_dir, project.clone()).expect("index");
    refresh_all(&mut index, &registry);
    let cache = cache_files(&state_dir).remove(0);
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache).expect("cache")).expect("cache JSON");
    json["watermarks"][0]["token"] = serde_json::json!("malformed");
    fs::write(&cache, serde_json::to_vec(&json).expect("encoded cache"))
        .expect("malformed watermark cache");

    let mut loaded = ConversationIndex::open(&state_dir, project).expect("load cache");
    let reset = loaded
        .refresh_page(
            &registry,
            DiscoveryLimit::new(PAGE_SIZE).expect("limit"),
            MetadataBudget::new(512 * 1024).expect("budget"),
        )
        .expect("reset malformed source");
    assert!(reset.has_more());
    assert!(!reset.errors().is_empty());
    assert!(loaded.is_empty());

    refresh_all(&mut loaded, &registry);
    assert_eq!(loaded.len(), 1);
}

#[test]
fn cancelled_refresh_does_not_publish_a_cache_generation() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    install_codex_sessions(&codex_root, &project, 4);
    let registry = registry(&project, &codex_root);
    let state_dir = state.path().join("plugin-state");
    let mut index = ConversationIndex::open(&state_dir, project).expect("index");
    let cancelled = AtomicBool::new(true);
    let refresh = index
        .refresh_page_cancellable(
            &registry,
            DiscoveryLimit::new(PAGE_SIZE).expect("limit"),
            MetadataBudget::new(512 * 1024).expect("budget"),
            &cancelled,
        )
        .expect("cancelled refresh");
    assert_eq!(refresh.added_or_updated(), 0);
    assert!(refresh.is_cancelled());
    assert!(index.is_empty());
    assert!(cache_files(&state_dir).is_empty());
}

#[test]
fn deleted_project_local_sessions_are_removed_from_the_cache() {
    let (project_dir, project, _home, _codex_root, state) = setup();
    let directory = project_dir.path().join(".herdr/conversations");
    fs::create_dir_all(&directory).expect("conversation directory");
    let path = directory.join("session.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n",
            serde_json::json!({
                "session_id": "project-local-session",
                "cwd": project.root(),
                "timestamp": "2026-01-02T03:04:05Z",
                "role": "user",
                "message": "private project-local fixture",
            })
        ),
    )
    .expect("conversation fixture");
    let source = GenericJsonlSource::new(
        project.clone(),
        vec![
            ProjectLocalLocation::new(".herdr/conversations").expect("location"),
            ProjectLocalLocation::new(".herdr/conversations.jsonl").expect("location"),
            ProjectLocalLocation::new(".herdr/conversations.json").expect("location"),
        ],
    )
    .expect("generic source");
    let registry = SourceRegistry::new(vec![Box::new(source)]).expect("registry");
    let mut index =
        ConversationIndex::open(state.path().join("plugin-state"), project.clone()).expect("index");
    refresh_all(&mut index, &registry);
    assert_eq!(index.len(), 1);

    fs::remove_file(path).expect("delete conversation");
    refresh_all(&mut index, &registry);
    assert!(index.is_empty());

    fs::write(
        directory.join("replacement.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "session_id": "replacement-session",
                "cwd": project.root(),
                "timestamp": "2026-01-02T03:04:06Z",
                "role": "user",
                "message": "private replacement fixture",
            })
        ),
    )
    .expect("replacement fixture");
    refresh_all(&mut index, &registry);
    assert_eq!(index.len(), 1);
    fs::remove_dir_all(directory).expect("remove registered store");
    refresh_all(&mut index, &registry);
    assert!(index.is_empty());
}

#[test]
fn unavailable_known_store_purges_its_cached_sessions() {
    let (_project_dir, project, _home, codex_root, state) = setup();
    install_codex_sessions(&codex_root, &project, 1);
    let registry = registry(&project, &codex_root);
    let mut index =
        ConversationIndex::open(state.path().join("plugin-state"), project).expect("index");
    refresh_all(&mut index, &registry);
    assert_eq!(index.len(), 1);

    fs::remove_dir_all(codex_root).expect("remove Codex store");
    refresh_all(&mut index, &registry);
    assert!(index.is_empty());
}

#[cfg(unix)]
#[test]
fn non_utf8_project_paths_round_trip_through_private_cache() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = TempDir::new().expect("root");
    let project_path = root
        .path()
        .join(OsString::from_vec(b"project-\xff".to_vec()));
    fs::create_dir(&project_path).expect("non-UTF-8 project");
    let project =
        ProjectIdentity::from_canonical_root(project_path).expect("canonical project identity");
    let source = GenericJsonlSource::new(
        project.clone(),
        vec![ProjectLocalLocation::new(".herdr/conversations").expect("location")],
    )
    .expect("generic source");
    let registry = SourceRegistry::new(vec![Box::new(source)]).expect("registry");
    let state = TempDir::new().expect("state");
    let mut index =
        ConversationIndex::open(state.path(), project.clone()).expect("non-UTF-8 index");
    refresh_all(&mut index, &registry);

    let loaded = ConversationIndex::open(state.path(), project).expect("load non-UTF-8 cache");
    assert!(loaded.is_empty());
    assert_eq!(loaded.status(), IndexStatus::Loaded);
}
