# diskeye

A god's-eye view of what is using disk space on a Linux system, and where.

`ncdu` shows you a directory tree. `diskeye` puts **every byte on the machine in one model**, from physical disk down to individual file:

```
disk → partition → LUKS/RAID/PV → VG → LV → filesystem → mount → directory → file
```

It labels those bytes with **what owns them**: a Docker image or volume, a VM disk, a Kubernetes PVC, a Flatpak runtime, the pacman cache, a forgotten `target/` directory.

It also **reconciles against `df`**, so space a directory walk can't see is accounted for too:

- files deleted but still held open by a process
- files hidden underneath mountpoints
- ext4 reserved blocks
- unallocated LVM extents
- unmounted LVs
- unpartitioned space

Finally, it ranks what you can safely reclaim and can do the cleanup for you, with a preview and confirmation first.

## Quick start

```sh
cargo build --release
./target/release/diskeye                # scan everything, open the TUI
./target/release/diskeye scan           # scan, save a snapshot, print a report
sudo ./target/release/diskeye scan      # full picture (LVM, other users, hidden files, containerd)
./target/release/diskeye serve          # interactive web UI on http://127.0.0.1:7878
```

## What it looks at

| Layer | Source | Without root |
|---|---|---|
| Disks, partitions, dm-crypt, RAID, loop devices | `lsblk`, sysfs | full |
| LVM: VGs, LVs, thin pools, free extents | `lvs/vgs/pvs` | estimated from device-mapper tables |
| Filesystems and reconciliation against `df` | `/proc/self/mountinfo`, `statvfs` | full (unreadable dirs are counted and reported) |
| Files | parallel `getdents64` + `statx` walker, hardlink- and mount-aware | your files |
| Deleted but still open | `/proc/*/fd` | your processes |
| Hidden under mountpoints | private mount namespace + non-recursive bind mount | root only |
| Docker (rootful and rootless), Podman, containerd, k3s, Kubernetes | Engine API over unix socket, `ctr`, `kubectl` | per-user engines |
| libvirt/QEMU VMs, backing chains, orphan disk images, unused LVs | `virsh`, `qemu-img` | `qemu:///system` if you are in the `libvirt` group |
| VirtualBox, Vagrant, GNOME Boxes, Incus/LXD, multipass | tools when present, else known paths | — |
| Flatpak, Snap, journald, coredumps | `flatpak`, `snap`, `journalctl` | — |
| Caches and build artifacts | `rules/builtin.toml` + `~/.config/diskeye/rules.toml` | — |

Entity sizes always come from the bytes the scanner actually measured on disk. The tools' own numbers, for example `docker system df`, are shown next to them as a cross-check. Bytes shared between entities, such as image layers or VM backing files, are reported as shared rather than counted twice.

## Commands

```
diskeye [PATHS] [-x]                     scan + TUI (or a text report when not on a terminal)
diskeye scan    [PATHS] [--json] [-o F]  scan and save a snapshot (keeps the newest 10 by default)
diskeye report  [SNAPSHOT] [--json]      summary of a saved snapshot
diskeye tui     [SNAPSHOT]               terminal UI
diskeye serve   [SNAPSHOT] [--port N]    web UI (localhost only, token-protected)
diskeye diff    [OLD] [NEW]              what grew/shrank between snapshots, and who owns it
diskeye clean   --list | ID... [--safe]  reclaim space for listed items (--safe: every safe one)
diskeye export  --path P [-o F]          ncdu-compatible JSON (`ncdu -f F`)
diskeye providers                        what was detected and how complete each view is
diskeye snapshots                        list saved snapshots
```

Useful scan options:
- `--exclude GLOB` and `--exclude-fs ntfs3` skip paths or filesystem types.
- `--exclude-mount /mnt/x` skips a mount.
- `--tmpfs` and `--network` also walk tmpfs and network filesystems.
- `--no-providers` and `--providers docker,libvirt` limit which providers run.

Snapshots live in `~/.local/state/diskeye/snapshots`. Under `sudo` they are saved to the invoking user's home and owned by that user.

## Safety

- Everything is read-only unless you run an action from `diskeye clean`, the TUI or the web UI.
- Each action shows the exact commands or API calls first, and is checked before it runs. Protected system paths are refused.
- Plain `yes` confirms ordinary items. If a batch contains a `danger` item, or you run as root, you type `delete` instead.
- You can run several items at once: mark them in the TUI (`Space`, `a` for all safe ones, `A` for all), tick them in the web UI, or pass several ids (or `--safe`) to `diskeye clean`. One confirmation covers the whole batch.
- Every action is logged to `~/.local/state/diskeye/actions.log`.
- Deleting a path moves it to the freedesktop Trash by default.
- LVM and partition changes are never automated, only reported.
- The web UI binds to `127.0.0.1` and requires a random token on every API call.

## Daily snapshots

```sh
cp contrib/systemd/diskeye-snapshot.* ~/.config/systemd/user/
systemctl --user enable --now diskeye-snapshot.timer
diskeye diff          # compares the two newest snapshots
```

## Custom rules

`~/.config/diskeye/rules.toml` uses the same format as [`rules/builtin.toml`](rules/builtin.toml). Use it to label your own big directories, mark them reclaimable, and attach a cleanup action.

## Development

```sh
just setup         # once: pre-commit hook, Playwright + Chromium, VHS
just screenshots   # regenerate docs/screenshots from the demo snapshot
```

The pre-commit hook re-runs `just screenshots` when UI code changes. If the images change, the commit stops so you can stage them.

## Screenshots

These are taken from a made-up demo machine (`just screenshots`) and regenerated by the pre-commit hook.

### Web UI (`diskeye serve`)

*Overview*

![Overview](docs/screenshots/web-overview.png)

*Physical layout*

![Physical layout](docs/screenshots/web-physical.png)

*Files treemap*

![Files treemap](docs/screenshots/web-files.png)

*Workloads*

![Workloads](docs/screenshots/web-workloads.png)

*Reclaim*

![Reclaim](docs/screenshots/web-reclaim.png)

*Diff between snapshots*

![Diff between snapshots](docs/screenshots/web-diff.png)

### Terminal UI (`diskeye tui`)

*Physical layout*

![Physical layout](docs/screenshots/tui-physical.png)

*Files*

![Files](docs/screenshots/tui-files.png)

*Treemap*

![Treemap](docs/screenshots/tui-treemap.png)

*Workloads*

![Workloads](docs/screenshots/tui-workloads.png)

*Reclaim*

![Reclaim](docs/screenshots/tui-reclaim.png)

*Reconcile against df*

![Reconcile against df](docs/screenshots/tui-reconcile.png)

*Diff between snapshots*

![Diff between snapshots](docs/screenshots/tui-diff.png)
