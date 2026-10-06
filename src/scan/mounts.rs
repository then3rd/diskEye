//! /proc/self/mountinfo parsing and choosing what to scan.

use crate::util::unescape_octal;

#[derive(Debug, Clone)]
pub struct Mount {
    pub id: u32,
    pub parent: u32,
    pub major: u32,
    pub minor: u32,
    /// Path within the filesystem that is mounted here.
    pub root: String,
    pub mount_point: String,
    pub options: String,
    pub fstype: String,
    pub source: String,
}

pub fn read_mountinfo() -> Vec<Mount> {
    std::fs::read_to_string("/proc/self/mountinfo").map(|s| parse_mountinfo(&s)).unwrap_or_default()
}

pub fn parse_mountinfo(s: &str) -> Vec<Mount> {
    s.lines()
        .filter_map(|l| {
            let (pre, post) = l.split_once(" - ")?;
            let f: Vec<&str> = pre.split(' ').collect();
            let g: Vec<&str> = post.split(' ').collect();
            if f.len() < 6 || g.len() < 2 {
                return None;
            }
            let (maj, min) = f[2].split_once(':')?;
            Some(Mount {
                id: f[0].parse().ok()?,
                parent: f[1].parse().ok()?,
                major: maj.parse().ok()?,
                minor: min.parse().ok()?,
                root: unescape_octal(f[3]),
                mount_point: unescape_octal(f[4]),
                options: f[5].to_string(),
                fstype: g[0].to_string(),
                source: unescape_octal(g[1]),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsClass {
    /// On-disk filesystem holding real data.
    Real,
    /// In-memory (tmpfs, ramfs): optional.
    Memory,
    Network,
    /// Container rootfs overlays, squashfs images, FUSE helpers: data is
    /// accounted elsewhere.
    Derived,
    /// Kernel interfaces (proc, sysfs, cgroup...).
    Pseudo,
}

pub fn classify(fstype: &str, source: &str) -> FsClass {
    match fstype {
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "zfs" | "f2fs" | "bcachefs" | "jfs" | "reiserfs" | "nilfs2"
        | "ntfs" | "ntfs3" | "fuseblk" | "vfat" | "exfat" | "hfsplus" | "udf" => FsClass::Real,
        "tmpfs" | "ramfs" => FsClass::Memory,
        "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "9p" | "ceph" | "glusterfs" | "fuse.sshfs" | "fuse.rclone"
        | "afs" | "virtiofs" => FsClass::Network,
        "overlay" | "squashfs" | "iso9660" | "fuse.portal" | "fuse.gvfsd-fuse" | "fuse.snapfuse" | "fuse.lxcfs"
        | "erofs" | "fuse.appimagefuse" => FsClass::Derived,
        "proc" | "sysfs" | "cgroup" | "cgroup2" | "devtmpfs" | "devpts" | "securityfs" | "debugfs" | "tracefs"
        | "pstore" | "bpf" | "mqueue" | "hugetlbfs" | "configfs" | "fusectl" | "efivarfs" | "binfmt_misc"
        | "autofs" | "rpc_pipefs" | "nsfs" | "selinuxfs" | "ramfs_" => FsClass::Pseudo,
        _ if source.starts_with("/dev/") => FsClass::Real,
        _ => FsClass::Pseudo,
    }
}

/// One filesystem (or btrfs subvolume) to walk, plus where else it's mounted.
#[derive(Debug, Clone)]
pub struct ScanUnit {
    pub mount: Mount,
    /// Where the walk starts (mount point unless the user asked for a subtree).
    pub scan_root: String,
    pub aliases: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub include_memory: bool,
    pub include_network: bool,
    pub exclude_fstypes: Vec<String>,
    pub exclude_mounts: Vec<String>,
}

fn is_under(path: &str, dir: &str) -> bool {
    dir == "/" || path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

/// Pick one primary mount per (device, subvolume); other mounts of the same
/// tree become aliases so nothing is scanned or counted twice.
pub fn select_units(mounts: &[Mount], sel: &Selection) -> (Vec<ScanUnit>, Vec<(Mount, String)>) {
    let mut skipped = Vec::new();
    let mut candidates: Vec<&Mount> = Vec::new();
    for m in mounts {
        let class = classify(&m.fstype, &m.source);
        let reason = match class {
            FsClass::Real => None,
            FsClass::Memory if sel.include_memory => None,
            FsClass::Network if sel.include_network => None,
            FsClass::Memory => Some("in-memory filesystem (use --tmpfs)"),
            FsClass::Network => Some("network filesystem (use --network)"),
            FsClass::Derived => Some("derived filesystem (data accounted at its source)"),
            FsClass::Pseudo => Some("pseudo filesystem"),
        };
        let reason = reason.or_else(|| {
            (sel.exclude_fstypes.iter().any(|t| t == &m.fstype)
                || sel.exclude_mounts.iter().any(|x| is_under(&m.mount_point, x)))
            .then_some("excluded by option")
        });
        match reason {
            Some(r) => {
                if class != FsClass::Pseudo {
                    skipped.push((m.clone(), r.to_string()));
                }
            }
            None => candidates.push(m),
        }
    }
    // Prefer the mount exposing the most of the filesystem, then the shortest path.
    candidates.sort_by(|a, b| {
        (a.major, a.minor, a.root.len(), a.mount_point.len()).cmp(&(
            b.major,
            b.minor,
            b.root.len(),
            b.mount_point.len(),
        ))
    });
    let mut units: Vec<ScanUnit> = Vec::new();
    for m in candidates {
        // Memory filesystems all share major 0 but each mount is distinct.
        let shared_dev = !(m.fstype == "tmpfs" || m.fstype == "ramfs");
        let primary = units.iter_mut().find(|u| {
            shared_dev && u.mount.major == m.major && u.mount.minor == m.minor && is_under(&m.root, &u.mount.root)
        });
        match primary {
            Some(u) => {
                let rel = m.root[u.mount.root.len().min(m.root.len())..].trim_start_matches('/');
                let target = if rel.is_empty() {
                    u.mount.mount_point.clone()
                } else {
                    format!("{}/{}", u.mount.mount_point.trim_end_matches('/'), rel)
                };
                if m.mount_point != target {
                    u.aliases.push((m.mount_point.clone(), target));
                }
            }
            None => units.push(ScanUnit { mount: m.clone(), scan_root: m.mount_point.clone(), aliases: Vec::new() }),
        }
    }
    units.sort_by(|a, b| a.mount.mount_point.cmp(&b.mount.mount_point));
    (units, skipped)
}

/// The mount containing `path` (longest matching mount point).
pub fn mount_for<'a>(mounts: &'a [Mount], path: &str) -> Option<&'a Mount> {
    mounts.iter().filter(|m| is_under(path, &m.mount_point)).max_by_key(|m| m.mount_point.len())
}

pub fn is_path_under(path: &str, dir: &str) -> bool {
    is_under(path, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
22 1 254:1 / / rw,relatime - ext4 /dev/mapper/vg-root rw
23 22 0:21 / /proc rw - proc proc rw
24 22 254:2 / /home rw,relatime - ext4 /dev/mapper/vg-home rw
25 22 254:2 /alice/data /srv/data rw - ext4 /dev/mapper/vg-home rw
26 22 0:30 / /tmp rw - tmpfs tmpfs rw
27 22 0:40 / /var/lib/docker/overlay2/abc/merged rw - overlay overlay rw
28 22 0:50 /@ /mnt/b rw - btrfs /dev/sdb1 rw
29 22 0:50 /@home /mnt/bh rw - btrfs /dev/sdb1 rw
30 22 8:1 / /mnt/my\\040disk rw - ntfs3 /dev/sda1 rw
";

    #[test]
    fn selects_units_and_aliases() {
        let m = parse_mountinfo(SAMPLE);
        assert_eq!(m.len(), 9);
        let (units, skipped) = select_units(&m, &Selection::default());
        let mps: Vec<&str> = units.iter().map(|u| u.mount.mount_point.as_str()).collect();
        assert_eq!(mps, vec!["/", "/home", "/mnt/b", "/mnt/bh", "/mnt/my disk"]);
        let home = units.iter().find(|u| u.mount.mount_point == "/home").unwrap();
        assert_eq!(home.aliases, vec![("/srv/data".to_string(), "/home/alice/data".to_string())]);
        assert!(skipped.iter().any(|(m, _)| m.fstype == "tmpfs"));
        assert!(skipped.iter().any(|(m, _)| m.fstype == "overlay"));
        assert!(!skipped.iter().any(|(m, _)| m.fstype == "proc"));
    }
}
