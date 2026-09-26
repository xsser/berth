//! zsh shell integration (DESIGN §7): interactive zsh sessions start with
//! `ZDOTDIR` pointing at a copy of `resources/shell-integration/zsh` in the
//! data directory. Its `.zshenv` puts the user's `ZDOTDIR` back (kept in
//! `BERTH_ORIG_ZDOTDIR`; empty = it was unset), sources the user's
//! `.zshenv` and, in interactive shells, loads `berth-integration.zsh`,
//! which only prints OSC 133 A/B/C/D and OSC 7. zsh then reads the user's
//! `.zprofile` / `.zshrc` / `.zlogin` from their usual place itself.
//!
//! Only zsh is covered: bash (`--rcfile`) and fish (`XDG_DATA_DIRS`) are not
//! implemented. A shell started later inside the session (`exec zsh`, a
//! nested `zsh`) sees the user's own `ZDOTDIR` and runs without it.

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

const ZSHENV: &str = include_str!("../resources/shell-integration/zsh/.zshenv");
const ZSH_INTEGRATION: &str =
    include_str!("../resources/shell-integration/zsh/berth-integration.zsh");
const ZSH_FILES: [(&str, &str); 2] = [
    (".zshenv", ZSHENV),
    ("berth-integration.zsh", ZSH_INTEGRATION),
];

/// Serialises installs from concurrent spawns (one daemon per data dir).
static INSTALL: Mutex<()> = Mutex::new(());

/// `<data_dir>/shell-integration/zsh`.
pub fn zsh_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("shell-integration").join("zsh")
}

/// Whether the zsh integration files in `data_dir` are this daemon's
/// (read-only; `berth doctor` reports it).
pub fn zsh_installed(data_dir: &Path) -> bool {
    let dir = zsh_dir(data_dir);
    ZSH_FILES
        .iter()
        .all(|(name, text)| std::fs::read(dir.join(name)).is_ok_and(|b| b == text.as_bytes()))
}

/// Make `zsh_dir` hold exactly this daemon's files (directories `0700`,
/// files `0600`, replaced atomically when missing or different) and return
/// it. Checked on every spawn, so an edited or deleted copy is repaired.
pub fn install_zsh(data_dir: &Path) -> io::Result<PathBuf> {
    let _guard = INSTALL.lock();
    let dir = zsh_dir(data_dir);
    for d in [dir.parent().unwrap_or(&dir), dir.as_path()] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(d)?;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
    }
    for (name, text) in ZSH_FILES {
        let path = dir.join(name);
        if std::fs::read(&path).is_ok_and(|b| b == text.as_bytes()) {
            continue;
        }
        let tmp = dir.join(format!("{name}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    Ok(dir)
}

/// The argv to spawn when `command` runs an interactive zsh, else `None`.
/// An empty command (the login shell) is resolved here, as berth-vt would
/// resolve it, and returned explicitly so the program that gets the
/// integration is the one that was checked. An explicit command qualifies
/// when its program is named `zsh` and its only options are `-l` / `-i`
/// (`--login`, `--interactive`, `-il`): `-c`, `-f`, a script, ... do not.
pub fn interactive_zsh(command: &[String], spawn_env: &[(String, String)]) -> Option<Vec<String>> {
    match command.split_first() {
        None => {
            let shell = login_shell(spawn_env);
            let shell = shell.to_str()?;
            is_zsh(shell).then(|| vec![shell.to_owned(), "-l".to_owned()])
        }
        Some((program, args)) => {
            let plain = args.iter().all(|a| {
                matches!(a.as_str(), "--login" | "--interactive")
                    || a.strip_prefix('-').is_some_and(|flags| {
                        !flags.is_empty() && flags.chars().all(|c| c == 'l' || c == 'i')
                    })
            });
            (is_zsh(program) && plain).then(|| command.to_vec())
        }
    }
}

fn is_zsh(program: &str) -> bool {
    let base = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);
    base.trim_start_matches('-') == "zsh"
}

/// Mirrors berth-vt's login shell choice for an empty command: `SHELL` from
/// the spawn env, else the daemon's `$SHELL`, else `/bin/zsh`, else
/// `/bin/sh`, the first that is an absolute path to an executable file.
fn login_shell(spawn_env: &[(String, String)]) -> PathBuf {
    let from_spec = spawn_env
        .iter()
        .rev()
        .find(|(key, _)| key == "SHELL")
        .map(|(_, value)| PathBuf::from(value));
    let from_daemon = std::env::var_os("SHELL").map(PathBuf::from);
    from_spec
        .into_iter()
        .chain(from_daemon)
        .chain(["/bin/zsh", "/bin/sh"].map(PathBuf::from))
        .find(|path| {
            path.is_absolute()
                && path
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// `ZDOTDIR` / `BERTH_ORIG_ZDOTDIR` for a shell using the integration in
/// `dir`. The user's `ZDOTDIR` is the one the shell would have inherited:
/// from the spawn env, else the daemon's (unless that is `dir` itself, when
/// the daemon was started from an integrated shell: then its
/// `BERTH_ORIG_ZDOTDIR`). `None` when that value cannot be passed on (not
/// UTF-8): the shell then starts without the integration.
pub fn zsh_env(dir: &Path, spawn_env: &[(String, String)]) -> Option<[(String, String); 2]> {
    let own = dir.to_str()?.to_owned();
    let inherited = match spawn_env.iter().rev().find(|(k, _)| k == "ZDOTDIR") {
        Some((_, v)) => Some(v.clone()),
        None => match std::env::var_os("ZDOTDIR") {
            Some(v) => Some(v.into_string().ok()?),
            None => None,
        },
    };
    let mut original = match inherited {
        Some(v) if Path::new(&v) == dir => match std::env::var_os("BERTH_ORIG_ZDOTDIR") {
            Some(v) => v.into_string().ok()?,
            None => String::new(),
        },
        Some(v) => v,
        None => String::new(),
    };
    if Path::new(&original) == dir {
        original.clear();
    }
    Some([
        ("ZDOTDIR".to_owned(), own),
        ("BERTH_ORIG_ZDOTDIR".to_owned(), original),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn fake_program(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_owned()
    }

    #[test]
    fn only_interactive_zsh_gets_the_integration() {
        let dir = tempfile::tempdir().unwrap();
        let zsh = fake_program(dir.path(), "zsh");
        let bash = fake_program(dir.path(), "bash");
        let env = |shell: &str| vec![("SHELL".to_owned(), shell.to_owned())];
        // The login shell is resolved and made explicit.
        assert_eq!(interactive_zsh(&[], &env(&zsh)), Some(v(&[&zsh, "-l"])));
        assert_eq!(interactive_zsh(&[], &env(&bash)), None);
        for ok in [
            v(&["/bin/zsh"]),
            v(&["zsh", "-l"]),
            v(&["/opt/homebrew/bin/zsh", "--login", "-i"]),
            v(&["zsh", "-il"]),
        ] {
            assert_eq!(interactive_zsh(&ok, &[]), Some(ok.clone()), "{ok:?}");
        }
        for no in [
            v(&["/bin/zsh", "-c", "echo hi"]),
            v(&["/bin/zsh", "-f"]),
            v(&["/bin/zsh", "script.zsh"]),
            v(&["/bin/zsh", "-"]),
            v(&["/bin/bash", "-l"]),
            v(&["claude"]),
            v(&["/usr/bin/env", "zsh"]),
        ] {
            assert_eq!(interactive_zsh(&no, &[]), None, "{no:?}");
        }
    }

    #[test]
    fn install_writes_private_files_and_repairs_them() {
        let data = tempfile::tempdir().unwrap();
        assert!(!zsh_installed(data.path()));
        let dir = install_zsh(data.path()).unwrap();
        assert_eq!(dir, zsh_dir(data.path()));
        assert!(zsh_installed(data.path()));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(dir.parent().unwrap()), 0o700);
        for (name, text) in ZSH_FILES {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), text);
            assert_eq!(mode(&dir.join(name)), 0o600);
        }
        // Edited or deleted copies are put back on the next spawn.
        std::fs::write(dir.join(".zshenv"), "echo tampered\n").unwrap();
        std::fs::remove_file(dir.join("berth-integration.zsh")).unwrap();
        assert!(!zsh_installed(data.path()));
        install_zsh(data.path()).unwrap();
        assert!(zsh_installed(data.path()));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "no temporary files left: {names:?}");
    }

    #[test]
    fn zsh_env_keeps_the_users_zdotdir() {
        let dir = Path::new("/data/shell-integration/zsh");
        let get = |env: &[(String, String)]| zsh_env(dir, env).unwrap();
        let spawn = vec![("ZDOTDIR".to_owned(), "/home/u/.config/zsh".to_owned())];
        let [(k1, v1), (k2, v2)] = get(&spawn);
        assert_eq!(
            (k1.as_str(), v1.as_str()),
            ("ZDOTDIR", "/data/shell-integration/zsh")
        );
        assert_eq!(
            (k2.as_str(), v2.as_str()),
            ("BERTH_ORIG_ZDOTDIR", "/home/u/.config/zsh")
        );
        // Inherited ZDOTDIR already ours: never point the user at it.
        let ours = vec![("ZDOTDIR".to_owned(), dir.to_str().unwrap().to_owned())];
        assert_ne!(get(&ours)[1].1, dir.to_str().unwrap());
    }

    /// The resource files are valid zsh (when zsh is installed).
    #[test]
    fn resource_files_parse() {
        if !Path::new("/bin/zsh").exists() {
            eprintln!("skipped: /bin/zsh not found");
            return;
        }
        let data = tempfile::tempdir().unwrap();
        let dir = install_zsh(data.path()).unwrap();
        for (name, _) in ZSH_FILES {
            let status = std::process::Command::new("/bin/zsh")
                .arg("-n")
                .arg(dir.join(name))
                .status()
                .unwrap();
            assert!(status.success(), "{name} does not parse");
        }
    }
}
