use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

#[derive(Clone, Debug)]
pub struct IgnorePolicy {
    root: PathBuf,
}

impl IgnorePolicy {
    pub fn new(root: PathBuf) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root must be an absolute directory",
            ));
        }
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root must be a directory",
            ));
        }
        Ok(Self { root })
    }

    pub fn visible_children(&self, relative_directory: &Path) -> io::Result<Vec<PathBuf>> {
        validate_relative_directory(relative_directory)?;
        let directory = self.root.join(relative_directory);
        let canonical = fs::canonicalize(&directory)?;
        if canonical != directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tree directory must not contain symlinked components",
            ));
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tree directory must be a real directory",
            ));
        }

        let mut builder = WalkBuilder::new(&directory);
        builder
            .max_depth(Some(1))
            .follow_links(false)
            .hidden(false)
            .ignore(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .parents(true)
            .require_git(true);

        let mut children = Vec::new();
        for result in builder.build().skip(1) {
            let entry = result.map_err(ignore_error)?;
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .map_err(|_| io::Error::other("ignore walker escaped tree root"))?;
            if relative == Path::new(".git") {
                continue;
            }
            children.push(relative.to_path_buf());
        }
        children.sort_unstable();
        Ok(children)
    }
}

fn validate_relative_directory(path: &Path) -> io::Result<()> {
    if path
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
        || path.as_os_str().is_empty()
    {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "tree path must be root-relative and normalized",
    ))
}

fn ignore_error(error: ignore::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::IgnorePolicy;

    fn touch(path: impl AsRef<Path>) {
        fs::write(path, []).expect("write fixture");
    }

    #[test]
    fn honors_root_anchored_directory_glob_and_negated_rules_lazily() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::create_dir(temp.path().join("build")).expect("build dir");
        fs::write(
            temp.path().join(".gitignore"),
            "/anchored.txt\n*.log\nbuild/*\n!build/keep.txt\n",
        )
        .expect("root ignore");
        touch(temp.path().join("anchored.txt"));
        touch(temp.path().join("visible.txt"));
        touch(temp.path().join("debug.log"));
        touch(temp.path().join("build/drop.txt"));
        touch(temp.path().join("build/keep.txt"));

        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");
        let root = policy
            .visible_children(Path::new(""))
            .expect("root children");
        assert!(root.contains(&PathBuf::from("build")));
        assert!(root.contains(&PathBuf::from("visible.txt")));
        assert!(!root.contains(&PathBuf::from("anchored.txt")));
        assert!(!root.contains(&PathBuf::from("debug.log")));

        assert_eq!(
            policy
                .visible_children(Path::new("build"))
                .expect("build children"),
            vec![PathBuf::from("build/keep.txt")]
        );
    }

    #[test]
    fn nested_rules_override_parent_rules() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect("git marker");
        fs::create_dir(temp.path().join("nested")).expect("nested dir");
        fs::write(temp.path().join(".gitignore"), "*.tmp\n").expect("root ignore");
        fs::write(
            temp.path().join("nested/.gitignore"),
            "!important.tmp\n/local-only\n",
        )
        .expect("nested ignore");
        touch(temp.path().join("nested/drop.tmp"));
        touch(temp.path().join("nested/important.tmp"));
        touch(temp.path().join("nested/local-only"));

        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");
        let children = policy
            .visible_children(Path::new("nested"))
            .expect("children");
        assert!(children.contains(&PathBuf::from("nested/important.tmp")));
        assert!(!children.contains(&PathBuf::from("nested/drop.tmp")));
        assert!(!children.contains(&PathBuf::from("nested/local-only")));
    }

    #[test]
    fn does_not_read_gitignore_above_the_repository_root() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join(".gitignore"), "*.txt\n").expect("parent ignore");
        let repository = temp.path().join("repository");
        fs::create_dir(&repository).expect("repository");
        fs::create_dir(repository.join(".git")).expect("git marker");
        touch(repository.join("visible.txt"));

        let policy = IgnorePolicy::new(repository).expect("policy");
        assert_eq!(
            policy.visible_children(Path::new("")).expect("children"),
            vec![PathBuf::from("visible.txt")]
        );
    }
    #[test]
    fn rejects_paths_outside_the_lazy_tree_root() {
        let temp = TempDir::new().expect("tempdir");
        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");
        assert!(policy.visible_children(Path::new("../outside")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_directories_reached_through_an_intermediate_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside");
        fs::create_dir_all(temp.path().join("a/b")).expect("inside directory");
        fs::create_dir(outside.path().join("b")).expect("outside directory");
        touch(outside.path().join("b/secret"));
        let policy = IgnorePolicy::new(temp.path().to_path_buf()).expect("policy");
        fs::remove_dir_all(temp.path().join("a")).expect("remove inside directory");
        symlink(outside.path(), temp.path().join("a")).expect("replace with symlink");

        assert!(policy.visible_children(Path::new("a/b")).is_err());
    }
}
