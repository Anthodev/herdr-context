use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::tree::{DirectoryLoader, FilesTree, TreeNodeKind};

pub const DIRECTORIES_PER_PAGE: usize = 64;
pub const MAX_INDEX_ENTRIES: usize = 50_000;
pub const MAX_EXAMINED_ENTRIES: usize = 200_000;
pub const MAX_QUERY_BYTES: usize = 256;

#[derive(Clone, Debug)]
pub struct IndexedSearchPath {
    pub kind: TreeNodeKind,
    pub normalized: String,
}

pub type SearchablePaths = Arc<BTreeMap<PathBuf, IndexedSearchPath>>;

#[derive(Debug)]
pub struct SearchPageRequest {
    pub index: FilesTree,
    pub searchable: SearchablePaths,
    pub directories: Vec<PathBuf>,
    pub remaining_entries: usize,
    pub remaining_examined: usize,
}

#[derive(Debug)]
pub struct PreparedSearchPage {
    pub index: FilesTree,
    pub searchable: SearchablePaths,
    pub discovered_directories: Vec<PathBuf>,
    pub scanned_entries: usize,
    pub examined_entries: usize,
    pub skipped_directories: usize,
    pub truncated: bool,
    pub cancelled: bool,
}

#[derive(Debug)]
pub struct SearchProjection {
    pub matches: BTreeSet<PathBuf>,
    pub expanded_context: BTreeSet<PathBuf>,
    pub rows: Vec<PathBuf>,
}

pub fn prepare_page(
    loader: &DirectoryLoader,
    request: SearchPageRequest,
    cancelled: &AtomicBool,
    page_cancelled: &AtomicBool,
) -> PreparedSearchPage {
    let SearchPageRequest {
        mut index,
        mut searchable,
        directories,
        remaining_entries,
        remaining_examined,
    } = request;
    let mut snapshots = Vec::with_capacity(directories.len());
    let mut scanned_directories = Vec::with_capacity(directories.len());
    let mut discovered_directories = Vec::new();
    let mut entries = Vec::new();
    let mut scanned_entries = 0_usize;
    let mut examined_entries = 0_usize;
    let mut skipped_directories = 0_usize;
    let mut truncated = false;
    let mut was_cancelled = false;

    for directory in directories {
        if is_cancelled(cancelled, page_cancelled) {
            was_cancelled = true;
            break;
        }
        let entry_budget = remaining_entries.saturating_sub(scanned_entries);
        let examined_budget = remaining_examined.saturating_sub(examined_entries);
        if entry_budget == 0 || examined_budget == 0 {
            truncated = true;
            break;
        }
        let (snapshot, examined, directory_truncated) = match loader.load_bounded(
            directory.clone(),
            entry_budget,
            examined_budget,
            cancelled,
            page_cancelled,
        ) {
            Ok(result) => result,
            Err(_) => {
                skipped_directories = skipped_directories.saturating_add(1);
                continue;
            }
        };
        if is_cancelled(cancelled, page_cancelled) {
            was_cancelled = true;
            break;
        }
        scanned_entries = scanned_entries.saturating_add(snapshot.nodes().len());
        examined_entries = examined_entries.saturating_add(examined);
        scanned_directories.push(directory);
        for node in snapshot.nodes() {
            if node.kind() == TreeNodeKind::Directory {
                discovered_directories.push(node.path().to_path_buf());
            } else {
                entries.push((
                    node.path().to_path_buf(),
                    IndexedSearchPath {
                        kind: node.kind(),
                        normalized: node.path().to_string_lossy().to_lowercase(),
                    },
                ));
            }
        }
        snapshots.push(snapshot);
        if directory_truncated {
            truncated = true;
            break;
        }
    }

    if !was_cancelled {
        let searchable = Arc::make_mut(&mut searchable);
        for directory in &scanned_directories {
            searchable.retain(|path, indexed| {
                indexed.kind == TreeNodeKind::Virtual || path.parent() != Some(directory.as_path())
            });
        }
        searchable.extend(entries);
        for snapshot in snapshots {
            index.apply_directory(snapshot);
        }
        searchable.retain(|path, _| index.node(path).is_some());
    }

    PreparedSearchPage {
        index,
        searchable,
        discovered_directories,
        scanned_entries,
        examined_entries,
        skipped_directories,
        truncated,
        cancelled: was_cancelled,
    }
}

pub fn project(
    index: &FilesTree,
    searchable: &BTreeMap<PathBuf, IndexedSearchPath>,
    query: &str,
    cancelled: &AtomicBool,
    projection_cancelled: &AtomicBool,
) -> Option<SearchProjection> {
    let normalized_query = query.to_lowercase();
    let mut matches = BTreeSet::new();
    for (path, indexed) in searchable {
        if is_cancelled(cancelled, projection_cancelled) {
            return None;
        }
        if indexed.normalized.contains(&normalized_query) {
            matches.insert(path.clone());
        }
    }
    let mut included = matches.clone();
    for path in &matches {
        if is_cancelled(cancelled, projection_cancelled) {
            return None;
        }
        let mut parent = index.display_parent_of(path);
        while let Some(ancestor) = parent.filter(|path| !path.as_os_str().is_empty()) {
            included.insert(ancestor.to_path_buf());
            parent = index.display_parent_of(ancestor);
        }
    }
    let mut expanded_context = BTreeSet::new();
    for path in &included {
        if is_cancelled(cancelled, projection_cancelled) {
            return None;
        }
        if index.node(path).map(|node| node.kind()) == Some(TreeNodeKind::Directory) {
            expanded_context.insert(path.clone());
        }
    }
    let mut rows = Vec::new();
    if !append_children(
        index,
        Path::new(""),
        &included,
        &mut rows,
        cancelled,
        projection_cancelled,
    ) {
        return None;
    }
    Some(SearchProjection {
        matches,
        expanded_context,
        rows,
    })
}

fn append_children(
    tree: &FilesTree,
    directory: &Path,
    included: &BTreeSet<PathBuf>,
    rows: &mut Vec<PathBuf>,
    cancelled: &AtomicBool,
    projection_cancelled: &AtomicBool,
) -> bool {
    for node in tree.children(directory) {
        if is_cancelled(cancelled, projection_cancelled) {
            return false;
        }
        if !included.contains(node.path()) {
            continue;
        }
        let path = node.path().to_path_buf();
        rows.push(path.clone());
        if node.kind() == TreeNodeKind::Directory
            && !append_children(tree, &path, included, rows, cancelled, projection_cancelled)
        {
            return false;
        }
    }
    true
}

fn is_cancelled(cancelled: &AtomicBool, page_cancelled: &AtomicBool) -> bool {
    cancelled.load(Ordering::Relaxed) || page_cancelled.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn prepares_bounded_pages_and_projects_collapsed_descendants() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("src")).expect("src");
        fs::write(temp.path().join("root.rs"), []).expect("root file");
        fs::write(temp.path().join("src/lib.rs"), []).expect("nested file");
        fs::write(temp.path().join(".hidden"), []).expect("hidden file");
        let source = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        let index = source.search_index(MAX_INDEX_ENTRIES);
        let loader = index.directory_loader();
        let cancelled = AtomicBool::new(false);
        let page_cancelled = AtomicBool::new(false);

        let root = prepare_page(
            &loader,
            SearchPageRequest {
                index,
                searchable: Arc::new(BTreeMap::new()),
                directories: vec![PathBuf::new()],
                remaining_entries: MAX_INDEX_ENTRIES,
                remaining_examined: MAX_EXAMINED_ENTRIES,
            },
            &cancelled,
            &page_cancelled,
        );
        assert_eq!(root.scanned_entries, 2);
        assert!(root.searchable.contains_key(Path::new("root.rs")));
        assert_eq!(root.discovered_directories, [PathBuf::from("src")]);

        let nested = prepare_page(
            &loader,
            SearchPageRequest {
                index: root.index,
                searchable: root.searchable,
                directories: root.discovered_directories,
                remaining_entries: MAX_INDEX_ENTRIES - root.scanned_entries,
                remaining_examined: MAX_EXAMINED_ENTRIES - root.examined_entries,
            },
            &cancelled,
            &page_cancelled,
        );
        let projection = project(
            &nested.index,
            &nested.searchable,
            "src/lib",
            &cancelled,
            &page_cancelled,
        )
        .expect("projection");
        assert_eq!(
            projection.rows,
            [PathBuf::from("src"), PathBuf::from("src/lib.rs")]
        );
        assert_eq!(
            projection.matches,
            BTreeSet::from([PathBuf::from("src/lib.rs")])
        );
        assert!(!nested.truncated);

        let source = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        let index = source.search_index(MAX_INDEX_ENTRIES);
        let truncated = prepare_page(
            &index.directory_loader(),
            SearchPageRequest {
                index,
                searchable: Arc::new(BTreeMap::new()),
                directories: vec![PathBuf::new()],
                remaining_entries: 1,
                remaining_examined: MAX_EXAMINED_ENTRIES,
            },
            &cancelled,
            &page_cancelled,
        );
        assert!(truncated.truncated);
        assert_eq!(truncated.scanned_entries, 1);

        page_cancelled.store(true, Ordering::Relaxed);
        let source = FilesTree::new(temp.path().to_path_buf()).expect("tree");
        let index = source.search_index(MAX_INDEX_ENTRIES);
        let cancelled_page = prepare_page(
            &index.directory_loader(),
            SearchPageRequest {
                index,
                searchable: Arc::new(BTreeMap::new()),
                directories: vec![PathBuf::new()],
                remaining_entries: MAX_INDEX_ENTRIES,
                remaining_examined: MAX_EXAMINED_ENTRIES,
            },
            &cancelled,
            &page_cancelled,
        );
        assert!(cancelled_page.cancelled);
        assert_eq!(cancelled_page.scanned_entries, 0);
    }
}
