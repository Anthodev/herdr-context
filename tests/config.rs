use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use herdr_context::config::{GitCadence, KeyAction, PluginConfig, VcsBackendSelection};
use herdr_context::intent::Intent;
use herdr_context::vcs::jj::JujutsuMode;
use tempfile::TempDir;

fn load(contents: &str) -> herdr_context::config::ConfigLoad {
    let directory = TempDir::new().expect("config directory");
    fs::write(directory.path().join("config.toml"), contents).expect("config file");
    let loaded = PluginConfig::load_from_dir(directory.path());
    assert_eq!(
        fs::read_to_string(directory.path().join("config.toml")).expect("unchanged config"),
        contents,
        "configuration loading must be read-only"
    );
    loaded
}

#[test]
fn missing_and_malformed_config_fall_back_without_failing() {
    let missing = TempDir::new().expect("config directory");
    let missing = PluginConfig::load_from_dir(missing.path());
    assert_eq!(missing.config(), &PluginConfig::default());
    assert!(
        missing
            .warnings()
            .iter()
            .any(|warning| warning.contains("missing"))
    );

    let malformed = load("[dock\ninitial_width = 50");
    assert_eq!(malformed.config(), &PluginConfig::default());
    assert!(
        malformed
            .warnings()
            .iter()
            .any(|warning| warning.contains("malformed"))
    );
}

#[test]
fn non_array_enabled_sources_warns_and_uses_the_default_set() {
    let loaded = load("[conversations]\nenabled_sources = \"pi\"\n");

    assert_eq!(
        loaded.config().conversations().enabled_sources(),
        PluginConfig::default().conversations().enabled_sources()
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("conversations.enabled_sources"))
    );
}

#[test]
fn oversized_keybinding_lists_warn_and_keep_the_safe_action_binding() {
    let chords = ('a'..='q')
        .map(|chord| format!("\"{chord}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let loaded = load(&format!("[keybindings]\nquit = [{chords}]\n"));

    assert_eq!(
        loaded.config().keybindings().intent_for("q"),
        Some(Intent::Quit)
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("keybindings.quit"))
    );
}

#[test]
fn unknown_fields_warn_without_echoing_the_unknown_names() {
    let loaded = load("[conversations]\ncache_entires = 16\n[vcs]\ngit_min_intervl_ms = 500\n");

    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("conversations.unknown_field"))
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("vcs.unknown_field"))
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .all(|warning| !warning.contains("entires") && !warning.contains("intervl"))
    );
}

#[test]
fn oversized_project_root_is_rejected_without_losing_valid_neighbors() {
    let oversized = "x".repeat(1_025);
    let loaded = load(&format!(
        "[conversations]\nproject_roots = [\"valid\", \"{oversized}\"]\n"
    ));

    assert_eq!(
        loaded.config().conversations().project_roots(),
        [PathBuf::from("valid")]
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("conversations.project_roots"))
    );
}

#[test]
fn oversized_external_root_is_rejected_without_losing_valid_neighbors() {
    let oversized = format!("/{}", "x".repeat(4_096));
    let loaded = load(&format!(
        "[conversations.external_roots]\npi = [\"/valid\", \"{oversized}\"]\n"
    ));

    assert_eq!(loaded.config().conversations().external_roots().len(), 1);
    assert_eq!(
        loaded.config().conversations().external_roots()[0].path(),
        Path::new("/valid")
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("conversations.external_roots"))
    );
}

#[test]
fn valid_fields_survive_invalid_neighbors_and_limits_are_bounded() {
    let loaded = load(
        r#"
[dock]
initial_width = 52

[files]
show_hidden = true
exclusions = ["target", "generated/cache"]

[conversations]
enabled_sources = ["codex-cli", "project-local-generic-jsonl"]
project_roots = [".agents/history"]
page_size = 0
cache_entries = 70000

[conversations.external_roots]
codex-cli = ["/var/lib/codex/sessions"]

[vcs]
backend = "git"
jujutsu_mode = "passive"
git_cadence = "adaptive"
git_min_interval_ms = 500
git_max_interval_ms = 10000
passive_jujutsu_interval_ms = 2000
"#,
    );

    let config = loaded.config();
    assert_eq!(config.dock().initial_width(), 52);
    assert!(config.files().show_hidden());
    assert_eq!(
        config.files().exclusions(),
        [PathBuf::from("target"), PathBuf::from("generated/cache")]
    );
    assert_eq!(
        config.conversations().enabled_sources(),
        &["codex-cli", "project-local-generic-jsonl"]
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        config.conversations().project_roots(),
        [PathBuf::from(".agents/history")]
    );
    assert_eq!(config.conversations().page_size().get(), 128);
    assert_eq!(config.conversations().cache_entries().get(), 4096);
    assert_eq!(config.conversations().external_roots().len(), 1);
    assert_eq!(
        config.conversations().external_roots()[0].source(),
        "codex-cli"
    );
    assert_eq!(
        config.conversations().external_roots()[0].path(),
        Path::new("/var/lib/codex/sessions")
    );
    assert_eq!(config.vcs().backend(), VcsBackendSelection::Git);
    assert_eq!(config.vcs().jujutsu_mode(), JujutsuMode::Passive);
    assert_eq!(
        config.vcs().refresh().git(),
        GitCadence::Adaptive {
            minimum: Duration::from_millis(500),
            maximum: Duration::from_secs(10),
        }
    );
    assert_eq!(
        config.vcs().refresh().passive_jujutsu(),
        Some(Duration::from_secs(2))
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("page_size"))
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("cache_entries"))
    );
}

#[test]
fn invalid_fields_fall_back_independently_and_warnings_never_echo_values() {
    let loaded = load(
        r#"
[dock]
initial_width = "bad\u001b[2J"

[files]
show_hidden = false
exclusions = ["../escape", "/absolute", "valid"]

[conversations]
enabled_sources = ["unknown\u001b-source", "pi"]
project_roots = ["../outside", ".valid"]

[conversations.external_roots]
pi = ["relative/path", "/safe/pi"]

[vcs]
backend = "svn\u001b"
jujutsu_mode = "fresh"
"#,
    );

    let config = loaded.config();
    assert_eq!(config.dock().initial_width(), 40);
    assert_eq!(config.files().exclusions(), [PathBuf::from("valid")]);
    assert_eq!(
        config.conversations().enabled_sources(),
        &std::iter::once(String::from("pi")).collect::<BTreeSet<_>>()
    );
    assert_eq!(
        config.conversations().project_roots(),
        [PathBuf::from(".valid")]
    );
    assert_eq!(config.conversations().external_roots().len(), 1);
    assert_eq!(config.vcs().backend(), VcsBackendSelection::Auto);
    assert!(loaded.warnings().len() >= 5);
    assert!(
        loaded
            .warnings()
            .iter()
            .all(|warning| !warning.contains('\u{1b}'))
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .all(|warning| !warning.contains("unknown"))
    );
}

#[test]
fn configured_keybindings_match_runtime_events_and_replace_the_default_action_binding() {
    let loaded = load(
        r#"
[keybindings]
refresh = ["x"]
"#,
    );
    let keymap = loaded.config().keybindings();

    assert_eq!(
        keymap.map_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        Some(Intent::Refresh)
    );
    assert_eq!(
        keymap.map_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        None
    );
}

#[test]
fn conflicting_keybindings_revert_only_the_conflicting_actions() {
    let loaded = load(
        r#"
[keybindings]
quit = ["x"]
refresh = ["x"]
select_next = ["n"]
"#,
    );

    let keymap = loaded.config().keybindings();
    assert_eq!(keymap.intent_for("q"), Some(Intent::Quit));
    assert_eq!(keymap.intent_for("r"), Some(Intent::Refresh));
    assert_eq!(keymap.intent_for("n"), Some(Intent::SelectNext));
    assert_eq!(keymap.bindings_for(KeyAction::Quit), ["q", "esc", "ctrl+c"]);
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("keybindings.quit"))
    );
    assert!(
        loaded
            .warnings()
            .iter()
            .any(|warning| warning.contains("keybindings.refresh"))
    );
}
