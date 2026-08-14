#![allow(
    dead_code,
    reason = "shared by the benchmark executable and focused integration tests"
)]

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const EXTERNAL_SESSION_COUNT: usize = 2_048;
pub const LOCAL_SESSION_COUNT: usize = 64;
pub const MONOREPO_VISIBLE_FILE_COUNT: usize = 1_024;
pub const MONOREPO_IGNORED_FILE_COUNT: usize = 4_096;
pub const MAX_FIXTURE_BYTES: u64 = 32 * 1024 * 1024;

const OWNED_DIRECTORY: &str = "hdc-15-performance-fixtures";
const CLAUDE_TEMPLATE: &str = include_str!(
    "../fixtures/conversations/claude-code/-workspace-project/11111111-1111-4111-8111-111111111111.jsonl"
);
const CODEX_TEMPLATE: &str = include_str!(
    "../fixtures/conversations/codex-cli/2026/01/02/rollout-2026-01-02T03-04-05-019b7c3b-af88-7000-8001-000000000001.jsonl"
);
const PI_TEMPLATE: &str = include_str!(
    "../fixtures/conversations/pi/--workspace-project--/2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl"
);
const OMP_TEMPLATE: &str = include_str!(
    "../fixtures/conversations/omp/--workspace-project--/2026-01-04T05-06-07-000Z_019b8721-4a18-7000-8005-000000000005.jsonl"
);

#[derive(Clone, Debug)]
pub struct PerformanceFixtures {
    root: PathBuf,
    no_vcs: PathBuf,
    small_git: PathBuf,
    native_jj: PathBuf,
    colocated_jj: PathBuf,
    monorepo: PathBuf,
    local_project: PathBuf,
    append_project: PathBuf,
    append_transcript: PathBuf,
    external_project: PathBuf,
    home: PathBuf,
    state: PathBuf,
    fake_git_bin: PathBuf,
    fake_git_log: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VcsFixtureMode {
    ExecutableBacked,
    MarkerOnly,
}

impl PerformanceFixtures {
    pub fn create(parent: &Path) -> io::Result<Self> {
        Self::create_with_mode(parent, VcsFixtureMode::ExecutableBacked)
    }

    pub fn create_for_tests(parent: &Path) -> io::Result<Self> {
        Self::create_with_mode(parent, VcsFixtureMode::MarkerOnly)
    }

    fn create_with_mode(parent: &Path, vcs_mode: VcsFixtureMode) -> io::Result<Self> {
        fs::create_dir_all(parent)?;
        let root = parent.join(OWNED_DIRECTORY);
        remove_owned_directory(&root)?;
        fs::create_dir(&root)?;

        let no_vcs = root.join("no-vcs");
        fs::create_dir(&no_vcs)?;
        fs::create_dir(no_vcs.join("virtual"))?;
        for index in 0..64 {
            write_synthetic(
                &no_vcs.join(format!("plain-{index:03}.txt")),
                format!("synthetic no-vcs file {index:03}\n").as_bytes(),
            )?;
        }

        let small_git = root.join("small-git");
        fs::create_dir(&small_git)?;
        for index in 0..128 {
            write_synthetic(
                &small_git.join(format!("tracked-{index:03}.rs")),
                format!("pub const ITEM_{index:03}: usize = {index};\n").as_bytes(),
            )?;
        }
        if vcs_mode == VcsFixtureMode::ExecutableBacked {
            run_tool("git", ["init", "--quiet", path_arg(&small_git)?], &root)?;
        } else {
            fs::create_dir(small_git.join(".git"))?;
        }

        let native_jj = root.join("native-jj");
        fs::create_dir(&native_jj)?;
        populate_small_workspace(&native_jj)?;
        if vcs_mode == VcsFixtureMode::ExecutableBacked {
            run_tool(
                "jj",
                ["--quiet", "git", "init", path_arg(&native_jj)?],
                &root,
            )?;
        } else {
            install_jj_markers(&native_jj, false)?;
        }
        write_synthetic(
            &native_jj.join("item-000.txt"),
            b"synthetic measured Jujutsu modification\n",
        )?;

        let colocated_jj = root.join("colocated-jj");
        fs::create_dir(&colocated_jj)?;
        populate_small_workspace(&colocated_jj)?;
        if vcs_mode == VcsFixtureMode::ExecutableBacked {
            run_tool(
                "jj",
                [
                    "--quiet",
                    "git",
                    "init",
                    "--colocate",
                    path_arg(&colocated_jj)?,
                ],
                &root,
            )?;
        } else {
            install_jj_markers(&colocated_jj, true)?;
        }
        write_synthetic(
            &colocated_jj.join("item-000.txt"),
            b"synthetic measured Jujutsu modification\n",
        )?;
        if vcs_mode == VcsFixtureMode::ExecutableBacked {
            run_tool(
                "jj",
                ["--quiet", "-R", path_arg(&colocated_jj)?, "status"],
                &root,
            )?;
        }

        let monorepo = root.join("ignore-heavy-monorepo");
        fs::create_dir(&monorepo)?;
        write_synthetic(&monorepo.join(".gitignore"), b"target/\nnode_modules/\n")?;
        for package in 0..16 {
            let package_root = monorepo.join(format!("packages/pkg-{package:02}"));
            let source = package_root.join("src");
            let ignored = package_root.join("target/cache");
            fs::create_dir_all(&source)?;
            fs::create_dir_all(&ignored)?;
            for file in 0..64 {
                write_synthetic(
                    &source.join(format!("module-{file:03}.rs")),
                    format!("pub const VALUE: usize = {};\n", package * 64 + file).as_bytes(),
                )?;
            }
            for file in 0..256 {
                write_synthetic(
                    &ignored.join(format!("artifact-{file:03}.bin")),
                    b"synthetic ignored artifact\n",
                )?;
            }
        }
        if vcs_mode == VcsFixtureMode::ExecutableBacked {
            run_tool("git", ["init", "--quiet", path_arg(&monorepo)?], &root)?;
        } else {
            fs::create_dir(monorepo.join(".git"))?;
        }

        let local_project = root.join("local-history-project");
        let local_store = local_project.join(".herdr/conversations");
        fs::create_dir_all(&local_store)?;
        let local_cwd = absolute_utf8(&local_project)?;
        for index in 0..LOCAL_SESSION_COUNT {
            let body = generic_transcript(&local_cwd, &format!("local-{index:04}"), index);
            write_synthetic(
                &local_store.join(format!("local-{index:04}.jsonl")),
                body.as_bytes(),
            )?;
        }

        let append_project = root.join("append-project");
        let append_store = append_project.join(".herdr/conversations");
        fs::create_dir_all(&append_store)?;
        let append_transcript = append_store.join("appending.jsonl");
        let append_cwd = absolute_utf8(&append_project)?;
        let first = generic_record(
            &append_cwd,
            "appending-session",
            "2026-01-01T00:00:00Z",
            "user",
        );
        write_synthetic(&append_transcript, format!("{first}\n").as_bytes())?;

        let external_project = root.join("external-project");
        fs::create_dir(&external_project)?;
        let external_cwd = absolute_utf8(&external_project)?;
        let home = root.join("synthetic-home");
        let state = root.join("state");
        fs::create_dir(&home)?;
        fs::create_dir(&state)?;
        install_external_sessions(&home, &external_project, &external_cwd)?;
        let fake_git_bin = root.join("fake-git-bin");
        let fake_git_log = fake_git_bin.join("status-events.log");
        install_fake_git(&fake_git_bin)?;

        Ok(Self {
            root,
            no_vcs,
            small_git,
            native_jj,
            colocated_jj,
            monorepo,
            local_project,
            append_project,
            append_transcript,
            external_project,
            home,
            state,
            fake_git_bin,
            fake_git_log,
        })
    }

    pub fn from_existing(parent: &Path) -> io::Result<Self> {
        let root = parent.join(OWNED_DIRECTORY);
        let append_project = root.join("append-project");
        let fixtures = Self {
            no_vcs: root.join("no-vcs"),
            small_git: root.join("small-git"),
            native_jj: root.join("native-jj"),
            colocated_jj: root.join("colocated-jj"),
            monorepo: root.join("ignore-heavy-monorepo"),
            local_project: root.join("local-history-project"),
            append_transcript: append_project.join(".herdr/conversations/appending.jsonl"),
            append_project,
            external_project: root.join("external-project"),
            home: root.join("synthetic-home"),
            state: root.join("state"),
            fake_git_bin: root.join("fake-git-bin"),
            fake_git_log: root.join("fake-git-bin/status-events.log"),
            root,
        };
        fixtures.validate()?;
        Ok(fixtures)
    }

    pub fn validate(&self) -> io::Result<FixtureManifest> {
        let external = count_extension(&self.home.join(".codex/sessions/2026/01/02"), "jsonl")?;
        let local = count_extension(&self.local_project.join(".herdr/conversations"), "jsonl")?;
        let mut visible = 0;
        let mut ignored = 0;
        for package in 0..16 {
            let package_root = self.monorepo.join(format!("packages/pkg-{package:02}"));
            visible += count_extension(&package_root.join("src"), "rs")?;
            ignored += count_extension(&package_root.join("target/cache"), "bin")?;
        }
        if external != EXTERNAL_SESSION_COUNT
            || local != LOCAL_SESSION_COUNT
            || visible != MONOREPO_VISIBLE_FILE_COUNT
            || ignored != MONOREPO_IGNORED_FILE_COUNT
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "performance fixture count mismatch",
            ));
        }
        let total_payload_bytes = payload_bytes(&self.root)?;
        if total_payload_bytes > MAX_FIXTURE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "performance fixture exceeds {} bytes: {total_payload_bytes}",
                    MAX_FIXTURE_BYTES
                ),
            ));
        }
        Ok(FixtureManifest {
            external_sessions: external,
            local_sessions: local,
            monorepo_visible_files: visible,
            monorepo_ignored_files: ignored,
            total_payload_bytes,
        })
    }

    pub fn append_synthetic_record(&self) -> io::Result<()> {
        use std::io::Write;
        let cwd = absolute_utf8(&self.append_project)?;
        let record = generic_record(
            &cwd,
            "appending-session",
            "2026-01-01T00:00:01Z",
            "assistant",
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&self.append_transcript)?;
        writeln!(file, "{record}")
    }

    pub fn reset_append_transcript(&self) -> io::Result<()> {
        let cwd = absolute_utf8(&self.append_project)?;
        let record = generic_record(&cwd, "appending-session", "2026-01-01T00:00:00Z", "user");
        fs::write(&self.append_transcript, format!("{record}\n"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn no_vcs(&self) -> &Path {
        &self.no_vcs
    }
    pub fn small_git(&self) -> &Path {
        &self.small_git
    }
    pub fn native_jj(&self) -> &Path {
        &self.native_jj
    }
    pub fn colocated_jj(&self) -> &Path {
        &self.colocated_jj
    }
    pub fn monorepo(&self) -> &Path {
        &self.monorepo
    }
    pub fn local_project(&self) -> &Path {
        &self.local_project
    }
    pub fn append_project(&self) -> &Path {
        &self.append_project
    }
    pub fn append_transcript(&self) -> &Path {
        &self.append_transcript
    }
    pub fn external_project(&self) -> &Path {
        &self.external_project
    }
    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn state(&self) -> &Path {
        &self.state
    }
    pub fn fake_git_bin(&self) -> &Path {
        &self.fake_git_bin
    }
    pub fn fake_git_log(&self) -> &Path {
        &self.fake_git_log
    }
    pub fn reset_fake_git_log(&self) -> io::Result<()> {
        match fs::remove_file(&self.fake_git_log) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixtureManifest {
    external_sessions: usize,
    local_sessions: usize,
    monorepo_visible_files: usize,
    monorepo_ignored_files: usize,
    total_payload_bytes: u64,
}

impl FixtureManifest {
    pub const fn external_sessions(self) -> usize {
        self.external_sessions
    }
    pub const fn local_sessions(self) -> usize {
        self.local_sessions
    }
    pub const fn monorepo_visible_files(self) -> usize {
        self.monorepo_visible_files
    }
    pub const fn monorepo_ignored_files(self) -> usize {
        self.monorepo_ignored_files
    }
    pub const fn total_payload_bytes(self) -> u64 {
        self.total_payload_bytes
    }
}

fn install_external_sessions(home: &Path, project: &Path, cwd: &str) -> io::Result<()> {
    let codex = home.join(".codex/sessions/2026/01/02");
    fs::create_dir_all(&codex)?;
    let base_id = "019b7c3b-af88-7000-8001-000000000001";
    for index in 1..=EXTERNAL_SESSION_COUNT {
        let id = format!("019b7c3b-af88-7000-8001-{index:012x}");
        let body = CODEX_TEMPLATE
            .replace(base_id, &id)
            .replace("/workspace/project", cwd);
        write_synthetic(
            &codex.join(format!("rollout-2026-01-02T03-04-05-{id}.jsonl")),
            body.as_bytes(),
        )?;
    }

    let claude = home.join(".claude/projects").join(claude_directory(cwd));
    fs::create_dir_all(&claude)?;
    write_synthetic(
        &claude.join("11111111-1111-4111-8111-111111111111.jsonl"),
        CLAUDE_TEMPLATE
            .replace("/workspace/project", cwd)
            .as_bytes(),
    )?;

    let pi = home.join(".pi/agent/sessions").join(pi_directory(cwd));
    fs::create_dir_all(&pi)?;
    write_synthetic(
        &pi.join("2026-01-02T03-04-05-000Z_019b7ca9-8c88-7000-8003-000000000003.jsonl"),
        PI_TEMPLATE.replace("/workspace/project", cwd).as_bytes(),
    )?;

    let omp = home
        .join(".omp/agent/sessions")
        .join(omp_directory(project, home)?);
    fs::create_dir_all(&omp)?;
    write_synthetic(
        &omp.join("2026-01-04T05-06-07-000Z_019b8721-4a18-7000-8005-000000000005.jsonl"),
        OMP_TEMPLATE.replace("/workspace/project", cwd).as_bytes(),
    )
}

fn generic_transcript(cwd: &str, session: &str, index: usize) -> String {
    format!(
        "{}\n{}\n",
        generic_record(cwd, session, "2026-01-01T00:00:00Z", "user"),
        generic_record(
            cwd,
            session,
            &format!("2026-01-01T00:{:02}:01Z", index % 60),
            "assistant"
        )
    )
}

fn generic_record(cwd: &str, session: &str, timestamp: &str, role: &str) -> String {
    serde_json::json!({
        "session_id": session,
        "cwd": cwd,
        "timestamp": timestamp,
        "role": role,
        "message": "synthetic benchmark message"
    })
    .to_string()
}

fn populate_small_workspace(root: &Path) -> io::Result<()> {
    for index in 0..64 {
        write_synthetic(
            &root.join(format!("item-{index:03}.txt")),
            format!("synthetic Jujutsu file {index:03}\n").as_bytes(),
        )?;
    }
    Ok(())
}

fn remove_owned_directory(path: &Path) -> io::Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    }
}

fn write_synthetic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)
}

#[cfg(unix)]
fn install_fake_git(directory: &Path) -> io::Result<()> {
    fs::create_dir(directory)?;
    let executable = directory.join("git");
    let script = r#"#!/bin/sh
set -eu
is_status=0
for argument in "$@"; do
    if [ "$argument" = "status" ]; then
        is_status=1
    fi
done
if [ "$is_status" -eq 0 ]; then
    exit 0
fi
log_dir=${0%/*}
owned_lock=0
if (set -C; : > "$log_dir/status-active.lock") 2>/dev/null; then
    owned_lock=1
else
    printf 'overlap %s\n' "$$" >> "$log_dir/status-events.log"
fi
printf 'start %s\n' "$$" >> "$log_dir/status-events.log"
printf '%s\n' "$$" >> "$log_dir/status-pids.log"
/usr/bin/sleep 0.05
if [ "$owned_lock" -eq 1 ]; then
    /usr/bin/rm -f "$log_dir/status-active.lock"
fi
printf 'end %s\n' "$$" >> "$log_dir/status-events.log"
"#;
    fs::write(&executable, script)?;
    let mut permissions = fs::metadata(&executable)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(executable, permissions)
}

#[cfg(not(unix))]
fn install_fake_git(_directory: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "performance status fixtures require Unix",
    ))
}

fn install_jj_markers(root: &Path, colocated: bool) -> io::Result<()> {
    fs::create_dir_all(root.join(".jj/repo"))?;
    fs::create_dir_all(root.join(".jj/working_copy"))?;
    if colocated {
        fs::create_dir(root.join(".git"))?;
    }
    Ok(())
}

fn run_tool<const N: usize>(tool: &str, args: [&str; N], cwd: &Path) -> io::Result<()> {
    let path = env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is unavailable"))?;
    let home = cwd.join(".tool-home");
    fs::create_dir_all(&home)?;
    let output = Command::new(tool)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("JJ_CONFIG", "/dev/null")
        .env("LC_ALL", "C")
        .env("NO_COLOR", "1")
        .output()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{tool} fixture setup failed to start: {error}"),
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{tool} fixture setup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn path_arg(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fixture path is not UTF-8"))
}

fn absolute_utf8(path: &Path) -> io::Result<String> {
    fs::canonicalize(path)?
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fixture path is not UTF-8"))
}

fn count_extension(directory: &Path, extension: &str) -> io::Result<usize> {
    Ok(fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension() == Some(OsStr::new(extension)))
        .count())
}

fn payload_bytes(root: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

fn claude_directory(cwd: &str) -> String {
    cwd.encode_utf16()
        .map(|unit| {
            if u8::try_from(unit).is_ok_and(|byte| byte.is_ascii_alphanumeric()) {
                char::from_u32(u32::from(unit)).expect("ASCII")
            } else {
                '-'
            }
        })
        .collect()
}

fn pi_directory(cwd: &str) -> String {
    format!("--{}--", cwd.trim_start_matches('/').replace('/', "-"))
}

fn omp_directory(project: &Path, home: &Path) -> io::Result<String> {
    let project = fs::canonicalize(project)?;
    let home = fs::canonicalize(home)?;
    let temp = fs::canonicalize(std::env::temp_dir())?;
    if let Ok(relative) = project.strip_prefix(&home) {
        return Ok(encode_omp_relative("-", relative));
    }
    if let Ok(relative) = project.strip_prefix(&temp) {
        return Ok(encode_omp_relative("-tmp", relative));
    }
    Ok(format!(
        "--{}--",
        project
            .to_string_lossy()
            .trim_start_matches(['/', '\\'])
            .replace(['/', '\\', ':'], "-")
    ))
}

fn encode_omp_relative(prefix: &str, relative: &Path) -> String {
    let encoded = relative.to_string_lossy().replace(['/', '\\', ':'], "-");
    if encoded.is_empty() {
        prefix.to_owned()
    } else if prefix.ends_with('-') {
        format!("{prefix}{encoded}")
    } else {
        format!("{prefix}-{encoded}")
    }
}
