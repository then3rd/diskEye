//! TUI tests: a synthetic snapshot built from a real tempdir scan plus
//! hand-made physical/workload data, driven through the key state machine and
//! rendered into ratatui's TestBackend.

use super::app::{App, Popup, Tab};
use super::lineview::Target;
use super::{diffview, reconcile, ui};
use crate::model::*;
use crate::scan::walker;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn write(p: &Path, bytes: usize) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, vec![b'x'; bytes]).unwrap();
}

fn walk(root: &Path, mountpoints: &HashSet<PathBuf>) -> walker::TmpDir {
    use std::os::unix::fs::MetadataExt;
    let st = std::fs::metadata(root).unwrap();
    let hl = walker::new_hardlink_set();
    let progress = walker::Progress::default();
    let ctx = walker::WalkCtx {
        mountpoints,
        dev: (libc::major(st.dev()), libc::minor(st.dev())),
        allow_dev_change: false,
        excludes: None,
        hardlinks: &hl,
        progress: &progress,
    };
    walker::walk_root(&ctx, root, root.to_string_lossy().as_bytes())
}

fn fs_info(root: &Path, total: u64, free: u64, avail: u64) -> FsInfo {
    let p = root.to_string_lossy().into_owned();
    FsInfo {
        mount_point: p.clone(),
        scan_root: p,
        source: "/dev/mapper/vg-x".into(),
        fstype: "ext4".into(),
        statvfs: Some(StatVfs { total, free, avail, files: 0, files_free: 0, bsize: 4096 }),
        ..Default::default()
    }
}

/// Root filesystem `<tmp>/sys` with `<tmp>/sys/data` mounted as a second filesystem.
pub fn synth() -> (tempfile::TempDir, Snapshot) {
    let td = tempfile::tempdir().unwrap();
    let sys = td.path().join("sys");
    write(&sys.join("big/a.bin"), 3 * MIB as usize);
    write(&sys.join("big/b.bin"), MIB as usize);
    write(&sys.join("cache/c1"), 512 * 1024);
    write(&sys.join("cache/c2"), 256 * 1024);
    write(&sys.join("docs/readme.txt"), 10_000);
    write(&sys.join("docs/notes/n1"), 4000);
    write(&sys.join("small.txt"), 1000);
    for i in 0..60 {
        write(&sys.join(format!("many/d{i:02}/f")), 100 * (i + 1));
    }
    let data = sys.join("data");
    write(&data.join("vm/disk.qcow2"), 2 * MIB as usize);
    write(&data.join("photos/p1.jpg"), 600 * 1024);

    let mps: HashSet<PathBuf> = [data.clone()].into_iter().collect();
    let mut snap = Snapshot { version: SNAPSHOT_VERSION, ..Default::default() };
    let mut f0 = fs_info(&sys, 100 * GIB, 40 * GIB, 35 * GIB);
    let r0 = crate::scan::append_tree(&mut snap.tree, walk(&sys, &mps), &mut f0);
    f0.root_node = Some(r0);
    f0.scanned_alloc = snap.tree.node(r0).alloc;
    f0.denied_dirs = 3;
    let mut f1 = fs_info(&data, 200 * GIB, 190 * GIB, 190 * GIB);
    let r1 = crate::scan::append_tree(&mut snap.tree, walk(&data, &HashSet::new()), &mut f1);
    f1.root_node = Some(r1);
    f1.scanned_alloc = snap.tree.node(r1).alloc;
    snap.tree.roots = vec![r0, r1];
    snap.filesystems = vec![f0, f1];
    snap.filesystems.push(FsInfo {
        mount_point: "/mnt/win".into(),
        scan_root: "/mnt/win".into(),
        fstype: "ntfs3".into(),
        statvfs: Some(StatVfs { total: 500 * GIB, free: 100 * GIB, avail: 100 * GIB, ..Default::default() }),
        skipped_reason: Some("excluded by --exclude-fs".into()),
        ..Default::default()
    });

    snap.meta = ScanMeta {
        host: "testhost".into(),
        started: crate::util::now_secs() - 3 * 3600,
        euid: 1000,
        ..Default::default()
    };
    let s = |x: &str| x.to_string();
    let dev = |kname: &str, name: &str, dtype: &str, size: u64, parents: &[&str]| BlockDev {
        kname: s(kname),
        name: s(name),
        path: format!("/dev/{name}"),
        dtype: s(dtype),
        size,
        parents: parents.iter().map(|p| s(p)).collect(),
        ..Default::default()
    };
    let sysp = sys.to_string_lossy().into_owned();
    let datap = data.to_string_lossy().into_owned();
    snap.block = vec![
        BlockDev { model: Some("Samsung 990".into()), ..dev("nvme0n1", "nvme0n1", "disk", 512 * GIB, &[]) },
        BlockDev {
            fstype: Some("vfat".into()),
            mountpoints: vec![s("/boot")],
            ..dev("nvme0n1p1", "nvme0n1p1", "part", GIB, &["nvme0n1"])
        },
        BlockDev {
            fstype: Some("LVM2_member".into()),
            ..dev("nvme0n1p2", "nvme0n1p2", "part", 400 * GIB, &["nvme0n1"])
        },
        BlockDev {
            fstype: Some("ext4".into()),
            mountpoints: vec![sysp.clone()],
            ..dev("dm-0", "vg-root", "lvm", 100 * GIB, &["nvme0n1p2"])
        },
        BlockDev {
            fstype: Some("ext4".into()),
            mountpoints: vec![datap.clone()],
            ..dev("dm-1", "vg-data", "lvm", 200 * GIB, &["nvme0n1p2"])
        },
        dev("dm-3", "vg-lv_vm_test", "lvm", 20 * GIB, &["nvme0n1p2"]),
        dev("dm-4", "vg-lv_old", "lvm", 30 * GIB, &["nvme0n1p2"]),
    ];
    snap.lvm = LvmInfo {
        source: "lvm2".into(),
        vgs: vec![Vg { name: "vg".into(), size: 400 * GIB, free: 50 * GIB, ..Default::default() }],
        pvs: vec![Pv { name: "/dev/nvme0n1p2".into(), vg: "vg".into(), size: 400 * GIB, free: Some(50 * GIB) }],
        lvs: vec![
            Lv {
                vg: "vg".into(),
                name: "root".into(),
                kname: Some("dm-0".into()),
                mountpoints: vec![sysp.clone()],
                ..Default::default()
            },
            Lv {
                vg: "vg".into(),
                name: "lv_vm_test".into(),
                size: 20 * GIB,
                kname: Some("dm-3".into()),
                ..Default::default()
            },
            Lv {
                vg: "vg".into(),
                name: "lv_old".into(),
                size: 30 * GIB,
                kname: Some("dm-4".into()),
                ..Default::default()
            },
        ],
    };
    snap.swaps = vec![Swap { path: "/swapfile".into(), kind: "file".into(), size: 8 * GIB, used: GIB }];

    let cache_dir = format!("{sysp}/cache");
    snap.entities = vec![
        Entity {
            kind: "cache".into(),
            name: "Package cache".into(),
            provider: "classifier".into(),
            group: "Caches".into(),
            paths: vec![cache_dir.clone()],
            reclaim: Some(Reclaim {
                risk: Risk::Safe,
                reason: "regenerated on demand".into(),
                estimate: None,
                action: Some(ActionSpec {
                    label: "empty cache".into(),
                    steps: vec![ActionStep::EmptyDir { path: cache_dir }],
                }),
            }),
            ..Default::default()
        },
        Entity {
            kind: "vm".into(),
            name: "win11".into(),
            provider: "libvirt".into(),
            group: "VMs (libvirt)".into(),
            attrs: vec![(s("state"), s("shut off"))],
            ..Default::default()
        },
        Entity {
            kind: "vm.disk".into(),
            name: "disk.qcow2".into(),
            provider: "libvirt".into(),
            group: "VMs (libvirt)".into(),
            parent: Some(1),
            paths: vec![format!("{datap}/vm/disk.qcow2"), "/var/lib/libvirt/images/gone.qcow2".into()],
            virtual_size: Some(64 * GIB),
            reported: Some(2 * MIB),
            reclaim: Some(Reclaim {
                risk: Risk::Danger,
                reason: "VM disk of a shut-off VM".into(),
                estimate: None,
                action: Some(ActionSpec {
                    label: "delete disk".into(),
                    steps: vec![ActionStep::DeletePath { path: format!("{datap}/vm/disk.qcow2"), trash: false }],
                }),
            }),
            ..Default::default()
        },
        Entity {
            kind: "lv".into(),
            name: "lv_vm_test".into(),
            provider: "lvm".into(),
            group: "LVM volumes".into(),
            block_devs: vec![s("dm-3")],
            external_bytes: 20 * GIB,
            reclaim: Some(Reclaim {
                risk: Risk::Review,
                reason: "not mounted, not used by a VM".into(),
                estimate: None,
                action: Some(ActionSpec {
                    label: "remove LV".into(),
                    steps: vec![ActionStep::Command { argv: vec![s("lvremove"), s("vg/lv_vm_test")], root: true }],
                }),
            }),
            ..Default::default()
        },
    ];
    crate::model::attribution::attribute(&mut snap);
    snap.deleted_open = vec![DeletedOpen {
        pid: 4242,
        comm: "java".into(),
        fd: 7,
        path: "/var/log/app.log".into(),
        alloc: 1536 * MIB,
        apparent: 1536 * MIB,
        fs: Some(0),
        ..Default::default()
    }];
    snap.hidden = vec![HiddenUnderMount { fs: 0, path: datap, alloc: 300 * MIB, apparent: 300 * MIB, items: 12 }];
    snap.providers = vec![
        ProviderReport { name: "docker".into(), coverage: Coverage::Complete, notes: vec![], duration_ms: 12 },
        ProviderReport {
            name: "libvirt".into(),
            coverage: Coverage::Partial,
            notes: vec!["system VMs need root".into()],
            duration_ms: 40,
        },
        ProviderReport {
            name: "hidden-under-mounts".into(),
            coverage: Coverage::Complete,
            notes: vec![],
            duration_ms: 3,
        },
    ];
    (td, snap)
}

pub fn buffer_text(buf: &Buffer) -> String {
    let a = buf.area;
    let mut s = String::new();
    for y in a.y..a.y + a.height {
        let mut line = String::new();
        for x in a.x..a.x + a.width {
            line.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

pub fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| ui::draw(app, f)).unwrap();
    buffer_text(t.backend().buffer())
}

fn key(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn keys(app: &mut App, s: &str) {
    for c in s.chars() {
        key(app, KeyCode::Char(c));
    }
}

fn sel_name(app: &mut App) -> String {
    let (snap, t) = (&app.snap, &app.targets);
    let n = app.files.selected(snap, t).unwrap();
    snap.tree.name(n).into_owned()
}

fn cur_path(app: &App) -> String {
    app.files.cur.map(|d| app.snap.tree.path(d)).unwrap_or_default()
}

fn app() -> (tempfile::TempDir, App) {
    let (td, snap) = synth();
    let mut a = App::new(snap, None);
    a.run_as_root = false;
    (td, a)
}

#[test]
fn renders_every_tab() {
    let (_td, mut app) = app();
    let out = render(&mut app, 140, 45);
    for s in [
        "diskeye",
        "testhost",
        "as user",
        "⚠ gaps: 1 provider, 3 unreadable dirs",
        "1 Physical",
        "6 Diff",
        "PHYSICAL LAYOUT",
    ] {
        assert!(out.contains(s), "header/physical missing {s:?}\n{out}");
    }
    for s in ["nvme0n1 (Samsung 990)", "VG vg", "unallocated in VG vg", "! LV vg/lv_old", "/mnt/win", "swapfile"] {
        assert!(out.contains(s), "physical missing {s:?}\n{out}");
    }
    assert!(!out.contains("! LV vg/lv_vm_test"), "LV owned by an entity is not 'unused'");

    key(&mut app, KeyCode::Char('2'));
    let out = render(&mut app, 140, 45);
    for s in [
        "many/",
        "big/",
        "cache/",
        "data/",
        "[mountpoint]",
        "[cache: Package cache]",
        "small.txt",
        "sorted by disk usage",
    ] {
        assert!(out.contains(s), "files missing {s:?}\n{out}");
    }

    key(&mut app, KeyCode::Char('3'));
    let out = render(&mut app, 140, 45);
    for s in ["VMs (libvirt)", "LVM volumes", "Caches", "lv_vm_test", "total", "unique", "virtual", "♻ 768"] {
        assert!(out.contains(s), "workloads missing {s:?}\n{out}");
    }

    key(&mut app, KeyCode::Char('4'));
    let out = render(&mut app, 140, 45);
    for s in ["safe", "danger", "Package cache", "VM disk: disk.qcow2", "selected: 0 items"] {
        assert!(out.contains(s), "reclaim missing {s:?}\n{out}");
    }

    key(&mut app, KeyCode::Char('5'));
    let out = render(&mut app, 140, 60);
    for s in [
        "WHERE THE SPACE GOES",
        "deleted-open 1.5 GiB",
        "hidden under mounts 300",
        "4242",
        "java",
        "UNREADABLE DIRECTORIES",
        "re-run with sudo",
        "PROVIDER COVERAGE",
        "libvirt",
        "Partial",
        "not walked: /mnt/win",
    ] {
        assert!(out.contains(s), "reconcile missing {s:?}\n{out}");
    }

    // Diff with an injected result (no saved snapshots needed).
    key(&mut app, KeyCode::Char('6'));
    app.diff.candidates = Some(vec![]);
    let out = render(&mut app, 140, 45);
    assert!(out.contains("No other snapshot"), "{out}");
}

#[test]
fn diff_tab_loads_in_background_and_switches_baseline() {
    let (td, mut snap) = synth();
    let old = snap.clone();
    // Make `big` grow by 600 MiB in the new snapshot.
    let big = snap.lookup(&format!("{}/sys/big", td.path().display())).unwrap();
    for a in snap.tree.ancestors(big).collect::<Vec<_>>() {
        snap.tree.nodes[a as usize].alloc += 600 * MIB;
    }
    snap.filesystems[0].statvfs.as_mut().unwrap().free -= 600 * MIB;
    let p1 = td.path().join("a-old.dkeye");
    let p2 = td.path().join("b-old.dkeye");
    crate::model::snapshot::save(&old, &p1).unwrap();
    let mut older = old.clone();
    older.meta.started -= 86_400;
    crate::model::snapshot::save(&older, &p2).unwrap();
    let mut app = App::new(snap, None);
    app.tab = Tab::Diff;
    app.diff.candidates = Some(vec![p2.clone(), p1.clone()]);
    app.diff.idx = 1;
    let out = render(&mut app, 140, 40);
    let t0 = std::time::Instant::now();
    while !app.diff.results.contains_key(&1) {
        app.tick();
        assert!(t0.elapsed().as_secs() < 30, "diff did not finish");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(out.contains("baseline"), "{out}");
    let out = render(&mut app, 140, 40);
    for s in ["a-old.dkeye", "FILESYSTEMS", "WHERE IT CHANGED", "/sys/big", "+600", "(2 of 2)"] {
        assert!(out.contains(s), "diff missing {s:?}\n{out}");
    }
    // Enter on a hotspot opens it in Files.
    let rows = app.rows_for(Tab::Diff);
    let i = rows.iter().position(|r| r.target == Some(Target::Node(big))).expect("hotspot row targets big");
    app.diff.view.cursor = i;
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.tab, Tab::Files);
    assert!(cur_path(&app).ends_with("/sys/big"));
    // `[` picks the older baseline and loads it.
    app.tab = Tab::Diff;
    key(&mut app, KeyCode::Char('['));
    assert_eq!(app.diff.idx, 0);
    let out = render(&mut app, 140, 40);
    assert!(out.contains("b-old.dkeye"), "{out}");
    while !app.diff.results.contains_key(&0) {
        app.tick();
        assert!(t0.elapsed().as_secs() < 30);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn diff_candidates_default_to_previous() {
    let l = vec![PathBuf::from("/x/a"), PathBuf::from("/x/b"), PathBuf::from("/x/c")];
    let (c, i) = diffview::pick_candidates(l.clone(), Some(Path::new("/x/b")));
    assert_eq!(c, vec![PathBuf::from("/x/a"), PathBuf::from("/x/c")]);
    assert_eq!(c[i], PathBuf::from("/x/a"));
    let (c, i) = diffview::pick_candidates(l.clone(), None);
    assert_eq!(c[i], PathBuf::from("/x/c"));
    let (c, i) = diffview::pick_candidates(l, Some(Path::new("/x/c")));
    assert_eq!(c[i], PathBuf::from("/x/b"));
}

#[test]
fn files_navigation_restores_selection_and_stitches_mounts() {
    let (td, mut app) = app();
    let base = td.path().join("sys").display().to_string();
    key(&mut app, KeyCode::Char('2'));
    render(&mut app, 120, 30);
    assert_eq!(cur_path(&app), base);
    // Sorted by disk usage: `big` (4 MiB) is first.
    assert_eq!(sel_name(&mut app), "big");

    // Descend into the mountpoint `data` -> lands in the other filesystem's root.
    let entries = app.files.entries(&app.snap, &app.targets);
    let data_i = entries.iter().position(|&n| app.snap.tree.name(n) == "data").unwrap();
    app.files.sel = data_i;
    key(&mut app, KeyCode::Enter);
    assert_eq!(cur_path(&app), format!("{base}/data"));
    assert_eq!(app.files.cur, app.snap.filesystems[1].root_node);
    assert_eq!(sel_name(&mut app), "vm");
    key(&mut app, KeyCode::Left);
    assert_eq!(cur_path(&app), base);
    assert_eq!(app.files.sel, data_i, "selection restored on the mountpoint");

    // Deep list with scrolling: select the 46th entry, descend, come back.
    let many = app.files.entries(&app.snap, &app.targets);
    let mi = many.iter().position(|&n| app.snap.tree.name(n) == "many").unwrap();
    app.files.sel = mi;
    key(&mut app, KeyCode::Char('l'));
    render(&mut app, 120, 20);
    for _ in 0..45 {
        key(&mut app, KeyCode::Down);
    }
    let want = sel_name(&mut app);
    let off = app.files.offset;
    assert!(off > 0);
    key(&mut app, KeyCode::Enter);
    assert!(cur_path(&app).ends_with(&want));
    key(&mut app, KeyCode::Backspace);
    assert_eq!(sel_name(&mut app), want);
    assert_eq!(app.files.offset, off, "scroll offset restored");
    key(&mut app, KeyCode::Char('h'));
    assert_eq!(sel_name(&mut app), "many");

    // Files can't be entered; going above the only root is a no-op.
    let small = app.files.entries(&app.snap, &app.targets);
    app.files.sel = small.iter().position(|&n| app.snap.tree.name(n) == "small.txt").unwrap();
    key(&mut app, KeyCode::Enter);
    assert!(app.status.as_ref().unwrap().0.contains("not a directory"));
    key(&mut app, KeyCode::Left);
    assert_eq!(cur_path(&app), base);
}

#[test]
fn files_metric_sort_filter() {
    let (_td, mut app) = app();
    key(&mut app, KeyCode::Char('2'));
    render(&mut app, 120, 30);
    // Items metric: `many` (121 entries) wins.
    key(&mut app, KeyCode::Char('s'));
    assert_eq!(app.files.metric, crate::views::Metric::Apparent);
    key(&mut app, KeyCode::Char('s'));
    assert_eq!(app.files.metric, crate::views::Metric::Items);
    key(&mut app, KeyCode::Home);
    assert_eq!(sel_name(&mut app), "many");
    let out = render(&mut app, 120, 30);
    assert!(out.contains("sorted by item count"), "{out}");
    key(&mut app, KeyCode::Char('s'));
    // Name sort: directories first, alphabetical.
    key(&mut app, KeyCode::Char('n'));
    key(&mut app, KeyCode::Home);
    assert_eq!(sel_name(&mut app), "big");
    let names: Vec<String> =
        app.files.entries(&app.snap, &app.targets).iter().map(|&n| app.snap.tree.name(n).into_owned()).collect();
    assert_eq!(names, vec!["big", "cache", "data", "docs", "many", "small.txt"]);
    // Filter.
    key(&mut app, KeyCode::Char('/'));
    keys(&mut app, "CA");
    assert_eq!(app.files.entries(&app.snap, &app.targets).len(), 1);
    let out = render(&mut app, 120, 30);
    assert!(out.contains("filter: CA"), "{out}");
    key(&mut app, KeyCode::Enter);
    assert!(!app.files.filter_editing);
    assert_eq!(sel_name(&mut app), "cache");
    // `q` while not editing still quits, Esc clears the filter first.
    key(&mut app, KeyCode::Esc);
    assert!(app.files.filter.is_empty());
    assert!(!app.quit);
    assert_eq!(app.files.entries(&app.snap, &app.targets).len(), 6);
}

#[test]
fn treemap_renders_and_navigates() {
    let (_td, mut app) = app();
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('t'));
    let out = render(&mut app, 100, 30);
    assert!(out.contains("big/"), "{out}");
    assert!(out.contains("many/"), "{out}");
    let before = sel_name(&mut app);
    let mut seen = HashSet::new();
    for d in [KeyCode::Right, KeyCode::Down, KeyCode::Left, KeyCode::Up, KeyCode::Right, KeyCode::Right] {
        key(&mut app, d);
        seen.insert(sel_name(&mut app));
    }
    assert!(seen.len() >= 2, "arrows move between blocks: {seen:?} from {before}");
    // Enter descends into the selected directory.
    let entries = app.files.entries(&app.snap, &app.targets);
    app.files.sel = entries.iter().position(|&n| app.snap.tree.name(n) == "big").unwrap();
    key(&mut app, KeyCode::Enter);
    assert!(cur_path(&app).ends_with("/big"));
    let out = render(&mut app, 100, 30);
    assert!(out.contains("a.bin"), "{out}");
    key(&mut app, KeyCode::Backspace);
    assert_eq!(sel_name(&mut app), "big");
    key(&mut app, KeyCode::Esc);
    assert!(!app.files.treemap, "Esc leaves the treemap first");
}

#[test]
fn owner_jump_and_cleanup_flow() {
    let (_td, mut app) = app();
    key(&mut app, KeyCode::Char('2'));
    let entries = app.files.entries(&app.snap, &app.targets);
    app.files.sel = entries.iter().position(|&n| app.snap.tree.name(n) == "cache").unwrap();
    // g: owner in Workloads.
    key(&mut app, KeyCode::Char('g'));
    assert_eq!(app.tab, Tab::Workloads);
    let out = render(&mut app, 140, 40);
    assert!(out.contains("cache: Package cache"), "{out}");
    assert!(out.contains("regenerated on demand"), "details pane shows reclaim info\n{out}");
    // d from Files: preview with describe + preflight.
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Char('d'));
    assert!(matches!(app.popup, Some(Popup::Preview { .. })));
    let out = render(&mut app, 140, 40);
    for s in ["cleanup preview", "delete contents of:", "Preflight: ok"] {
        assert!(out.contains(s), "preview missing {s:?}\n{out}");
    }
    // x → confirmation needing "yes".
    key(&mut app, KeyCode::Char('x'));
    let Some(Popup::Confirm { strict, .. }) = &app.popup else { panic!("confirm popup") };
    assert!(!strict);
    let out = render(&mut app, 140, 40);
    assert!(out.contains("Type yes"), "{out}");
    keys(&mut app, "no");
    key(&mut app, KeyCode::Enter);
    assert!(app.pending_exec.is_none());
    assert!(app.status.as_ref().unwrap().0.contains("did not match"));
    key(&mut app, KeyCode::Char('d'));
    key(&mut app, KeyCode::Char('x'));
    keys(&mut app, "yes");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.pending_exec.as_deref(), Some(&[app.reclaim.iter().position(|r| r.entity == 0).unwrap()][..]));
    // Simulate the run loop marking it done.
    let i = app.pending_exec.take().unwrap()[0];
    app.mark_done(i);
    key(&mut app, KeyCode::Char('4'));
    let out = render(&mut app, 140, 40);
    assert!(out.contains("stale until you re-scan"), "{out}");
    assert!(out.contains("✓"), "{out}");
}

#[test]
fn reclaim_marks_and_danger_confirmation() {
    let (_td, mut app) = app();
    key(&mut app, KeyCode::Char('4'));
    // Order: safe, review, danger.
    let risks: Vec<Risk> = app.reclaim.iter().map(|r| r.risk).collect();
    assert_eq!(risks, vec![Risk::Safe, Risk::Review, Risk::Danger]);
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Char(' '));
    let out = render(&mut app, 140, 30);
    assert!(out.contains("selected: 2 items, 20"), "{out}");
    // Batch of safe+review as non-root: confirmation with "yes"; review needs root → preflight fails.
    key(&mut app, KeyCode::Char('x'));
    assert!(app.popup.is_none());
    let st = &app.status.as_ref().unwrap().0;
    assert!(st.contains("lv_vm_test") && st.contains("cannot run"), "{st}");
    // A danger item needs `delete`, not `yes`.
    key(&mut app, KeyCode::Esc); // clears marks
    assert!(app.recl.marked.is_empty());
    key(&mut app, KeyCode::End);
    key(&mut app, KeyCode::Char('x'));
    let Some(Popup::Confirm { strict, .. }) = &app.popup else { panic!("confirm popup: {:?}", app.status) };
    assert!(strict);
    let out = render(&mut app, 140, 30);
    assert!(out.contains("Type delete"), "{out}");
    keys(&mut app, "yes");
    key(&mut app, KeyCode::Enter);
    assert!(app.pending_exec.is_none(), "typing yes is not enough for danger items");
    // A batch with a danger item runs together after one `delete`.
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Home);
    key(&mut app, KeyCode::Char(' '));
    key(&mut app, KeyCode::Char('x'));
    assert!(matches!(app.popup, Some(Popup::Confirm { strict: true, .. })), "{:?}", app.status);
    keys(&mut app, "delete");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.pending_exec.take(), Some(vec![0, 2]));
    // Root: even safe items need `delete`.
    app.run_as_root = true;
    app.recl.marked.clear();
    app.recl.sel = 0;
    key(&mut app, KeyCode::Char('x'));
    assert!(matches!(app.popup, Some(Popup::Confirm { strict: true, .. })));
    let out = render(&mut app, 140, 30);
    assert!(out.contains("running as root"), "{out}");
    key(&mut app, KeyCode::Esc);
    assert!(app.popup.is_none());
    // `A` marks every item that has an action.
    app.run_as_root = false;
    key(&mut app, KeyCode::Char('A'));
    assert_eq!(app.recl.marked.len(), app.reclaim.iter().filter(|r| r.action.is_some()).count());
}

#[test]
fn workloads_tree_and_path_jump() {
    let (td, mut app) = app();
    key(&mut app, KeyCode::Char('3'));
    render(&mut app, 140, 40);
    let rows = super::workloads::rows(&app);
    let vm = rows.iter().position(|r| matches!(r, super::workloads::WRow::Entity { id: 1, .. })).expect("win11 row");
    app.work.sel = vm;
    // Enter on an entity without own paths expands it.
    key(&mut app, KeyCode::Enter);
    let out = render(&mut app, 140, 40);
    assert!(out.contains("VM disk: disk.qcow2"), "{out}");
    key(&mut app, KeyCode::Down);
    let out = render(&mut app, 140, 40);
    for s in ["virtual     64 GiB", "/var/lib/libvirt/images/gone.qcow2", "not in scan", "danger"] {
        assert!(out.contains(s), "details missing {s:?}\n{out}");
    }
    // Enter focuses the path list; Enter opens it in Files.
    key(&mut app, KeyCode::Enter);
    assert!(app.work.focus_paths);
    render(&mut app, 140, 40);
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.tab, Tab::Files);
    assert!(cur_path(&app).ends_with("/sys/data/vm"), "{}", cur_path(&app));
    assert_eq!(sel_name(&mut app), "disk.qcow2");
    // The stack goes back through the mount stitch.
    key(&mut app, KeyCode::Left);
    assert!(cur_path(&app).ends_with("/sys/data"));
    key(&mut app, KeyCode::Left);
    assert_eq!(cur_path(&app), td.path().join("sys").display().to_string());
    assert_eq!(sel_name(&mut app), "data");
    // Collapse a group with ←.
    key(&mut app, KeyCode::Char('3'));
    assert!(app.work.focus_paths, "coming back keeps the path focus");
    key(&mut app, KeyCode::Esc);
    assert!(!app.work.focus_paths && !app.quit);
    app.work.sel = 0;
    let before = super::workloads::rows(&app).len();
    key(&mut app, KeyCode::Left);
    assert!(super::workloads::rows(&app).len() < before);
}

#[test]
fn physical_enter_opens_filesystem_and_global_keys() {
    let (td, mut app) = app();
    render(&mut app, 140, 45);
    let rows = app.rows_for(Tab::Physical);
    let root = app.snap.filesystems[1].root_node.unwrap();
    let i = rows.iter().position(|r| r.target == Some(Target::Node(root))).expect("vg-data row");
    // Walk the cursor down to it with the keyboard.
    while app.phys.cursor < i {
        key(&mut app, KeyCode::Down);
    }
    assert_eq!(app.phys.cursor, i);
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.tab, Tab::Files);
    assert_eq!(cur_path(&app), format!("{}/sys/data", td.path().display()));

    key(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Workloads);
    key(&mut app, KeyCode::BackTab);
    key(&mut app, KeyCode::BackTab);
    assert_eq!(app.tab, Tab::Physical);
    key(&mut app, KeyCode::Char('?'));
    let out = render(&mut app, 120, 40);
    assert!(out.contains("keys — any key closes"), "{out}");
    key(&mut app, KeyCode::Char('z'));
    assert!(app.popup.is_none());
    let out = render(&mut app, 40, 8);
    assert!(out.contains("terminal too small"), "{out}");
    key(&mut app, KeyCode::Esc);
    assert!(app.quit);
}

#[test]
fn every_tab_survives_odd_sizes() {
    let (_td, mut app) = app();
    for (w, h) in [(60, 12), (61, 13), (80, 24), (99, 30), (110, 20), (250, 70), (59, 40), (300, 5)] {
        for t in 0..6 {
            app.tab = Tab::ALL[t];
            for tm in [false, true] {
                app.files.treemap = tm;
                render(&mut app, w, h);
            }
            for k in [KeyCode::Down, KeyCode::PageDown, KeyCode::End, KeyCode::Up, KeyCode::Home] {
                key(&mut app, k);
                render(&mut app, w, h);
            }
        }
        app.tab = Tab::Reclaim;
        key(&mut app, KeyCode::Enter);
        render(&mut app, w, h);
        key(&mut app, KeyCode::Esc);
        app.popup = Some(Popup::Help);
        render(&mut app, w, h);
        app.popup = None;
    }
    // An empty snapshot renders too.
    let mut empty = App::new(Snapshot::default(), None);
    for t in 0..6 {
        empty.tab = Tab::ALL[t];
        if empty.tab == Tab::Diff {
            empty.diff.candidates = Some(vec![]);
        }
        let out = render(&mut empty, 100, 30);
        assert!(out.contains("diskeye"));
        key(&mut empty, KeyCode::Enter);
        key(&mut empty, KeyCode::Down);
    }
}

#[test]
fn reconcile_bar_widths() {
    assert_eq!(reconcile::split_widths(&[50, 50], 10), vec![5, 5]);
    let w = reconcile::split_widths(&[1000, 1, 0, 3, 0, 996], 40);
    assert_eq!(w.iter().sum::<usize>(), 40);
    assert!(w[1] >= 1 && w[3] >= 1 && w[2] == 0);
    assert_eq!(reconcile::split_widths(&[0, 0], 10), vec![0, 0]);
}

#[test]
fn files_jump_to_top_level_with_multiple_roots() {
    // Two unrelated roots -> a virtual top level.
    let (_td, mut snap) = synth();
    snap.filesystems[1].scan_root = "/elsewhere".into();
    snap.filesystems[1].mount_point = "/elsewhere".into();
    let mut app = App::new(snap, None);
    assert_eq!(app.files.cur, None);
    key(&mut app, KeyCode::Char('2'));
    let out = render(&mut app, 120, 30);
    assert!(out.contains("all scanned filesystems"), "{out}");
    let r1 = app.snap.filesystems[1].root_node.unwrap();
    app.goto(Target::Node(r1));
    assert_eq!(app.files.cur, Some(r1));
    key(&mut app, KeyCode::Left);
    assert_eq!(app.files.cur, None);
    assert_eq!(app.files.selected(&app.snap, &app.targets), Some(r1));
}

/// Render every tab of a real snapshot and time it:
/// `DISKEYE_REAL_SNAPSHOT=path cargo test real_snapshot -- --ignored --nocapture`
#[test]
#[ignore]
fn real_snapshot_perf() {
    use std::time::Instant;
    let Ok(path) = std::env::var("DISKEYE_REAL_SNAPSHOT") else {
        eprintln!("DISKEYE_REAL_SNAPSHOT not set; skipping");
        return;
    };
    let t = Instant::now();
    let snap = crate::model::snapshot::load(Path::new(&path)).unwrap();
    let load = t.elapsed();
    let nodes = snap.tree.len();
    let t = Instant::now();
    let mut app = App::new(snap, Some(PathBuf::from(&path)));
    let init = t.elapsed();
    let t = Instant::now();
    let phys = render(&mut app, 160, 50);
    let first = t.elapsed();
    println!("nodes {nodes}: load {load:?}, App::new {init:?}, first frame {first:?}");
    println!("{phys}");
    for tab in ["2", "3", "4", "5"] {
        key(&mut app, KeyCode::Char(tab.chars().next().unwrap()));
        let t = Instant::now();
        let out = render(&mut app, 160, 50);
        println!("tab {tab}: {:?}", t.elapsed());
        if tab == "2" || tab == "5" {
            println!("{out}");
        }
    }
    // The directory with the most direct children.
    let big = (0..nodes as u32).max_by_key(|&n| app.snap.tree.node(n).child_count).unwrap();
    let count = app.snap.tree.node(big).child_count;
    let t = Instant::now();
    app.goto(Target::Node(big));
    let out = render(&mut app, 160, 50);
    println!("jump into {} ({count} children): {:?}", app.snap.tree.path(big), t.elapsed());
    let t = Instant::now();
    for _ in 0..200 {
        key(&mut app, KeyCode::Down);
    }
    render(&mut app, 160, 50);
    println!("200 × Down + frame: {:?}", t.elapsed());
    let t = Instant::now();
    key(&mut app, KeyCode::Char('s'));
    render(&mut app, 160, 50);
    println!("re-sort by apparent + frame: {:?}", t.elapsed());
    let t = Instant::now();
    key(&mut app, KeyCode::Char('/'));
    keys(&mut app, "a");
    render(&mut app, 160, 50);
    println!("filter 'a' + frame: {:?}", t.elapsed());
    key(&mut app, KeyCode::Esc);
    let t = Instant::now();
    key(&mut app, KeyCode::Char('t'));
    let tm = render(&mut app, 160, 50);
    println!("treemap frame: {:?}", t.elapsed());
    let _ = out;
    // Walk down the largest child chain from the top and back up.
    key(&mut app, KeyCode::Char('t'));
    key(&mut app, KeyCode::Char('s'));
    key(&mut app, KeyCode::Char('s'));
    let top = app.files.top.clone();
    app.goto(Target::Node(top[0]));
    let t = Instant::now();
    let mut depth = 0;
    for _ in 0..12 {
        key(&mut app, KeyCode::Home);
        let before = app.files.cur;
        key(&mut app, KeyCode::Enter);
        render(&mut app, 160, 50);
        if app.files.cur == before {
            break;
        }
        depth += 1;
    }
    for _ in 0..depth {
        key(&mut app, KeyCode::Left);
        render(&mut app, 160, 50);
    }
    println!("descend {depth} levels and back (with frames): {:?}", t.elapsed());
    let _ = tm;
    // Review screens: list and treemap of the home filesystem.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home".into());
    if let Some(home) = app.snap.lookup(&home) {
        let home = crate::views::mount_target(&app.snap, &app.targets, home).unwrap_or(home);
        app.goto(Target::Node(home));
        println!("{}", render(&mut app, 120, 30));
        key(&mut app, KeyCode::Char('t'));
        println!("{}", render(&mut app, 120, 30));
        key(&mut app, KeyCode::Char('t'));
    }
    // Physical, scrolled to the hotspots.
    app.tab = Tab::Physical;
    key(&mut app, KeyCode::End);
    println!("{}", render(&mut app, 140, 40));
    // Diff of the snapshot against itself (worst case: full size maps of both).
    let t = Instant::now();
    let d = crate::model::diff::diff(&app.snap, &app.snap, diffview::THRESHOLD);
    println!("diff self vs self: {:?} ({} hotspots)", t.elapsed(), d.hotspots.len());
    app.tab = Tab::Diff;
    app.diff.candidates = Some(vec![PathBuf::from(&path)]);
    app.diff.idx = 0;
    app.diff.results.insert(0, Ok(Rc::new(d)));
    println!("{}", render(&mut app, 160, 30));
}
