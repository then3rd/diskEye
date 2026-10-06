//! Snapshot persistence: `DKEYE` magic + version + zstd(postcard(Snapshot)).

use super::{SNAPSHOT_VERSION, Snapshot};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 5] = b"DKEYE";

pub fn save(snap: &Snapshot, path: &Path) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut w = std::io::BufWriter::new(file);
    w.write_all(MAGIC)?;
    w.write_all(&SNAPSHOT_VERSION.to_le_bytes())?;
    let mut enc = zstd::Encoder::new(w, 3)?;
    let bytes = postcard::to_stdvec(snap).context("serializing snapshot")?;
    enc.write_all(&bytes)?;
    enc.finish()?.flush()?;
    std::fs::rename(&tmp, path)?;
    for p in path.ancestors().take(4) {
        crate::util::chown_to_invoker(p);
    }
    Ok(())
}

pub fn load(path: &Path) -> Result<Snapshot> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut r = std::io::BufReader::new(file);
    let mut magic = [0u8; 5];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("{} is not a diskeye snapshot", path.display());
    }
    let mut ver = [0u8; 4];
    r.read_exact(&mut ver)?;
    let ver = u32::from_le_bytes(ver);
    if ver != SNAPSHOT_VERSION {
        bail!("snapshot version {ver} unsupported (expected {SNAPSHOT_VERSION})");
    }
    let mut bytes = Vec::new();
    zstd::Decoder::new(r)?.read_to_end(&mut bytes)?;
    postcard::from_bytes(&bytes).context("decoding snapshot")
}

/// `$XDG_STATE_HOME/diskeye/snapshots` (falls back to `~/.local/state`). Under sudo
/// we still use the invoking user's home so snapshots stay in one place.
pub fn default_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_STATE_HOME")
        && !d.is_empty()
        && std::env::var_os("SUDO_USER").is_none()
    {
        return PathBuf::from(d).join("diskeye/snapshots");
    }
    crate::util::invoking_home().join(".local/state/diskeye/snapshots")
}

pub fn default_path_for_now(host: &str) -> PathBuf {
    let ts = crate::util::timestamp_compact(crate::util::now_secs());
    default_dir().join(format!("{host}-{ts}.dkeye"))
}

/// Snapshots in the default directory, oldest first.
pub fn list() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(default_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "dkeye"))
        .collect();
    v.sort();
    v
}

/// Delete all but the newest `keep` snapshots in the default directory.
pub fn prune(keep: usize) -> Vec<PathBuf> {
    let all = list();
    let n = all.len().saturating_sub(keep);
    all.into_iter().take(n).filter(|p| std::fs::remove_file(p).is_ok()).collect()
}

pub fn latest() -> Option<PathBuf> {
    list().pop()
}
