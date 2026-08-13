use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use herdr_context::project::{ProjectIdentity, ProjectResolutionError, resolve_project_context};
use tempfile::TempDir;

fn directory(path: &Path) {
    fs::create_dir_all(path).expect("create test directory");
}

fn jj_marker(root: &Path) {
    directory(&root.join(".jj/repo"));
    directory(&root.join(".jj/working_copy"));
}

fn git_admin(path: &Path) {
    directory(&path.join("objects"));
    fs::write(path.join("HEAD"), "ref: refs/heads/main\n").expect("write Git HEAD fixture");
}

fn git_marker(root: &Path) {
    git_admin(&root.join(".git"));
}

fn command_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn run(program: &str, arguments: &[&str], cwd: &Path) {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(cwd)
        .output()
        .expect("run VCS fixture command");
    assert!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn plain_directory_uses_canonical_directory_identity() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let opened = temp.path().join("project");
    directory(&opened);

    let context = resolve_project_context(&opened)?;

    assert_eq!(context.files_root(), opened);
    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&opened)?
    );
    assert!(context.vcs().is_none());
    Ok(())
}

#[test]
fn valid_gitfile_sets_identity_without_changing_files_root()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let repository = temp.path().join("repository");
    let metadata = temp.path().join("git-metadata");
    let opened = repository.join("crates/app");
    directory(&opened);
    git_admin(&metadata);
    fs::write(
        repository.join(".git"),
        format!("gitdir: {}", metadata.display()),
    )?;

    let context = resolve_project_context(&opened)?;

    assert_eq!(context.files_root(), opened);
    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&repository)?
    );
    assert_eq!(
        context.vcs().expect("detected Git").backend().as_str(),
        "git"
    );
    Ok(())
}

#[test]
fn closest_jj_workspace_wins_over_closest_git_worktree() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let jj_workspace = temp.path().join("jj-workspace");
    let git_worktree = jj_workspace.join("nested-git");
    let opened = git_worktree.join("src");
    directory(&opened);
    jj_marker(&jj_workspace);
    git_marker(&git_worktree);

    let context = resolve_project_context(&opened)?;

    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&jj_workspace)?
    );
    assert_eq!(
        context.vcs().expect("detected Jujutsu").backend().as_str(),
        "jj"
    );
    Ok(())
}

#[test]
fn closest_workspace_of_same_backend_is_selected() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let outer = temp.path().join("outer");
    let inner = outer.join("inner");
    let opened = inner.join("src");
    directory(&opened);
    jj_marker(&outer);
    jj_marker(&inner);

    let context = resolve_project_context(&opened)?;

    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&inner)?
    );
    Ok(())
}

#[test]
fn colocated_jj_and_git_prefers_jj() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let repository = temp.path().join("colocated");
    directory(&repository);
    jj_marker(&repository);
    git_marker(&repository);

    let context = resolve_project_context(&repository)?;

    assert_eq!(
        context.vcs().expect("detected Jujutsu").backend().as_str(),
        "jj"
    );
    Ok(())
}

#[test]
fn invalid_nested_jj_marker_does_not_steal_git_identity() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = TempDir::new()?;
    let repository = temp.path().join("repository");
    let nested = repository.join("nested");
    let opened = nested.join("src");
    directory(&opened);
    git_marker(&repository);
    fs::write(nested.join(".jj"), "not a workspace")?;

    let context = resolve_project_context(&opened)?;

    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&repository)?
    );
    assert_eq!(
        context.vcs().expect("detected Git").backend().as_str(),
        "git"
    );
    Ok(())
}

#[test]
fn malformed_gitfile_is_ignored() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let repository = temp.path().join("repository");
    let opened = repository.join("src");
    directory(&opened);
    fs::write(repository.join(".git"), "not a gitdir")?;

    let context = resolve_project_context(&opened)?;

    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&opened)?
    );
    assert!(context.vcs().is_none());
    Ok(())
}

#[test]
fn empty_marker_directories_are_ignored() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let repository = temp.path().join("repository");
    let opened = repository.join("src");
    directory(&opened);
    directory(&repository.join(".jj"));
    directory(&repository.join(".git"));

    let context = resolve_project_context(&opened)?;

    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&opened)?
    );
    assert!(context.vcs().is_none());
    Ok(())
}

#[test]
fn project_identity_normalizes_absolute_alias() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let project = temp.path().join("project");
    let child = project.join("child");
    directory(&child);

    let direct = ProjectIdentity::from_canonical_root(project)?;
    let aliased = ProjectIdentity::from_canonical_root(child.join(".."))?;

    assert_eq!(direct, aliased);
    Ok(())
}

#[test]
fn relative_files_root_returns_typed_error() {
    let error = resolve_project_context("relative").expect_err("relative root must fail");
    assert_eq!(
        error,
        ProjectResolutionError::NonAbsoluteFilesRoot(PathBuf::from("relative"))
    );
}

#[test]
fn real_git_worktree_resolves_to_worktree_root() -> Result<(), Box<dyn std::error::Error>> {
    if !command_available("git") {
        eprintln!("skipped: git unavailable");
        return Ok(());
    }

    let temp = TempDir::new()?;
    let repository = temp.path().join("repository");
    let worktree = temp.path().join("worktree");
    directory(&repository);
    run("git", &["init", "--quiet"], &repository);
    run(
        "git",
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ],
        &repository,
    );
    run(
        "git",
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.to_str().expect("UTF-8 path"),
        ],
        &repository,
    );
    let opened = worktree.join("src");
    directory(&opened);

    let context = resolve_project_context(&opened)?;

    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&worktree)?
    );
    assert_eq!(
        context.vcs().expect("detected Git").backend().as_str(),
        "git"
    );
    Ok(())
}

#[test]
fn real_jj_workspace_resolves_and_wins_colocation() -> Result<(), Box<dyn std::error::Error>> {
    if !command_available("jj") {
        eprintln!("skipped: jj unavailable");
        return Ok(());
    }

    let temp = TempDir::new()?;
    let repository = temp.path().join("repository");
    run(
        "jj",
        &[
            "git",
            "init",
            "--colocate",
            repository.to_str().expect("UTF-8 path"),
        ],
        temp.path(),
    );
    let opened = repository.join("src");
    directory(&opened);

    let context = resolve_project_context(&opened)?;

    assert_eq!(
        context.conversation_identity().root(),
        fs::canonicalize(&repository)?
    );
    assert_eq!(
        context.vcs().expect("detected Jujutsu").backend().as_str(),
        "jj"
    );
    Ok(())
}

#[test]
fn missing_directory_returns_typed_resolution_error() {
    let missing = PathBuf::from("/path/that/does/not/exist/herdr-context-test");
    let error = resolve_project_context(missing).expect_err("missing root must fail");
    assert!(error.to_string().contains("cannot canonicalize"));
    assert!(std::error::Error::source(&error).is_some());
}
