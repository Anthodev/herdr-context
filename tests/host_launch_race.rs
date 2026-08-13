use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use herdr_context::host::launch::TabLock;
use herdr_context::host::{TabId, WorkspaceId};
use tempfile::TempDir;

const HELPER_ENV: &str = "HERDR_CONTEXT_LOCK_HELPER";

#[test]
fn lock_helper_process() -> Result<(), Box<dyn std::error::Error>> {
    let Some(state_dir) = std::env::var_os(HELPER_ENV) else {
        return Ok(());
    };
    let workspace = WorkspaceId::new(std::env::var("LOCK_WORKSPACE")?)?;
    let tab = TabId::new(std::env::var("LOCK_TAB")?)?;
    let timeout = Duration::from_millis(std::env::var("LOCK_TIMEOUT_MS")?.parse()?);
    let hold = Duration::from_millis(std::env::var("LOCK_HOLD_MS")?.parse()?);

    match TabLock::acquire(&state_dir, &workspace, &tab, timeout) {
        Ok(_lock) => {
            println!("locked");
            std::io::stdout().flush()?;
            std::thread::sleep(hold);
        }
        Err(error) => {
            println!("error:{error}");
            std::io::stdout().flush()?;
        }
    }
    Ok(())
}

struct Helper {
    child: std::process::Child,
    stdout: BufReader<std::process::ChildStdout>,
}

fn spawn_helper(
    state_dir: &std::path::Path,
    tab: &str,
    timeout_ms: u64,
    hold_ms: u64,
) -> Result<Helper, Box<dyn std::error::Error>> {
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", "lock_helper_process", "--nocapture"])
        .env(HELPER_ENV, state_dir)
        .env("LOCK_WORKSPACE", "workspace")
        .env("LOCK_TAB", tab)
        .env("LOCK_TIMEOUT_MS", timeout_ms.to_string())
        .env("LOCK_HOLD_MS", hold_ms.to_string())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("helper stdout unavailable")?;
    Ok(Helper {
        child,
        stdout: BufReader::new(stdout),
    })
}

fn read_helper_line(helper: &mut Helper) -> Result<String, Box<dyn std::error::Error>> {
    loop {
        let mut line = String::new();
        if helper.stdout.read_line(&mut line)? == 0 {
            return Err("helper exited before reporting lock state".into());
        }
        if line.contains("locked") || line.contains("error:") {
            return Ok(line);
        }
    }
}

#[test]
fn same_tab_lock_wait_is_bounded_across_processes() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut holder = spawn_helper(state.path(), "tab", 500, 800)?;
    assert!(read_helper_line(&mut holder)?.contains("locked"));

    let started = Instant::now();
    let mut contender = spawn_helper(state.path(), "tab", 100, 0)?;
    let line = read_helper_line(&mut contender)?;
    let status = contender.child.wait()?;

    assert!(status.success());
    assert!(line.contains("timed out"));
    assert!(started.elapsed() < Duration::from_millis(500));
    holder.child.kill()?;
    holder.child.wait()?;
    Ok(())
}

#[test]
fn independent_tabs_do_not_block_each_other() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut holder = spawn_helper(state.path(), "tab-a", 500, 800)?;
    assert!(read_helper_line(&mut holder)?.contains("locked"));

    let started = Instant::now();
    let mut independent = spawn_helper(state.path(), "tab-b", 200, 0)?;
    let line = read_helper_line(&mut independent)?;
    let status = independent.child.wait()?;

    assert!(status.success());
    assert!(line.contains("locked"));
    assert!(started.elapsed() < Duration::from_millis(500));
    holder.child.kill()?;
    holder.child.wait()?;
    Ok(())
}

#[test]
fn process_exit_releases_lock_without_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    let state = TempDir::new()?;
    let mut holder = spawn_helper(state.path(), "tab", 500, 5_000)?;
    assert!(read_helper_line(&mut holder)?.contains("locked"));
    holder.child.kill()?;
    holder.child.wait()?;

    let workspace = WorkspaceId::new("workspace")?;
    let tab = TabId::new("tab")?;
    let _recovered = TabLock::acquire(state.path(), &workspace, &tab, Duration::from_millis(200))?;
    Ok(())
}

#[test]
fn lock_key_separates_workspace_and_tab_components() -> Result<(), Box<dyn std::error::Error>> {
    let workspace_ab = WorkspaceId::new("ab")?;
    let workspace_a = WorkspaceId::new("a")?;
    let tab_c = TabId::new("c")?;
    let tab_bc = TabId::new("bc")?;

    assert_ne!(
        TabLock::file_name(&workspace_ab, &tab_c),
        TabLock::file_name(&workspace_a, &tab_bc)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinked_lock_file_is_rejected_without_chmodding_target()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let state = TempDir::new()?;
    let lock_dir = state.path().join("locks");
    std::fs::create_dir(&lock_dir)?;
    let victim = state.path().join("victim");
    std::fs::write(&victim, b"unchanged")?;
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644))?;
    let workspace = WorkspaceId::new("workspace")?;
    let tab = TabId::new("tab")?;
    symlink(&victim, lock_dir.join(TabLock::file_name(&workspace, &tab)))?;

    let result = TabLock::acquire(state.path(), &workspace, &tab, Duration::from_millis(20));

    assert!(result.is_err());
    assert_eq!(std::fs::read(&victim)?, b"unchanged");
    assert_eq!(
        std::fs::metadata(&victim)?.permissions().mode() & 0o777,
        0o644
    );
    Ok(())
}
