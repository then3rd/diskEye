# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

diskeye is a Linux disk-usage analyzer (Rust, edition 2024). It builds one model of every byte on the machine, from physical disk down to individual files. It labels bytes with their owners (Docker, VMs, k8s PVCs, Flatpak, caches…) and reconciles totals against `df`. It has a TUI (ratatui) and a localhost web UI (axum plus vanilla JS/d3).

## Commands

`cargo` may not be on PATH. It lives at `~/.cargo/bin/cargo`, and the justfile falls back to that path automatically.

```sh
just build [--release]          # cargo build
just run tui                    # cargo run --release -- tui
cargo test                      # all tests (unit tests live inside src/, there is no tests/*.rs)
cargo test <name_substring>     # single test, e.g. cargo test parse_lsblk
cargo test --no-default-features --features tui   # build without the web frontend
cargo fmt                       # rustfmt.toml: max_width=120, use_small_heuristics=Max
cargo clippy
just screenshots                # regenerate docs/screenshots from the demo snapshot (needs `just setup`)
```

Ignored tests run against real data:
- `DISKEYE_REAL_SNAPSHOT=path cargo test real_snapshot -- --ignored --nocapture` runs the TUI against a real snapshot.
- There is also an `#[ignore]` docker test in `src/providers/docker.rs`.

A pre-commit hook runs `just screenshots` whenever `src/`, `web/`, `scripts/screenshots/` or the `justfile` changes. If the PNGs change, the commit fails until they are staged.

`DISKEYE_NOW=<unix secs>` freezes `util::now_secs()` so demo or screenshot output is reproducible.

## Architecture

**Pipeline** (`src/pipeline.rs`) builds a `Snapshot` in this order:
1. Filesystem walk (`scan/`):
   - `walker.rs` is a parallel getdents64+statx walker, aware of hardlinks and mounts.
   - `mounts.rs` parses mountinfo and plans scan units.
2. Space outside the tree:
   - `scan/deleted_open.rs` finds files deleted but still open, via `/proc/*/fd`.
   - `scan/hidden.rs` finds files hidden under mountpoints. It is root-only and uses a private mount namespace. It runs as a re-exec'd hidden subcommand, which `main.rs` dispatches *before any threads start*. Keep it that way.
3. Providers run in the fixed order given by `providers::all()`.
4. `model::attribution::attribute` turns entity path claims into measured sizes.

**Model** (`src/model/`):
- `Snapshot` is the single data structure every frontend consumes.
- `tree.rs` is the arena file tree, indexed by `NodeId`.
- Snapshots are saved as `DKEYE` magic + `SNAPSHOT_VERSION` + zstd(postcard). If you change serialized model types, bump `SNAPSHOT_VERSION` in `model/mod.rs`. Old snapshots are rejected, not migrated.
- `diff.rs` compares snapshots.
- `ncdu.rs` handles ncdu JSON export.

**Providers** (`src/providers/`):
- Each provider implements `Provider::collect(&Ctx, &mut Snapshot) -> Outcome`. It adds `Entity`s that claim paths, and reports `Coverage` (Complete/Partial/Denied/Absent), so gaps without root are flagged rather than hidden.
- Entity sizes come from the scanned tree (attribution), not from the tools' own numbers. Tool-reported sizes are only kept as a cross-check.
- Order matters. A path claimed by an earlier provider is never claimed again, so `classifier` (the generic path and dir rules from `rules/builtin.toml`, embedded via `include_str!`, plus `~/.config/diskeye/rules.toml`) must stay last.
- External commands go through the `CommandRunner` trait (`runner.rs`). Tests use `FakeRunner::with(argv, output)`, which replays recorded output from `tests/fixtures/<provider>/`. Docker talks to the Engine API over a unix socket behind its own `Api` trait, with `FakeApi` in tests.

**Views** (`src/views.rs`) are view models shared by the text report (`report.rs`), the TUI and the web API. Put projection logic here rather than duplicating it per frontend.

**Frontends**: both are optional cargo features, `tui` and `web`, and both are on by default.
- `src/tui/`: `app.rs` is the key-driven state machine, with one module per tab. `tui/tests.rs` builds a synthetic snapshot (`synth()`) and renders it into ratatui's `TestBackend`.
- `src/web/`: `api.rs` holds the JSON API. The `web/` directory (app.js, charts.js, app.css, vendored d3) is embedded into the binary with `rust-embed`, so frontend changes need a rebuild. The server binds 127.0.0.1 only and requires a random token on every API call.

**Actions** (`src/actions/mod.rs`) handle guided cleanup. Specs are previewed (`describe`) and checked (`preflight`, with protected paths refused) before they run. Every action is logged to `~/.local/state/diskeye/actions.log`, and path deletions go to the freedesktop Trash. To confirm, the user types `yes`, or `delete` (`STRICT_WORD`) when the batch contains a `danger` item or the process runs as root. One confirmation covers a whole batch.

**Demo** (`src/demo.rs`, `diskeye demo <dir>`) generates a made-up snapshot. It is used for the README screenshots (`scripts/screenshots/tui.tape` via VHS, `web.mjs` via Playwright).

## Invariants

- Read-only unless the user explicitly runs an action. LVM and partition changes are only reported, never automated.
- The tool must work unprivileged, with gaps flagged through `Coverage` and provider notes. Under `sudo`, use the invoking user's identity (`util::invoking_ids`, `invoking_home`). Snapshots are saved to that user's home and owned by them.
- Snapshot retention is bounded (newest 10 by default), because snapshots are large.
