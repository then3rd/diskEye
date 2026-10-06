//! Providers enrich a scanned snapshot: the physical block layer, and
//! workload entities (containers, VMs, caches...) that own scanned paths.

pub mod block;
pub mod classifier;
pub mod containerd;
pub mod docker;
pub mod flatpak_snap;
pub mod journald;
pub mod kube;
pub mod libvirt;
pub mod lvm;
pub mod podman;
pub mod runner;
pub mod vbox_vagrant;

use crate::model::{Coverage, Entity, ProviderReport, Snapshot};
use crate::scan::mounts::Mount;
use runner::CommandRunner;
use std::path::PathBuf;
use std::time::Instant;

pub struct Ctx<'a> {
    pub runner: &'a dyn CommandRunner,
    pub is_root: bool,
    /// The human user (SUDO_UID under sudo).
    pub uid: u32,
    pub home: PathBuf,
    /// Mount topology, for providers that need to map paths to filesystems.
    #[allow(dead_code)]
    pub mounts: &'a [Mount],
    /// Users whose per-user data (rootless docker, podman, caches) to inspect.
    pub users: Vec<(u32, String, PathBuf)>,
}

impl Ctx<'_> {
    pub fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub coverage: Coverage,
    pub notes: Vec<String>,
}

impl Outcome {
    pub fn absent() -> Self {
        Outcome { coverage: Coverage::Absent, notes: vec![] }
    }
    pub fn complete() -> Self {
        Outcome { coverage: Coverage::Complete, notes: vec![] }
    }
    pub fn denied(note: impl Into<String>) -> Self {
        Outcome { coverage: Coverage::Denied, notes: vec![note.into()] }
    }
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
    /// Downgrade Complete to Partial, keeping worse states.
    pub fn degrade(&mut self, note: impl Into<String>) {
        if self.coverage == Coverage::Complete {
            self.coverage = Coverage::Partial;
        }
        self.notes.push(note.into());
    }
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome;
}

impl Snapshot {
    /// Append an entity, returning its id.
    pub fn add_entity(&mut self, mut e: Entity) -> u32 {
        let id = self.entities.len() as u32;
        e.id = id;
        self.entities.push(e);
        id
    }
}

pub fn all() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(block::Block),
        Box::new(lvm::Lvm),
        Box::new(docker::Docker),
        Box::new(podman::Podman),
        Box::new(containerd::Containerd),
        Box::new(kube::Kube),
        Box::new(libvirt::Libvirt),
        Box::new(vbox_vagrant::VboxVagrant),
        Box::new(flatpak_snap::FlatpakSnap),
        Box::new(journald::Journald),
        // Last: generic path rules must not shadow specific owners.
        Box::new(classifier::Classifier::builtin()),
    ]
}

/// Run providers in order, recording how complete each one's view was.
pub fn run(providers: &[Box<dyn Provider>], ctx: &Ctx, snap: &mut Snapshot, only: Option<&[String]>, quiet: bool) {
    for p in providers {
        if only.is_some_and(|o| !o.iter().any(|n| n == p.name())) {
            continue;
        }
        if !quiet && std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            eprint!("\r\x1b[2Kprovider: {}", p.name());
        }
        let start = Instant::now();
        let out = p.collect(ctx, snap);
        snap.providers.push(ProviderReport {
            name: p.name().to_string(),
            coverage: out.coverage,
            notes: out.notes,
            duration_ms: start.elapsed().as_millis() as u64,
        });
    }
    if !quiet && std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        eprint!("\r\x1b[2K");
    }
}

/// Parse a size like "1.5G", "12 MB", "530.2kB", "1024" into bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let idx = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let n: f64 = num.parse().ok()?;
    let unit = unit.trim().trim_end_matches('B').trim_end_matches('b').to_ascii_lowercase();
    let unit = unit.trim_end_matches('i');
    let mult: f64 = match unit {
        "" => 1.0,
        "k" => 1024.0,
        "m" => 1024.0 * 1024.0,
        "g" => 1024.0 * 1024.0 * 1024.0,
        "t" => 1024.0f64.powi(4),
        "p" => 1024.0f64.powi(5),
        _ => return None,
    };
    Some((n * mult) as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert_eq!(super::parse_size("1024"), Some(1024));
        assert_eq!(super::parse_size("1.5G"), Some(1610612736));
        assert_eq!(super::parse_size("12 MB"), Some(12 << 20));
        assert_eq!(super::parse_size("4.0 KiB"), Some(4096));
        assert_eq!(super::parse_size("x"), None);
    }
}
