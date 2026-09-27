use super::*;

use std::time::Duration;

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    /// A HOME and a `bin/` holding `berth` and an executable `berth-hook`.
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let bin = dir.path().join("bin dir");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        for name in ["berth", "berth-hook"] {
            fs::write(bin.join(name), "#!/bin/sh\n").unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        Fixture {
            _dir: dir,
            home,
            bin,
        }
    }

    fn env(&self, secs: u64) -> Env {
        Env {
            home: self.home.clone(),
            exe: Some(self.bin.join("berth")),
            cwd: self.home.clone(),
            claude_config_dir: false,
            codex_home: false,
            now: SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000 + secs),
        }
    }

    fn settings(&self) -> PathBuf {
        self.home.join(".claude/settings.json")
    }

    fn write_settings(&self, text: &str) {
        fs::create_dir_all(self.home.join(".claude")).unwrap();
        fs::write(self.settings(), text).unwrap();
    }

    fn hook(&self) -> String {
        self.bin.join("berth-hook").to_str().unwrap().to_owned()
    }

    fn files(&self, dir: &str) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.home.join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }
}

fn args(agent: Agent, flags: &[&str]) -> Args {
    Args {
        agent,
        statusline: flags.contains(&"statusline"),
        yes: flags.contains(&"yes"),
        undo: flags.contains(&"undo"),
        hook_path: None,
    }
}

const SETTINGS: &str = claude::tests::SETTINGS;

#[test]
fn preview_writes_nothing_then_yes_backs_up_and_writes() {
    let f = Fixture::new();
    f.write_settings(SETTINGS);
    let report = run(&args(Agent::Claude, &[]), &f.env(0)).unwrap();
    assert!(report.contains("只是预览，未写入"), "{report}");
    assert!(
        report.contains(&format!(
            "+            \"command\": \"'{}' claude\"",
            f.hook()
        )),
        "{report}"
    );
    assert!(report.contains(".bak.berth-20260921T"), "{report}");
    assert_eq!(fs::read_to_string(f.settings()).unwrap(), SETTINGS);
    assert_eq!(f.files(".claude"), ["settings.json"]);

    let report = run(&args(Agent::Claude, &["yes"]), &f.env(1)).unwrap();
    assert!(report.contains("已备份"), "{report}");
    let files = f.files(".claude");
    assert_eq!(files.len(), 2, "{files:?}");
    let backup = f.home.join(".claude").join(&files[1]);
    assert!(files[1].starts_with("settings.json.bak.berth-20260921T"));
    assert_eq!(fs::read_to_string(&backup).unwrap(), SETTINGS);
    assert_eq!(
        fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let written = fs::read_to_string(f.settings()).unwrap();
    assert_eq!(claude::installed_hook(&written).unwrap().path, f.hook());
    // Again: nothing to do, no second backup.
    let report = run(&args(Agent::Claude, &["yes"]), &f.env(2)).unwrap();
    assert!(report.contains("已是最新"), "{report}");
    assert_eq!(f.files(".claude").len(), 2);
    assert_eq!(fs::read_to_string(f.settings()).unwrap(), written);
}

#[test]
fn undo_restores_the_exact_bytes_and_keeps_a_copy() {
    let f = Fixture::new();
    f.write_settings(SETTINGS);
    run(&args(Agent::Claude, &["yes", "statusline"]), &f.env(0)).unwrap();
    let installed = fs::read_to_string(f.settings()).unwrap();
    // Preview first: unchanged.
    let report = run(&args(Agent::Claude, &["undo"]), &f.env(1)).unwrap();
    assert!(report.contains("逐字节恢复"), "{report}");
    assert_eq!(fs::read_to_string(f.settings()).unwrap(), installed);
    let report = run(&args(Agent::Claude, &["undo", "yes"]), &f.env(2)).unwrap();
    assert!(report.contains("已写入"), "{report}");
    assert_eq!(fs::read(f.settings()).unwrap(), SETTINGS.as_bytes());
    let undo_copy = f
        .files(".claude")
        .into_iter()
        .find(|n| n.contains(".bak.berth-undo-"))
        .expect("undo copy");
    assert_eq!(
        fs::read_to_string(f.home.join(".claude").join(undo_copy)).unwrap(),
        installed
    );
    // Nothing left to undo.
    let report = run(&args(Agent::Claude, &["undo", "yes"]), &f.env(3)).unwrap();
    assert!(report.contains("无需撤销"), "{report}");
    assert_eq!(fs::read(f.settings()).unwrap(), SETTINGS.as_bytes());
}

/// Two installs (the second adds the status line) are undone one step at a
/// time: the newest backup equal to the file is skipped on the second undo.
#[test]
fn undo_steps_back_through_successive_installs() {
    let f = Fixture::new();
    f.write_settings(SETTINGS);
    run(&args(Agent::Claude, &["yes"]), &f.env(0)).unwrap();
    let first = fs::read_to_string(f.settings()).unwrap();
    run(&args(Agent::Claude, &["yes", "statusline"]), &f.env(1)).unwrap();
    assert_ne!(fs::read_to_string(f.settings()).unwrap(), first);
    run(&args(Agent::Claude, &["undo", "yes"]), &f.env(2)).unwrap();
    assert_eq!(fs::read_to_string(f.settings()).unwrap(), first);
    let report = run(&args(Agent::Claude, &["undo", "yes"]), &f.env(3)).unwrap();
    assert!(report.contains("逐字节恢复"), "{report}");
    assert_eq!(fs::read(f.settings()).unwrap(), SETTINGS.as_bytes());
}

#[test]
fn undo_after_later_edits_only_removes_berth_entries() {
    let f = Fixture::new();
    f.write_settings(SETTINGS);
    run(&args(Agent::Claude, &["yes"]), &f.env(0)).unwrap();
    let installed = fs::read_to_string(f.settings()).unwrap();
    let edited = installed.replace("\"model\": \"opus\"", "\"model\": \"sonnet\"");
    fs::write(f.settings(), &edited).unwrap();
    let report = run(&args(Agent::Claude, &["undo", "yes"]), &f.env(1)).unwrap();
    assert!(report.contains("又被修改过"), "{report}");
    let after = fs::read_to_string(f.settings()).unwrap();
    assert_eq!(
        after,
        SETTINGS.replace("\"model\": \"opus\"", "\"model\": \"sonnet\"")
    );
}

#[test]
fn a_missing_file_is_created_and_undo_empties_it_again() {
    let f = Fixture::new();
    let report = run(&args(Agent::Claude, &["yes"]), &f.env(0)).unwrap();
    assert!(report.contains("将新建"), "{report}");
    assert_eq!(
        fs::metadata(f.settings()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(f.home.join(".claude"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    run(&args(Agent::Claude, &["undo", "yes"]), &f.env(1)).unwrap();
    assert_eq!(fs::read_to_string(f.settings()).unwrap(), "{}\n");
}

#[test]
fn codex_round_trip() {
    let f = Fixture::new();
    fs::create_dir_all(f.home.join(".codex")).unwrap();
    let config = f.home.join(".codex/config.toml");
    let original = "# mine\nmodel = \"m\"\nnotify = [\"notify-send\", \"Codex\"]\n";
    fs::write(&config, original).unwrap();
    let report = run(&args(Agent::Codex, &[]), &f.env(0)).unwrap();
    assert!(report.contains("--chain"), "{report}");
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    run(&args(Agent::Codex, &["yes"]), &f.env(1)).unwrap();
    let written = fs::read_to_string(&config).unwrap();
    assert!(written.contains(&format!(
        "notify = [\"{}\", \"codex\", \"--chain\", \"notify-send\", \"Codex\"]",
        f.hook()
    )));
    run(&args(Agent::Codex, &["undo", "yes"]), &f.env(2)).unwrap();
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    let err = run(&args(Agent::Codex, &["statusline"]), &f.env(3)).unwrap_err();
    assert!(err.to_string().contains("只用于 claude"));
}

#[test]
fn a_symlinked_settings_file_keeps_its_link_and_mode() {
    let f = Fixture::new();
    let dotfiles = f.home.join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    fs::write(dotfiles.join("settings.json"), SETTINGS).unwrap();
    fs::set_permissions(
        dotfiles.join("settings.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::create_dir_all(f.home.join(".claude")).unwrap();
    std::os::unix::fs::symlink(dotfiles.join("settings.json"), f.settings()).unwrap();
    run(&args(Agent::Claude, &["yes"]), &f.env(0)).unwrap();
    assert!(fs::symlink_metadata(f.settings())
        .unwrap()
        .file_type()
        .is_symlink());
    let target = dotfiles.join("settings.json");
    assert_ne!(fs::read_to_string(&target).unwrap(), SETTINGS);
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o644
    );
    // The backup sits next to the link, not in the dotfiles repository.
    assert_eq!(f.files("dotfiles"), ["settings.json"]);
    assert!(f.files(".claude").iter().any(|n| n.contains(".bak.berth-")));
}

#[test]
fn hook_path_must_be_an_executable() {
    let f = Fixture::new();
    f.write_settings(SETTINGS);
    fs::remove_file(f.bin.join("berth-hook")).unwrap();
    let err = run(&args(Agent::Claude, &["yes"]), &f.env(0)).unwrap_err();
    assert!(err.to_string().contains("找不到 berth-hook"), "{err}");
    let mut a = args(Agent::Claude, &["yes"]);
    a.hook_path = Some(PathBuf::from("hooks/berth-hook"));
    assert!(run(&a, &f.env(0))
        .unwrap_err()
        .to_string()
        .contains("--hook-path"));
    fs::create_dir_all(f.home.join("hooks")).unwrap();
    fs::write(f.home.join("hooks/berth-hook"), "").unwrap();
    fs::set_permissions(
        f.home.join("hooks/berth-hook"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    run(&a, &f.env(0)).unwrap();
    let written = fs::read_to_string(f.settings()).unwrap();
    let want = f.home.join("hooks/berth-hook");
    assert_eq!(
        claude::installed_hook(&written).unwrap().path,
        want.to_str().unwrap()
    );
    assert_eq!(f.files(".claude").len(), 2, "one backup, nothing else");
}

#[test]
fn a_file_changed_after_the_preview_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("settings.json");
    fs::write(&file, "{\"a\": 1}").unwrap();
    let err = write_file(&file, Some(b"{\"a\": 0}"), b"{}").unwrap_err();
    assert!(err.to_string().contains("被其他程序修改"), "{err}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "{\"a\": 1}");
    let names: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(names.len(), 1, "temporary file removed");
    write_file(&file, Some(b"{\"a\": 1}"), b"{}").unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "{}");
}

#[test]
fn preview_hides_secret_looking_values() {
    let f = Fixture::new();
    let text = "{\n  \"env\": {\n    \"ANTHROPIC_API_KEY\": \"sk-ant-SECRET1\",\n    \"X\": \"TOKEN=SECRET2 y\"\n  },\n  \"inline\": {\"GITHUB_TOKEN\": \"SECRET3\"}\n}\n";
    f.write_settings(text);
    let report = run(&args(Agent::Claude, &[]), &f.env(0)).unwrap();
    for secret in ["SECRET1", "SECRET2", "SECRET3"] {
        assert!(!report.contains(secret), "{secret} shown:\n{report}");
    }
    assert!(report.contains("已隐藏"), "{report}");
    assert!(
        report.contains("\"SessionStart\": ["),
        "event lines stay readable"
    );
    // Only the preview is masked.
    run(&args(Agent::Claude, &["yes"]), &f.env(1)).unwrap();
    assert!(fs::read_to_string(f.settings())
        .unwrap()
        .contains("sk-ant-SECRET1"));
    let toml_line = mask("env = { DOCS_TOKEN = \"abc\" }");
    assert_eq!(toml_line, "env = { DOCS_TOKEN = \"***\" }");
    assert_eq!(mask("api_key = \"abc\""), "api_key = \"***\"");
}

#[test]
fn backups_sort_newest_first_and_timestamps_are_utc() {
    assert_eq!(timestamp(SystemTime::UNIX_EPOCH), "19700101T000000Z");
    assert_eq!(
        timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_458_767)),
        "20260926T213927Z"
    );
    assert_eq!(
        timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)),
        "20000229T000000Z"
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    for name in [
        "config.toml.bak.berth-20260101T000000Z",
        "config.toml.bak.berth-20260101T000000Z-2",
        "config.toml.bak.berth-20260101T000000Z-10",
        "config.toml.bak.berth-20250101T000000Z",
        "config.toml.bak.berth-undo-20270101T000000Z",
        "config.toml.bak.berth-garbage",
    ] {
        fs::write(dir.path().join(name), "").unwrap();
    }
    let names: Vec<String> = backups(&file)
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "config.toml.bak.berth-20260101T000000Z-10",
            "config.toml.bak.berth-20260101T000000Z-2",
            "config.toml.bak.berth-20260101T000000Z",
            "config.toml.bak.berth-20250101T000000Z",
        ]
    );
}

#[test]
fn config_dir_overrides_are_warned_about() {
    let f = Fixture::new();
    let mut env = f.env(0);
    env.claude_config_dir = true;
    let report = run(&args(Agent::Claude, &[]), &env).unwrap();
    assert!(report.starts_with("注意：CLAUDE_CONFIG_DIR"), "{report}");
}
