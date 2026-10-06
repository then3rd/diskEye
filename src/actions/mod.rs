//! Guided cleanup. Everything is read-only unless the user explicitly runs an
//! action; each action can be previewed, and every execution is logged.

use crate::model::{ActionSpec, ActionStep, Risk};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The word that confirms a batch containing `danger` items, or any batch when
/// running as root. Ordinary batches only need `yes`.
pub const STRICT_WORD: &str = "delete";

/// What the user has to type to run a batch.
pub fn confirm_word(strict: bool) -> &'static str {
    if strict { STRICT_WORD } else { "yes" }
}

/// Whether `input` confirms a batch. The strict word is always accepted.
pub fn confirmed(input: &str, strict: bool) -> bool {
    let t = input.trim();
    t.eq_ignore_ascii_case(STRICT_WORD) || (!strict && (t.eq_ignore_ascii_case("yes") || t.eq_ignore_ascii_case("y")))
}

/// Paths that must never be deleted, whatever a rule or provider says.
const PROTECTED: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib64",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/srv",
    "/sys",
    "/tmp",
    "/usr",
    "/var",
    "/var/lib",
    "/var/cache",
    "/var/log",
    "/mnt",
    "/media",
];

pub fn is_protected(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    let p = if p.is_empty() { "/" } else { p };
    if PROTECTED.contains(&p) || p.split('/').filter(|c| !c.is_empty()).count() < 2 {
        return true;
    }
    // Home directories themselves.
    crate::util::human_users().iter().any(|(_, _, h)| h.as_os_str() == p)
}

/// Human-readable lines describing exactly what the action will do.
pub fn describe(spec: &ActionSpec) -> Vec<String> {
    spec.steps
        .iter()
        .map(|s| match s {
            ActionStep::Command { argv, root } => {
                format!("{}run: {}", if *root { "[root] " } else { "" }, shell_quote(argv))
            }
            ActionStep::DeletePath { path, trash: true } => format!("move to trash: {path}"),
            ActionStep::DeletePath { path, trash: false } => format!("delete permanently: {path}"),
            ActionStep::EmptyDir { path } => format!("delete contents of: {path}/*"),
            ActionStep::DockerApi { socket, method, path } => format!("docker API {method} {path} (via {socket})"),
        })
        .collect()
}

pub fn shell_quote(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c)) && !a.is_empty() {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Check an action can run as the current user, without running it.
pub fn preflight(spec: &ActionSpec) -> Result<()> {
    for s in &spec.steps {
        match s {
            ActionStep::Command { argv, root } => {
                if argv.is_empty() {
                    bail!("empty command");
                }
                if *root && !crate::util::is_root() {
                    bail!("`{}` needs root; re-run with sudo", shell_quote(argv));
                }
                if crate::providers::runner::which(&argv[0]).is_none() {
                    bail!("`{}` is not installed", argv[0]);
                }
            }
            ActionStep::DeletePath { path, .. } | ActionStep::EmptyDir { path } => {
                if !path.starts_with('/') {
                    bail!("refusing relative path {path}");
                }
                if is_protected(path) {
                    bail!("refusing to touch protected path {path}");
                }
                if std::fs::symlink_metadata(path).is_err() {
                    bail!("{path} no longer exists");
                }
            }
            ActionStep::DockerApi { socket, .. } => {
                if !Path::new(socket).exists() {
                    bail!("docker socket {socket} not found");
                }
            }
        }
    }
    Ok(())
}

pub fn execute(spec: &ActionSpec, risk: Risk, bytes: u64) -> Result<()> {
    preflight(spec)?;
    let mut log = audit_log();
    let _ = writeln!(
        log,
        "{} START {} [{}] ~{} bytes",
        crate::util::timestamp_human(crate::util::now_secs()),
        spec.label,
        risk.label(),
        bytes
    );
    for (step, line) in spec.steps.iter().zip(describe(spec)) {
        let _ = writeln!(log, "  {line}");
        let res = run_step(step);
        if let Err(e) = &res {
            let _ = writeln!(log, "  FAILED: {e:#}");
        }
        res?;
    }
    let _ = writeln!(log, "  DONE");
    Ok(())
}

fn run_step(step: &ActionStep) -> Result<()> {
    match step {
        ActionStep::Command { argv, .. } => {
            let status = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .status()
                .with_context(|| format!("running {}", argv[0]))?;
            if !status.success() {
                bail!("`{}` exited with {status}", shell_quote(argv));
            }
            Ok(())
        }
        ActionStep::DeletePath { path, trash } => {
            if *trash {
                move_to_trash(Path::new(path))
            } else {
                let md = std::fs::symlink_metadata(path)?;
                if md.is_dir() {
                    std::fs::remove_dir_all(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
                Ok(())
            }
        }
        ActionStep::EmptyDir { path } => {
            for e in std::fs::read_dir(path)? {
                let e = e?;
                let ft = e.file_type()?;
                if ft.is_dir() {
                    std::fs::remove_dir_all(e.path())?;
                } else {
                    std::fs::remove_file(e.path())?;
                }
            }
            Ok(())
        }
        ActionStep::DockerApi { socket, method, path } => {
            let (status, body) = crate::providers::docker::api_request(socket, method, path)?;
            if !(200..300).contains(&status) {
                bail!("docker API {method} {path} returned {status}: {}", body.trim());
            }
            Ok(())
        }
    }
}

/// Freedesktop trash: same-filesystem rename into ~/.local/share/Trash, or
/// `$topdir/.Trash-$uid` on other filesystems. Falls back to refusing rather
/// than copying gigabytes across filesystems.
pub fn move_to_trash(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let (uid, _) = crate::util::invoking_ids();
    let home_trash = crate::util::invoking_home().join(".local/share/Trash");
    let dev = std::fs::symlink_metadata(path)?.dev();
    let trash = if std::fs::metadata(crate::util::invoking_home()).map(|m| m.dev()).ok() == Some(dev) {
        home_trash
    } else {
        let mounts = crate::scan::mounts::read_mountinfo();
        let m = crate::scan::mounts::mount_for(&mounts, &path.to_string_lossy()).context("no mount for path")?;
        PathBuf::from(&m.mount_point).join(format!(".Trash-{uid}"))
    };
    let files = trash.join("files");
    let info = trash.join("info");
    std::fs::create_dir_all(&files)?;
    std::fs::create_dir_all(&info)?;
    let base = path.file_name().context("path has no file name")?.to_string_lossy().into_owned();
    let mut name = base.clone();
    let mut i = 1;
    while files.join(&name).exists() || info.join(format!("{name}.trashinfo")).exists() {
        i += 1;
        name = format!("{base}.{i}");
    }
    let ts = crate::util::timestamp_human(crate::util::now_secs());
    let date = ts.replace(' ', "T");
    std::fs::write(
        info.join(format!("{name}.trashinfo")),
        format!("[Trash Info]\nPath={}\nDeletionDate={date}:00\n", path.display()),
    )?;
    std::fs::rename(path, files.join(&name))
        .with_context(|| format!("moving {} to {}", path.display(), trash.display()))?;
    for p in [&trash, &files, &info] {
        crate::util::chown_to_invoker(p);
    }
    Ok(())
}

fn audit_log() -> Box<dyn Write> {
    let dir = crate::util::invoking_home().join(".local/state/diskeye");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("actions.log");
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => {
            crate::util::chown_to_invoker(&path);
            Box::new(f)
        }
        Err(_) => Box::new(std::io::sink()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_system_paths() {
        assert!(is_protected("/"));
        assert!(is_protected("/usr/"));
        assert!(is_protected("/var/lib"));
        assert!(is_protected("/foo"));
        assert!(!is_protected("/var/cache/pacman/pkg"));
    }

    #[test]
    fn quotes() {
        assert_eq!(shell_quote(&["echo".into(), "a b".into(), "it's".into()]), r"echo 'a b' 'it'\''s'");
    }

    #[test]
    fn preflight_rejects_protected() {
        let spec =
            ActionSpec { label: "x".into(), steps: vec![ActionStep::DeletePath { path: "/usr".into(), trash: false }] };
        assert!(preflight(&spec).is_err());
    }

    #[test]
    fn empty_dir_step() {
        let td = tempfile::tempdir().unwrap();
        let d = td.path().join("cache");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("f"), b"1").unwrap();
        let spec =
            ActionSpec { label: "x".into(), steps: vec![ActionStep::EmptyDir { path: d.display().to_string() }] };
        execute_without_log(&spec).unwrap();
        assert!(d.exists());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    }

    fn execute_without_log(spec: &ActionSpec) -> Result<()> {
        preflight(spec)?;
        for s in &spec.steps {
            run_step(s)?;
        }
        Ok(())
    }
}
