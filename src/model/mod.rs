//! The storage graph: one snapshot holds the scanned file tree, the physical
//! block layer, filesystems, and every workload entity that owns parts of them.

pub mod attribution;
pub mod diff;
pub mod ncdu;
pub mod snapshot;
pub mod tree;

use serde::{Deserialize, Serialize};

pub use tree::{FileTree, NONE, NodeId};

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub meta: ScanMeta,
    pub tree: FileTree,
    pub filesystems: Vec<FsInfo>,
    pub block: Vec<BlockDev>,
    pub lvm: LvmInfo,
    pub swaps: Vec<Swap>,
    pub entities: Vec<Entity>,
    /// (node, entity) pairs: entity `e` owns the subtree rooted at `node`.
    pub claims: Vec<Claim>,
    pub deleted_open: Vec<DeletedOpen>,
    pub hidden: Vec<HiddenUnderMount>,
    pub providers: Vec<ProviderReport>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanMeta {
    pub host: String,
    pub diskeye_version: String,
    /// Unix seconds.
    pub started: i64,
    pub duration_ms: u64,
    pub euid: u32,
    /// The user who invoked us via sudo, if any.
    pub sudo_user: Option<String>,
    pub roots: Vec<String>,
    pub kernel: String,
}

impl ScanMeta {
    pub fn is_root(&self) -> bool {
        self.euid == 0
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatVfs {
    pub total: u64,
    pub free: u64,
    pub avail: u64,
    pub files: u64,
    pub files_free: u64,
    pub bsize: u64,
}

impl StatVfs {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
    /// Blocks reserved for root (ext4 `-m`), computed without privileges.
    pub fn reserved(&self) -> u64 {
        self.free.saturating_sub(self.avail)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsInfo {
    pub mount_point: String,
    /// Directory the scan started from (usually `mount_point`).
    pub scan_root: String,
    /// Other places the same filesystem (or part of it) is mounted:
    /// (alias mountpoint, equivalent path under `mount_point`).
    pub aliases: Vec<(String, String)>,
    pub source: String,
    pub fstype: String,
    pub options: String,
    /// Path inside the filesystem that is mounted (btrfs subvol, bind mounts).
    pub fs_root: String,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub statvfs: Option<StatVfs>,
    pub root_node: Option<NodeId>,
    pub scanned_alloc: u64,
    pub scanned_apparent: u64,
    pub scanned_items: u64,
    pub denied_dirs: u64,
    pub errors: u64,
    pub skipped_reason: Option<String>,
}

impl FsInfo {
    pub fn dev(&self) -> u64 {
        libc::makedev(self.dev_major, self.dev_minor)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockDev {
    /// Kernel name, e.g. `nvme1n1p2`, `dm-3`.
    pub kname: String,
    /// Display name, e.g. `vg_system-lv_root`.
    pub name: String,
    pub path: String,
    /// lsblk TYPE: disk, part, lvm, crypt, loop, raid1, rom, ...
    pub dtype: String,
    pub size: u64,
    pub fstype: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    /// Partition type name, e.g. "EFI System", "Microsoft reserved".
    pub part_type: Option<String>,
    pub mountpoints: Vec<String>,
    pub model: Option<String>,
    pub tran: Option<String>,
    pub ro: bool,
    pub removable: bool,
    /// Kernel names of the devices this one sits on.
    pub parents: Vec<String>,
    pub fs_avail: Option<u64>,
    pub fs_used: Option<u64>,
    pub loop_backing_file: Option<String>,
    /// Who uses this device when nothing is mounted: "swap", "VM win11", "PV of vg_system"...
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LvmInfo {
    /// "lvm2" when read with lvs/vgs/pvs, "sysfs" when inferred without root.
    pub source: String,
    pub vgs: Vec<Vg>,
    pub lvs: Vec<Lv>,
    pub pvs: Vec<Pv>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Vg {
    pub name: String,
    pub size: u64,
    pub free: u64,
    pub free_is_estimate: bool,
    pub extent_size: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lv {
    pub vg: String,
    pub name: String,
    pub size: u64,
    pub attr: Option<String>,
    pub segtype: Option<String>,
    pub pool: Option<String>,
    pub origin: Option<String>,
    pub data_percent: Option<f64>,
    pub kname: Option<String>,
    pub mountpoints: Vec<String>,
    pub fstype: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pv {
    pub name: String,
    pub vg: String,
    pub size: u64,
    pub free: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Swap {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub used: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Risk {
    /// Regenerated automatically (caches, dangling images).
    Safe,
    /// Probably unneeded, but look first (unused volumes, old snapshots, orphan disks).
    Review,
    /// Data loss if wrong (VM disks, volumes in use, LVs).
    Danger,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::Safe => "safe",
            Risk::Review => "review",
            Risk::Danger => "danger",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ActionStep {
    /// Run a command. `root` means it must run with euid 0.
    Command { argv: Vec<String>, root: bool },
    /// Remove a path. With `trash` it is moved to the invoking user's Trash.
    DeletePath { path: String, trash: bool },
    /// Remove the *contents* of a directory, keeping the directory itself.
    EmptyDir { path: String },
    /// Call the Docker Engine API.
    DockerApi { socket: String, method: String, path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    pub label: String,
    pub steps: Vec<ActionStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reclaim {
    pub risk: Risk,
    pub reason: String,
    /// Bytes we expect to free, if different from the entity's measured unique size.
    pub estimate: Option<u64>,
    pub action: Option<ActionSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entity {
    pub id: u32,
    /// Machine-readable kind: "docker.image", "docker.volume", "vm", "vm.disk", "cache", ...
    pub kind: String,
    pub name: String,
    pub provider: String,
    /// Top-level grouping in the Workloads view, e.g. "Docker (rootless, alice)".
    pub group: String,
    pub parent: Option<u32>,
    /// Absolute paths this entity owns on scanned filesystems.
    pub paths: Vec<String>,
    /// Block devices (kname) this entity owns outright (e.g. an LV used as a VM disk).
    pub block_devs: Vec<String>,
    /// Entities sharing the same key are checked for shared paths (e.g. image layers).
    pub share_key: Option<String>,
    pub attrs: Vec<(String, String)>,
    /// Size the owning tool reports (cross-check).
    pub reported: Option<u64>,
    /// Logical size (VM virtual disk size, sparse file size).
    pub virtual_size: Option<u64>,
    /// Bytes outside scanned filesystems (raw LVs, unscanned paths).
    pub external_bytes: u64,
    /// Filled in by attribution.
    pub measured_alloc: u64,
    pub measured_apparent: u64,
    pub measured_unique: u64,
    pub unresolved_paths: Vec<String>,
    pub reclaim: Option<Reclaim>,
}

impl Entity {
    #[cfg(test)]
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Bytes this entity occupies on disk, including bytes that are only shared.
    pub fn total_bytes(&self) -> u64 {
        self.measured_alloc + self.external_bytes
    }

    /// Bytes freed if only this entity were removed.
    pub fn unique_bytes(&self) -> u64 {
        self.measured_unique + self.external_bytes
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Claim {
    pub node: NodeId,
    pub entity: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeletedOpen {
    pub pid: i32,
    pub comm: String,
    pub fd: i32,
    pub path: String,
    pub dev: u64,
    pub ino: u64,
    pub apparent: u64,
    pub alloc: u64,
    /// Index into `Snapshot::filesystems`.
    pub fs: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HiddenUnderMount {
    /// Index into `Snapshot::filesystems` of the filesystem holding the hidden files.
    pub fs: usize,
    /// The mountpoint covering them.
    pub path: String,
    pub alloc: u64,
    pub apparent: u64,
    pub items: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Coverage {
    Complete,
    /// Ran, but some data needs root or was unreadable.
    Partial,
    /// Detected but could not be queried at all (permissions, daemon down).
    Denied,
    /// Not present on this system.
    Absent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderReport {
    pub name: String,
    pub coverage: Coverage,
    pub notes: Vec<String>,
    pub duration_ms: u64,
}

/// Per-filesystem breakdown that always sums to `statvfs.used()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reconcile {
    pub fs: usize,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub reserved: u64,
    pub scanned: u64,
    pub deleted_open: u64,
    pub hidden: u64,
    /// used - scanned - deleted_open - hidden. Filesystem metadata/journal,
    /// unreadable directories, or files changed during the scan. Can be negative.
    pub unaccounted: i64,
    pub denied_dirs: u64,
}

impl Snapshot {
    pub fn reconcile(&self) -> Vec<Reconcile> {
        self.filesystems
            .iter()
            .enumerate()
            .filter_map(|(i, fs)| {
                let sv = fs.statvfs?;
                fs.root_node?;
                if fs.scan_root != fs.mount_point {
                    return None;
                }
                let deleted_open: u64 = self.deleted_open.iter().filter(|d| d.fs == Some(i)).map(|d| d.alloc).sum();
                let hidden: u64 = self.hidden.iter().filter(|h| h.fs == i).map(|h| h.alloc).sum();
                let used = sv.used();
                Some(Reconcile {
                    fs: i,
                    total: sv.total,
                    used,
                    avail: sv.avail,
                    reserved: sv.reserved(),
                    scanned: fs.scanned_alloc,
                    deleted_open,
                    hidden,
                    unaccounted: used as i64 - fs.scanned_alloc as i64 - deleted_open as i64 - hidden as i64,
                    denied_dirs: fs.denied_dirs,
                })
            })
            .collect()
    }

    /// Index of the filesystem whose tree contains `node`.
    pub fn fs_of_node(&self, node: NodeId) -> Option<usize> {
        let root = self.tree.root_of(node);
        self.filesystems.iter().position(|f| f.root_node == Some(root))
    }

    /// Resolve an absolute path to a node in the scanned tree.
    pub fn lookup(&self, path: &str) -> Option<NodeId> {
        let path = path.trim_end_matches('/');
        let path = if path.is_empty() { "/" } else { path };
        // Longest scanned mountpoint that is a prefix of `path`.
        let mut best: Option<(&FsInfo, usize)> = None;
        for fs in &self.filesystems {
            let Some(_) = fs.root_node else { continue };
            let mp = fs.scan_root.as_str();
            let matches =
                path == mp || mp == "/" || (path.starts_with(mp) && path.as_bytes().get(mp.len()) == Some(&b'/'));
            if matches && best.is_none_or(|(_, l)| mp.len() > l) {
                best = Some((fs, mp.len()));
            }
        }
        let (fs, _) = best?;
        let rest = path[fs.scan_root.len()..].trim_start_matches('/');
        let mut node = fs.root_node?;
        for comp in rest.split('/').filter(|c| !c.is_empty()) {
            node = self.tree.child_by_name(node, comp.as_bytes())?;
        }
        Some(node)
    }
}

pub fn fmt_size(b: u64) -> String {
    humansize::format_size(b, humansize::BINARY.decimal_places(1).space_after_value(true))
}

pub fn fmt_signed(b: i64) -> String {
    if b < 0 { format!("-{}", fmt_size(b.unsigned_abs())) } else { format!("+{}", fmt_size(b as u64)) }
}
