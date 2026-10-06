//! A made-up but realistic workstation snapshot for the README screenshots
//! (`diskeye demo`, `just screenshots`). Names and sizes are fixed, and
//! timestamps are relative to `util::now_secs()`, so with `$DISKEYE_NOW` set
//! the output is byte-for-byte reproducible.

use crate::model::tree::{Kind, flags};
use crate::model::*;
use crate::scan::walker::{TmpDir, TmpEntry};
use anyhow::Result;
use std::path::{Path, PathBuf};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

const DAY: i64 = 86_400;

fn now() -> i64 {
    crate::util::now_secs()
}

/// Write the demo snapshot and its baseline from a week earlier into `dir`,
/// named like real snapshots. Returns (current, baseline).
pub fn write(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let mut paths = [false, true].map(|old| {
        let snap = build(old);
        let p = dir.join(format!("{}-{}.dkeye", snap.meta.host, crate::util::timestamp_compact(snap.meta.started)));
        (snap, p)
    });
    for (snap, p) in &mut paths {
        snapshot::save(snap, p)?;
        // The web UI lists snapshots with their file time; keep it reproducible.
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(snap.meta.started as u64);
        std::fs::File::options().write(true).open(&*p)?.set_modified(t)?;
    }
    let [(_, current), (_, older)] = paths;
    Ok((current, older))
}

// ------------------------------------------------------------ tree builder

enum N {
    File { name: String, size: u64, alloc: Option<u64>, age: i64 },
    Dir { name: String, kids: Vec<N> },
    Mount(String),
}

fn f(name: &str, size: u64) -> N {
    N::File { name: name.into(), size, alloc: None, age: age_of(name) }
}

/// A sparse file: `size` apparent, `alloc` on disk.
fn sparse(name: &str, size: u64, alloc: u64) -> N {
    N::File { name: name.into(), size, alloc: Some(alloc), age: age_of(name) % (3 * DAY) }
}

fn d(name: &str, kids: Vec<N>) -> N {
    N::Dir { name: name.into(), kids }
}

fn mp(name: &str) -> N {
    N::Mount(name.into())
}

fn hash(s: &str) -> u64 {
    // FNV-1a, then a splitmix finaliser.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h
}

fn age_of(name: &str) -> i64 {
    (hash(name) % 400) as i64 * DAY + (hash(name) % DAY as u64) as i64
}

const WORDS: &[&str] = &[
    "core", "util", "net", "io", "gfx", "audio", "proto", "index", "render", "parse", "sched", "crypt", "store",
    "font", "codec", "shader", "locale", "input", "layout", "query",
];

/// `count` files totalling about `total` bytes, named `{prefix}-{word}-{i}.{ext}`,
/// with a long-tailed size distribution.
fn filler(prefix: &str, ext: &str, count: usize, total: u64) -> Vec<N> {
    let weights: Vec<u64> = (0..count).map(|i| 1 + hash(&format!("{prefix}{i}")) % 1000).collect();
    let weights: Vec<u64> = weights.iter().map(|w| w * w).collect();
    let sum: u64 = weights.iter().sum();
    (0..count)
        .map(|i| {
            let w = WORDS[(hash(&format!("{prefix}w{i}")) % WORDS.len() as u64) as usize];
            let name = if ext.is_empty() { format!("{prefix}-{w}-{i}") } else { format!("{prefix}-{w}-{i}.{ext}") };
            f(&name, (total as u128 * weights[i] as u128 / sum as u128) as u64)
        })
        .collect()
}

/// A directory of `dirs` subdirectories with `per` files each, about `total` bytes.
fn blob(name: &str, dirs: &[&str], per: usize, ext: &str, total: u64) -> N {
    let weights: Vec<u64> = dirs.iter().map(|s| 1 + hash(s) % 9).collect();
    let sum: u64 = weights.iter().sum();
    d(name, dirs.iter().zip(&weights).map(|(s, w)| d(s, filler(s, ext, per, total * w / sum))).collect())
}

fn round_alloc(size: u64) -> u64 {
    size.div_ceil(4096) * 4096
}

fn to_tmp(n: N) -> TmpDir {
    let N::Dir { name, kids } = n else { unreachable!("roots are directories") };
    let mut t = TmpDir { name: name.into_bytes().into(), items: 1, alloc: 4096, apparent: 4096, ..Default::default() };
    for k in kids {
        match k {
            N::File { name, size, alloc, age } => {
                let alloc = alloc.unwrap_or_else(|| round_alloc(size));
                let mtime = now() - age;
                let fl = if alloc + 64 * MIB < size { flags::SPARSE } else { 0 };
                t.apparent += size;
                t.alloc += alloc;
                t.items += 1;
                t.mtime = t.mtime.max(mtime);
                t.files.push(TmpEntry {
                    name: name.into_bytes().into(),
                    kind: Kind::File,
                    flags: fl,
                    apparent: size,
                    alloc,
                    mtime,
                });
            }
            N::Mount(name) => t.files.push(TmpEntry {
                name: name.into_bytes().into(),
                kind: Kind::Dir,
                flags: flags::MOUNTPOINT,
                apparent: 0,
                alloc: 0,
                mtime: now() - 40 * DAY,
            }),
            dir @ N::Dir { .. } => {
                let c = to_tmp(dir);
                t.apparent += c.apparent;
                t.alloc += c.alloc;
                t.items += c.items;
                t.mtime = t.mtime.max(c.mtime);
                t.dirs.push(c);
            }
        }
    }
    t
}

// ------------------------------------------------------------ the machine

/// `old` is the same machine a week earlier, for the Diff views.
pub fn build(old: bool) -> Snapshot {
    // New vs. older size.
    let s = |new: u64, older: u64| if old { older } else { new };
    let started = if old { now() - 7 * DAY - 2 * 3600 } else { now() - 2 * 3600 };

    let layers: Vec<N> = (0..18)
        .map(|i| {
            let size = [2900, 1800, 1300, 980, 760, 610, 540, 420, 380, 310, 260, 210, 160, 120, 90, 60, 40, 20][i];
            d(
                &format!("{:012x}", hash(&format!("layer{i}")) >> 16),
                vec![d("diff", filler(&format!("l{i}"), "so", 6, size * MIB))],
            )
        })
        .collect();
    let layers: Vec<N> = if old { layers.into_iter().take(15).collect() } else { layers };

    let root = d(
        "/",
        vec![
            mp("boot"),
            mp("home"),
            d("mnt", vec![mp("data")]),
            f("swapfile", 16 * GIB),
            d(
                "usr",
                vec![
                    blob(
                        "lib",
                        &["llvm", "python3.13", "firmware", "jvm", "qt6", "gcc", "dri", "libreoffice", "node_modules"],
                        14,
                        "so",
                        12 * GIB,
                    ),
                    blob(
                        "share",
                        &["icons", "fonts", "doc", "locale", "texmf-dist", "man", "help"],
                        12,
                        "dat",
                        6 * GIB,
                    ),
                    d("bin", filler("bin", "", 30, 1200 * MIB)),
                    d("include", filler("inc", "h", 20, 380 * MIB)),
                ],
            ),
            d(
                "opt",
                vec![
                    blob("cuda", &["lib64", "nsight", "extras"], 8, "so", 4500 * MIB),
                    d("google", filler("chrome", "pak", 8, 380 * MIB)),
                ],
            ),
            d(
                "var",
                vec![
                    d(
                        "lib",
                        vec![
                            d(
                                "docker",
                                vec![
                                    d("overlay2", layers),
                                    d(
                                        "volumes",
                                        vec![
                                            d("pgdata", vec![d("_data", filler("pg", "", 24, s(9400, 8100) * MIB))]),
                                            d("grafana-storage", vec![d("_data", filler("gf", "db", 4, 310 * MIB))]),
                                            d("3f9c1a7be02d", vec![d("_data", filler("anon", "", 6, 1100 * MIB))]),
                                        ],
                                    ),
                                    d("buildkit", filler("bk", "", 12, s(6200, 4300) * MIB)),
                                    d("containers", filler("ctr", "log", 4, 420 * MIB)),
                                    d("image", filler("img", "json", 6, 40 * MIB)),
                                ],
                            ),
                            d(
                                "libvirt",
                                vec![d(
                                    "images",
                                    vec![
                                        sparse("archlinux.qcow2", 64 * GIB, s(21, 19) * GIB),
                                        sparse("ubuntu-server.qcow2", 40 * GIB, 11 * GIB),
                                        f("Win11_24H2_English_x64.iso", 5800 * MIB),
                                    ],
                                )],
                            ),
                            blob("flatpak", &["runtime", "app", "repo"], 10, "", 9100 * MIB),
                            d("systemd", vec![d("coredump", filler("core.firefox", "zst", 5, 2200 * MIB))]),
                            d("pacman", filler("sync", "db", 6, 180 * MIB)),
                        ],
                    ),
                    d(
                        "cache",
                        vec![d("pacman", vec![d("pkg", filler("pkg", "pkg.tar.zst", 40, s(7600, 5900) * MIB))])],
                    ),
                    d("log", vec![d("journal", filler("system", "journal", 20, s(3900, 3100) * MIB))]),
                    d("tmp", filler("tmp", "", 4, 90 * MIB)),
                ],
            ),
            d("etc", filler("etc", "conf", 30, 28 * MIB)),
            d("root", vec![d(".cache", filler("rc", "", 4, 140 * MIB))]),
        ],
    );

    let home = d(
        "/home",
        vec![d(
            "alex",
            vec![
                d(
                    ".cache",
                    vec![
                        d("yay", filler("aur", "tar.zst", 14, s(4100, 3800) * MIB)),
                        d("pip", filler("wheel", "whl", 18, 2300 * MIB)),
                        d("go-build", filler("go", "a", 30, 3400 * MIB)),
                        d("JetBrains", filler("idea", "dat", 10, 2500 * MIB)),
                        d("mozilla", filler("ff", "", 16, 1200 * MIB)),
                        d("thumbnails", filler("thumb", "png", 30, 310 * MIB)),
                    ],
                ),
                d(
                    ".local",
                    vec![d(
                        "share",
                        vec![
                            d(
                                "Steam",
                                vec![d(
                                    "steamapps",
                                    vec![d(
                                        "common",
                                        vec![
                                            blob(
                                                "Baldurs Gate 3",
                                                &["Data", "bin", "Localization"],
                                                10,
                                                "pak",
                                                122 * GIB,
                                            ),
                                            blob("Cyberpunk 2077", &["archive", "bin", "r6"], 10, "archive", 71 * GIB),
                                            blob("Hades II", &["Content", "Ship"], 8, "pkg", 14 * GIB),
                                        ],
                                    )],
                                )],
                            ),
                            d("containers", vec![d("storage", filler("podman", "", 10, 6100 * MIB))]),
                            d("Trash", vec![d("files", filler("old", "mkv", 6, 8400 * MIB))]),
                        ],
                    )],
                ),
                d(
                    ".ollama",
                    vec![d(
                        "models",
                        vec![d(
                            "blobs",
                            if old {
                                vec![f("sha256-llama3.1-8b", 4700 * MIB), f("sha256-qwen3-14b", 9300 * MIB)]
                            } else {
                                vec![
                                    f("sha256-llama3.1-8b", 4700 * MIB),
                                    f("sha256-qwen3-14b", 9300 * MIB),
                                    f("sha256-qwen3-32b", 19 * GIB),
                                ]
                            },
                        )],
                    )],
                ),
                d(".rustup", vec![blob("toolchains", &["stable", "nightly"], 12, "rlib", 5500 * MIB)]),
                d(".cargo", vec![d("registry", filler("crate", "crate", 40, 2400 * MIB))]),
                d(
                    "projects",
                    vec![
                        d(
                            "diskeye",
                            vec![
                                d(
                                    "target",
                                    vec![blob(
                                        "release",
                                        &["deps", "build", "incremental"],
                                        16,
                                        "rlib",
                                        s(9800, 4100) * MIB,
                                    )],
                                ),
                                d("src", filler("src", "rs", 30, 2 * MIB)),
                                f("Cargo.toml", 2 * KIB),
                            ],
                        ),
                        d(
                            "webshop",
                            vec![
                                d("node_modules", filler("pkg", "js", 60, 1400 * MIB)),
                                d("src", filler("src", "ts", 30, 6 * MIB)),
                                f("package.json", 3 * KIB),
                            ],
                        ),
                        d(
                            "ml-experiments",
                            vec![
                                d(
                                    ".venv",
                                    vec![blob(
                                        "lib",
                                        &["torch", "nvidia", "triton", "transformers"],
                                        12,
                                        "so",
                                        7200 * MIB,
                                    )],
                                ),
                                d("checkpoints", filler("ckpt", "safetensors", 8, 14 * GIB)),
                                d("datasets", filler("shard", "parquet", 40, 38 * GIB)),
                            ],
                        ),
                    ],
                ),
                d("Videos", filler("clip", "mp4", 30, 45 * GIB)),
                blob("Pictures", &["2023", "2024", "2025", "2026"], 40, "jpg", 22 * GIB),
                d(
                    "Downloads",
                    if old {
                        filler("dl", "zip", 20, 9 * GIB)
                    } else {
                        let mut v = filler("dl", "zip", 20, 9 * GIB);
                        v.push(f("ubuntu-24.04.3-desktop-amd64.iso", 6100 * MIB));
                        v.push(f("fedora-43-x86_64.iso", 2300 * MIB));
                        v
                    },
                ),
                d("Documents", filler("doc", "pdf", 40, 3100 * MIB)),
            ],
        )],
    );

    let boot = d(
        "/boot",
        vec![
            f("vmlinuz-linux", 14 * MIB),
            f("initramfs-linux.img", 38 * MIB),
            f("initramfs-linux-fallback.img", 140 * MIB),
            d("EFI", filler("efi", "efi", 4, 6 * MIB)),
        ],
    );
    let data = d(
        "/mnt/data",
        vec![
            blob("backups", &["borg-2024", "borg-2025", "borg-2026", "phone"], 30, "", 820 * GIB),
            blob("media", &["Movies", "Series", "Music"], 30, "mkv", 1100 * GIB),
            d(
                "vm-archive",
                vec![sparse("win10-legacy.qcow2", 128 * GIB, 47 * GIB), sparse("debian-12.qcow2", 128 * GIB, 19 * GIB)],
            ),
        ],
    );

    let mut snap = Snapshot { version: SNAPSHOT_VERSION, ..Default::default() };
    snap.meta = ScanMeta {
        host: "workstation".into(),
        diskeye_version: env!("CARGO_PKG_VERSION").into(),
        started,
        duration_ms: 9_400,
        euid: 0,
        sudo_user: Some("alex".into()),
        roots: vec![],
        kernel: "6.17.1-arch1-1".into(),
    };

    // (tree, mountpoint, source, fstype, (major, minor), size, reserved %, extra used)
    let fss = [
        (root, "/", "/dev/mapper/vg_system-lv_root", "ext4", (254, 1), 160 * GIB, 5, 4300 * MIB + 2300 * MIB),
        (home, "/home", "/dev/mapper/vg_system-lv_home", "ext4", (254, 2), 600 * GIB, 5, 5200 * MIB),
        (boot, "/boot", "/dev/nvme0n1p1", "vfat", (259, 1), GIB, 0, MIB),
        (data, "/mnt/data", "/dev/sda1", "ext4", (8, 1), 3726 * GIB, 1, 31 * GIB),
    ];
    for (i, (tree, mount, source, fstype, (major, minor), total, reserved_pct, extra)) in fss.into_iter().enumerate() {
        let mut fs = FsInfo {
            mount_point: mount.into(),
            scan_root: mount.into(),
            source: source.into(),
            fstype: fstype.into(),
            options: "rw,relatime".into(),
            fs_root: "/".into(),
            dev_major: major,
            dev_minor: minor,
            ..Default::default()
        };
        let r = crate::scan::append_tree(&mut snap.tree, to_tmp(tree), &mut fs);
        let n = *snap.tree.node(r);
        fs.root_node = Some(r);
        fs.scanned_alloc = n.alloc;
        fs.scanned_apparent = n.apparent;
        fs.scanned_items = n.items as u64;
        // The older snapshot was taken with a bit more in the deleted-open gap.
        let used = n.alloc + extra + if i == 0 && !old { 4200 * MIB } else { 0 };
        let reserved = total / 100 * reserved_pct;
        let free = total.saturating_sub(used);
        fs.statvfs = Some(StatVfs {
            total,
            free,
            avail: free.saturating_sub(reserved),
            files: total / 16384,
            files_free: total / 16384 - n.items as u64,
            bsize: 4096,
        });
        snap.tree.roots.push(r);
        snap.filesystems.push(fs);
    }
    snap.filesystems.push(FsInfo {
        mount_point: "/mnt/games".into(),
        scan_root: "/mnt/games".into(),
        source: "/dev/sdb2".into(),
        fstype: "ntfs3".into(),
        statvfs: Some(StatVfs { total: 1863 * GIB, free: 412 * GIB, avail: 412 * GIB, ..Default::default() }),
        skipped_reason: Some("excluded by --exclude-fs".into()),
        ..Default::default()
    });

    physical(&mut snap);
    entities(&mut snap, old);
    crate::model::attribution::attribute(&mut snap);

    if !old {
        snap.deleted_open = vec![DeletedOpen {
            pid: 2231,
            comm: "java".into(),
            fd: 41,
            path: "/var/log/elasticsearch/gc.log".into(),
            alloc: 4200 * MIB,
            apparent: 4200 * MIB,
            fs: Some(0),
            ..Default::default()
        }];
    }
    snap.hidden =
        vec![HiddenUnderMount { fs: 0, path: "/home".into(), alloc: 2300 * MIB, apparent: 2300 * MIB, items: 18_204 }];
    let p = |name: &str, coverage, notes: &[&str], ms| ProviderReport {
        name: name.into(),
        coverage,
        notes: notes.iter().map(|n| n.to_string()).collect(),
        duration_ms: ms,
    };
    snap.providers = vec![
        p("block", Coverage::Complete, &[], 14),
        p("lvm", Coverage::Complete, &[], 61),
        p("deleted-open", Coverage::Complete, &[], 22),
        p("hidden-under-mounts", Coverage::Complete, &[], 310),
        p("docker", Coverage::Complete, &[], 180),
        p("podman", Coverage::Complete, &[], 95),
        p("libvirt", Coverage::Complete, &[], 240),
        p("flatpak", Coverage::Complete, &[], 70),
        p("journald", Coverage::Complete, &[], 30),
        p("classifier", Coverage::Complete, &[], 120),
        p("containerd", Coverage::Absent, &["not installed"], 1),
        p("kube", Coverage::Absent, &["no kubeconfig"], 2),
    ];
    snap
}

fn physical(snap: &mut Snapshot) {
    let s = |x: &str| x.to_string();
    let dev = |kname: &str, name: &str, dtype: &str, size: u64, parents: &[&str]| BlockDev {
        kname: s(kname),
        name: s(name),
        path: if dtype == "lvm" || dtype == "crypt" { format!("/dev/mapper/{name}") } else { format!("/dev/{name}") },
        dtype: s(dtype),
        size,
        parents: parents.iter().map(|p| s(p)).collect(),
        ..Default::default()
    };
    let fs = |t: &str, mps: &[&str]| (Some(s(t)), mps.iter().map(|m| s(m)).collect::<Vec<_>>());
    let with =
        |b: BlockDev, (fstype, mountpoints): (Option<String>, Vec<String>)| BlockDev { fstype, mountpoints, ..b };
    snap.block = vec![
        BlockDev {
            model: Some(s("Samsung SSD 990 PRO 1TB")),
            tran: Some(s("nvme")),
            ..dev("nvme0n1", "nvme0n1", "disk", 931 * GIB + 524 * MIB, &[])
        },
        BlockDev {
            part_type: Some(s("EFI System")),
            ..with(dev("nvme0n1p1", "nvme0n1p1", "part", GIB, &["nvme0n1"]), fs("vfat", &["/boot"]))
        },
        with(dev("nvme0n1p2", "nvme0n1p2", "part", 930 * GIB + 524 * MIB, &["nvme0n1"]), fs("crypto_LUKS", &[])),
        with(dev("dm-0", "cryptlvm", "crypt", 930 * GIB + 508 * MIB, &["nvme0n1p2"]), fs("LVM2_member", &[])),
        with(dev("dm-1", "vg_system-lv_root", "lvm", 160 * GIB, &["dm-0"]), fs("ext4", &["/"])),
        with(dev("dm-2", "vg_system-lv_home", "lvm", 600 * GIB, &["dm-0"]), fs("ext4", &["/home"])),
        BlockDev { used_by: vec![s("VM win11")], ..dev("dm-3", "vg_system-lv_win11", "lvm", 80 * GIB, &["dm-0"]) },
        with(dev("dm-4", "vg_system-lv_old_backup", "lvm", 40 * GIB, &["dm-0"]), fs("ext4", &[])),
        BlockDev {
            model: Some(s("WDC WD40EFRX-68N")),
            tran: Some(s("sata")),
            ..dev("sda", "sda", "disk", 3726 * GIB, &[])
        },
        with(dev("sda1", "sda1", "part", 3726 * GIB, &["sda"]), fs("ext4", &["/mnt/data"])),
        BlockDev { model: Some(s("ST2000DM008")), tran: Some(s("sata")), ..dev("sdb", "sdb", "disk", 1863 * GIB, &[]) },
        BlockDev {
            part_type: Some(s("Microsoft basic data")),
            ..with(dev("sdb2", "sdb2", "part", 1863 * GIB, &["sdb"]), fs("ntfs", &["/mnt/games"]))
        },
    ];
    let lv = |name: &str, size: u64, kname: &str, mps: &[&str], fstype: Option<&str>| Lv {
        vg: s("vg_system"),
        name: s(name),
        size,
        attr: Some(s("-wi-ao----")),
        segtype: Some(s("linear")),
        kname: Some(s(kname)),
        mountpoints: mps.iter().map(|m| s(m)).collect(),
        fstype: fstype.map(s),
        ..Default::default()
    };
    snap.lvm = LvmInfo {
        source: s("lvm2"),
        vgs: vec![Vg {
            name: s("vg_system"),
            size: 930 * GIB + 508 * MIB,
            free: 50 * GIB + 508 * MIB,
            free_is_estimate: false,
            extent_size: Some(4 * MIB),
        }],
        pvs: vec![Pv {
            name: s("/dev/mapper/cryptlvm"),
            vg: s("vg_system"),
            size: 930 * GIB + 508 * MIB,
            free: Some(50 * GIB + 508 * MIB),
        }],
        lvs: vec![
            lv("lv_home", 600 * GIB, "dm-2", &["/home"], Some("ext4")),
            Lv { attr: Some(s("-wi-a-----")), ..lv("lv_old_backup", 40 * GIB, "dm-4", &[], Some("ext4")) },
            lv("lv_root", 160 * GIB, "dm-1", &["/"], Some("ext4")),
            lv("lv_win11", 80 * GIB, "dm-3", &[], None),
        ],
    };
    snap.swaps = vec![Swap { path: s("/swapfile"), kind: s("file"), size: 16 * GIB, used: 1200 * MIB }];
}

/// Entity builder: `add` pushes and returns the new index.
struct B<'a> {
    snap: &'a mut Snapshot,
}

impl B<'_> {
    fn add(&mut self, e: Entity) -> u32 {
        self.snap.entities.push(e);
        self.snap.entities.len() as u32 - 1
    }
}

fn ent(kind: &str, name: &str, provider: &str, group: &str, paths: &[&str]) -> Entity {
    Entity {
        kind: kind.into(),
        name: name.into(),
        provider: provider.into(),
        group: group.into(),
        paths: paths.iter().map(|p| p.to_string()).collect(),
        ..Default::default()
    }
}

fn reclaim(risk: Risk, reason: &str, label: &str, steps: Vec<ActionStep>) -> Option<Reclaim> {
    Some(Reclaim {
        risk,
        reason: reason.into(),
        estimate: None,
        action: Some(ActionSpec { label: label.into(), steps }),
    })
}

fn empty(path: &str) -> Vec<ActionStep> {
    vec![ActionStep::EmptyDir { path: path.into() }]
}

fn cmd(argv: &[&str], root: bool) -> Vec<ActionStep> {
    vec![ActionStep::Command { argv: argv.iter().map(|a| a.to_string()).collect(), root }]
}

fn entities(snap: &mut Snapshot, old: bool) {
    let mut b = B { snap };
    let s = |x: &str| x.to_string();
    let layer_paths: Vec<String> = (0..if old { 15 } else { 18 })
        .map(|i| format!("/var/lib/docker/overlay2/{:012x}", hash(&format!("layer{i}")) >> 16))
        .collect();
    let lp = |idx: &[usize]| -> Vec<String> { idx.iter().filter_map(|&i| layer_paths.get(i).cloned()).collect() };

    // Docker (system)
    const DOCKER: &str = "Docker (system)";
    let sock = "/var/run/docker.sock";
    let images = b.add(ent("docker.images", "Images", "docker", DOCKER, &[]));
    let mut images_list = vec![
        ("postgres:16", vec![0, 3, 6, 9], "in use by 1 container"),
        ("node:22-bookworm", vec![0, 1, 4, 7, 10], "in use by 1 container"),
        ("python:3.12", vec![0, 1, 2, 8], "not used by any container"),
        ("<none>:<none>", vec![11, 12, 13, 14], "dangling"),
    ];
    if !old {
        images_list.push(("grafana/grafana:12.1", vec![5, 15, 16, 17], "in use by 1 container"));
    }
    for (name, layers, state) in images_list {
        let dangling = state == "dangling";
        let unused = state.starts_with("not used");
        b.add(Entity {
            parent: Some(images),
            share_key: Some(s("docker:/var/lib/docker")),
            attrs: vec![(s("state"), s(state))],
            paths: lp(&layers),
            reclaim: if dangling {
                reclaim(
                    Risk::Safe,
                    "dangling image",
                    "remove image",
                    vec![ActionStep::DockerApi {
                        socket: s(sock),
                        method: s("DELETE"),
                        path: format!("/images/{:012x}", hash(name) >> 16),
                    }],
                )
            } else if unused {
                reclaim(
                    Risk::Review,
                    "not used by any container",
                    "remove image",
                    vec![ActionStep::DockerApi {
                        socket: s(sock),
                        method: s("DELETE"),
                        path: format!("/images/{:012x}", hash(name) >> 16),
                    }],
                )
            } else {
                None
            },
            ..ent("docker.image", name, "docker", DOCKER, &[])
        });
    }
    let vols = b.add(ent("docker.volumes", "Volumes", "docker", DOCKER, &[]));
    b.add(Entity {
        parent: Some(vols),
        attrs: vec![(s("used by"), s("db-1"))],
        ..ent("docker.volume", "pgdata", "docker", DOCKER, &["/var/lib/docker/volumes/pgdata"])
    });
    b.add(Entity {
        parent: Some(vols),
        attrs: vec![(s("used by"), s("grafana-1"))],
        ..ent("docker.volume", "grafana-storage", "docker", DOCKER, &["/var/lib/docker/volumes/grafana-storage"])
    });
    b.add(Entity {
        parent: Some(vols),
        reclaim: reclaim(
            Risk::Review,
            "anonymous volume not used by any container",
            "remove volume",
            vec![ActionStep::DockerApi { socket: s(sock), method: s("DELETE"), path: s("/volumes/3f9c1a7be02d") }],
        ),
        ..ent("docker.volume", "3f9c1a7be02d", "docker", DOCKER, &["/var/lib/docker/volumes/3f9c1a7be02d"])
    });
    b.add(Entity {
        reclaim: reclaim(
            Risk::Safe,
            "rebuilt on the next docker build",
            "prune build cache",
            vec![ActionStep::DockerApi { socket: s(sock), method: s("POST"), path: s("/build/prune") }],
        ),
        ..ent("docker.buildcache", "Build cache", "docker", DOCKER, &["/var/lib/docker/buildkit"])
    });
    b.add(ent("docker.containers", "Containers", "docker", DOCKER, &["/var/lib/docker/containers"]));

    // Podman (rootless)
    b.add(ent(
        "podman.storage",
        "Image and container storage",
        "podman",
        "Podman (rootless, alex)",
        &["/home/alex/.local/share/containers/storage"],
    ));

    // Virtual machines
    const VMS: &str = "Virtual machines";
    let arch = b.add(Entity { attrs: vec![(s("state"), s("running"))], ..ent("vm", "archlinux", "libvirt", VMS, &[]) });
    b.add(Entity {
        parent: Some(arch),
        virtual_size: Some(64 * GIB),
        ..ent("vm.disk", "archlinux.qcow2", "libvirt", VMS, &["/var/lib/libvirt/images/archlinux.qcow2"])
    });
    let win = b.add(Entity { attrs: vec![(s("state"), s("shut off"))], ..ent("vm", "win11", "libvirt", VMS, &[]) });
    b.add(Entity {
        parent: Some(win),
        block_devs: vec![s("dm-3")],
        external_bytes: 80 * GIB,
        reclaim: reclaim(
            Risk::Danger,
            "VM disk of a shut-off VM",
            "remove LV",
            cmd(&["lvremove", "vg_system/lv_win11"], true),
        ),
        ..ent("vm.disk", "lv_win11", "libvirt", VMS, &[])
    });
    b.add(Entity {
        parent: Some(win),
        ..ent(
            "vm.iso",
            "Win11_24H2_English_x64.iso",
            "libvirt",
            VMS,
            &["/var/lib/libvirt/images/Win11_24H2_English_x64.iso"],
        )
    });
    const ORPHAN: &str = "Disk images (not used by any VM)";
    for p in [
        "/var/lib/libvirt/images/ubuntu-server.qcow2",
        "/mnt/data/vm-archive/win10-legacy.qcow2",
        "/mnt/data/vm-archive/debian-12.qcow2",
    ] {
        let name = p.rsplit('/').next().unwrap();
        b.add(Entity {
            virtual_size: Some(if name.starts_with("ubuntu") { 40 * GIB } else { 128 * GIB }),
            reclaim: reclaim(
                Risk::Review,
                "no VM uses this disk image",
                "move to trash",
                vec![ActionStep::DeletePath { path: s(p), trash: true }],
            ),
            ..ent("vm.image", name, "libvirt", ORPHAN, &[p])
        });
    }
    b.add(Entity {
        block_devs: vec![s("dm-4")],
        external_bytes: 40 * GIB,
        reclaim: reclaim(
            Risk::Review,
            "not mounted, not used by a VM",
            "remove LV",
            cmd(&["lvremove", "vg_system/lv_old_backup"], true),
        ),
        ..ent("lv", "vg_system/lv_old_backup", "libvirt", "Unused logical volumes", &[])
    });

    // Flatpak
    b.add(ent("flatpak.runtime", "Runtimes", "flatpak", "Flatpak", &["/var/lib/flatpak/runtime"]));
    b.add(ent("flatpak.app", "Apps", "flatpak", "Flatpak", &["/var/lib/flatpak/app"]));
    b.add(Entity {
        reclaim: reclaim(
            Risk::Safe,
            "objects not referenced by any installed ref",
            "flatpak uninstall --unused",
            cmd(&["flatpak", "uninstall", "--unused", "-y"], true),
        ),
        ..ent("flatpak.repo", "Repository", "flatpak", "Flatpak", &["/var/lib/flatpak/repo"])
    });

    // Classifier rules
    let rule = |b: &mut B, kind: &str, name: &str, group: &str, path: &str, r: Option<Reclaim>| {
        b.add(Entity { reclaim: r, ..ent(kind, name, "classifier", group, &[path]) })
    };
    let pk = "/var/cache/pacman/pkg";
    rule(
        &mut b,
        "pkgcache",
        "pacman package cache",
        "Package caches",
        pk,
        reclaim(
            Risk::Safe,
            "old package versions; paccache -rk2 keeps the two newest of each",
            "paccache -rk2",
            cmd(&["paccache", "-rk2"], true),
        ),
    );
    let yay = "/home/alex/.cache/yay";
    rule(
        &mut b,
        "pkgcache",
        "yay build cache",
        "Package caches",
        yay,
        reclaim(Risk::Safe, "AUR helper build cache, re-downloaded on demand", "empty", empty(yay)),
    );
    for (name, p) in [
        ("pip cache", "/home/alex/.cache/pip"),
        ("Go build cache", "/home/alex/.cache/go-build"),
        ("JetBrains caches", "/home/alex/.cache/JetBrains"),
    ] {
        rule(
            &mut b,
            "devcache",
            name,
            "Developer caches",
            p,
            reclaim(Risk::Safe, "rebuilt on demand", "empty", empty(p)),
        );
    }
    rule(&mut b, "devcache", "Cargo registry", "Developer caches", "/home/alex/.cargo/registry", None);
    rule(&mut b, "devcache", "Rust toolchains", "Developer caches", "/home/alex/.rustup/toolchains", None);
    rule(
        &mut b,
        "cache",
        "Firefox cache",
        "Browser caches",
        "/home/alex/.cache/mozilla",
        reclaim(Risk::Safe, "browser cache", "empty", empty("/home/alex/.cache/mozilla")),
    );
    rule(
        &mut b,
        "cache",
        "Thumbnails",
        "App caches",
        "/home/alex/.cache/thumbnails",
        reclaim(Risk::Safe, "regenerated on demand", "empty", empty("/home/alex/.cache/thumbnails")),
    );
    let models = Reclaim {
        risk: Risk::Review,
        reason: s("downloaded models; remove individual ones with `ollama rm`"),
        estimate: None,
        action: None,
    };
    rule(&mut b, "models", "Ollama models", "AI models", "/home/alex/.ollama/models", Some(models));
    let t = "/home/alex/projects/diskeye/target";
    rule(
        &mut b,
        "build-artifacts",
        "Rust target/ (diskeye)",
        "Build artifacts",
        t,
        reclaim(
            Risk::Safe,
            "rebuilt by cargo build",
            "cargo clean",
            cmd(&["cargo", "clean", "--manifest-path", "/home/alex/projects/diskeye/Cargo.toml"], false),
        ),
    );
    let nm = "/home/alex/projects/webshop/node_modules";
    rule(
        &mut b,
        "build-artifacts",
        "node_modules (webshop)",
        "Build artifacts",
        nm,
        reclaim(
            Risk::Safe,
            "reinstalled by npm install",
            "move to trash",
            vec![ActionStep::DeletePath { path: s(nm), trash: true }],
        ),
    );
    let venv = "/home/alex/projects/ml-experiments/.venv";
    rule(
        &mut b,
        "build-artifacts",
        "Python venv (ml-experiments)",
        "Build artifacts",
        venv,
        reclaim(
            Risk::Review,
            "recreated from requirements.txt",
            "move to trash",
            vec![ActionStep::DeletePath { path: s(venv), trash: true }],
        ),
    );
    let trash = "/home/alex/.local/share/Trash";
    rule(
        &mut b,
        "trash",
        "Trash (alex)",
        "Trash",
        trash,
        reclaim(Risk::Review, "files you already deleted", "empty trash", empty(&format!("{trash}/files"))),
    );
    let journal = "/var/log/journal";
    rule(
        &mut b,
        "logs",
        "systemd journal",
        "Logs",
        journal,
        reclaim(
            Risk::Safe,
            "keeps the newest 500M of logs",
            "journalctl --vacuum-size=500M",
            cmd(&["journalctl", "--vacuum-size=500M"], true),
        ),
    );
    let cores = "/var/lib/systemd/coredump";
    rule(
        &mut b,
        "crash",
        "Core dumps",
        "Logs",
        cores,
        reclaim(Risk::Safe, "crash dumps of past crashes", "empty", empty(cores)),
    );
}
