use crate::model::{self, Snapshot, snapshot};
use crate::pipeline::{self, PipelineOptions};
use crate::scan::{ScanOptions, mounts::Selection};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "diskeye", version, about = "A god's-eye view of what uses disk space on this Linux system")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    scan: ScanArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan the system and save a snapshot (prints a summary).
    Scan {
        #[command(flatten)]
        scan: ScanArgs,
        /// Write the snapshot here instead of the default state directory.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Don't save a snapshot.
        #[arg(long)]
        no_save: bool,
        /// Keep only this many snapshots in the default directory (0 = keep all).
        #[arg(long, default_value_t = DEFAULT_KEEP)]
        keep: usize,
        /// Print the summary as JSON.
        #[arg(long)]
        json: bool,
        /// Don't print a summary.
        #[arg(short, long)]
        quiet: bool,
    },
    /// Print a summary of a snapshot (default: latest).
    Report {
        snapshot: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Rows per section.
        #[arg(long, default_value_t = 15)]
        top: usize,
    },
    /// Browse a snapshot in the terminal (default: latest; scans if none).
    #[cfg(feature = "tui")]
    Tui { snapshot: Option<PathBuf> },
    /// Serve the interactive web UI on localhost.
    #[cfg(feature = "web")]
    Serve {
        snapshot: Option<PathBuf>,
        #[arg(long, default_value_t = 7878)]
        port: u16,
        /// Don't open a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Compare two snapshots (default: the two most recent).
    Diff {
        old: Option<PathBuf>,
        new: Option<PathBuf>,
        /// Ignore changes smaller than this (e.g. 50M, 1G).
        #[arg(long, default_value = "100M")]
        threshold: String,
        #[arg(long)]
        json: bool,
    },
    /// Export a subtree in ncdu's JSON format (`ncdu -f file`).
    Export {
        snapshot: Option<PathBuf>,
        /// Subtree to export.
        #[arg(long, default_value = "/")]
        path: String,
        /// Don't descend into other filesystems mounted below the path.
        #[arg(short = 'x', long)]
        one_file_system: bool,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Show detected providers and how complete their view is.
    Providers {
        #[arg(long)]
        json: bool,
    },
    /// List or run reclaim actions from a snapshot.
    Clean {
        /// Entity ids from `diskeye clean --list` or the report.
        ids: Vec<u32>,
        /// Also run every `safe` item that has an action.
        #[arg(long)]
        safe: bool,
        #[arg(long)]
        snapshot: Option<PathBuf>,
        #[arg(long)]
        list: bool,
        /// Show what would happen without doing it.
        #[arg(long)]
        dry_run: bool,
        /// Don't ask for confirmation (never applies to `danger` items or to root).
        #[arg(short, long)]
        yes: bool,
    },
    /// List saved snapshots.
    Snapshots,
    /// Write the made-up demo snapshots used for the README screenshots.
    #[command(hide = true)]
    Demo {
        /// Directory for the snapshots.
        dir: PathBuf,
    },
}

#[derive(Args, Clone, Default)]
struct ScanArgs {
    /// Paths to scan (default: every local filesystem).
    paths: Vec<PathBuf>,
    /// With paths: stay on the filesystem of each path.
    #[arg(short = 'x', long)]
    one_file_system: bool,
    /// Also walk tmpfs/ramfs.
    #[arg(long)]
    tmpfs: bool,
    /// Also walk network filesystems (NFS, CIFS, sshfs...).
    #[arg(long)]
    network: bool,
    /// Glob of paths to skip (repeatable).
    #[arg(long = "exclude", value_name = "GLOB")]
    excludes: Vec<String>,
    /// Filesystem types to skip (repeatable), e.g. ntfs3.
    #[arg(long = "exclude-fs", value_name = "TYPE")]
    exclude_fs: Vec<String>,
    /// Mount points to skip (repeatable).
    #[arg(long = "exclude-mount", value_name = "PATH")]
    exclude_mounts: Vec<String>,
    /// Skip container/VM/cache providers (physical layer still runs).
    #[arg(long)]
    no_providers: bool,
    /// Only run these providers (comma separated).
    #[arg(long, value_delimiter = ',')]
    providers: Option<Vec<String>>,
    /// Don't look for files hidden under mountpoints (root only).
    #[arg(long)]
    no_hidden: bool,
    #[arg(long)]
    threads: Option<usize>,
}

impl ScanArgs {
    fn pipeline(&self, quiet: bool) -> PipelineOptions {
        let mut only = self.providers.clone();
        if let Some(o) = only.as_mut() {
            for core in ["block", "lvm"] {
                if !o.iter().any(|p| p == core) {
                    o.push(core.into());
                }
            }
        }
        PipelineOptions {
            scan: ScanOptions {
                roots: self.paths.clone(),
                one_file_system: self.one_file_system,
                selection: Selection {
                    include_memory: self.tmpfs,
                    include_network: self.network,
                    exclude_fstypes: self.exclude_fs.clone(),
                    exclude_mounts: self.exclude_mounts.clone(),
                },
                excludes: self.excludes.clone(),
                threads: self.threads,
                quiet,
            },
            no_walk: false,
            no_providers: self.no_providers,
            only_providers: only,
            no_hidden: self.no_hidden,
        }
    }
}

fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn resolve(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => snapshot::latest().context("no saved snapshots yet; run `diskeye scan` first"),
    }
}

fn load(path: Option<PathBuf>) -> Result<(Snapshot, PathBuf)> {
    let p = resolve(path)?;
    Ok((snapshot::load(&p)?, p))
}

/// Snapshots kept in the default directory when no `--keep` is given.
const DEFAULT_KEEP: usize = 10;

fn scan_and_save(
    args: &ScanArgs,
    output: Option<PathBuf>,
    save: bool,
    quiet: bool,
    keep: usize,
) -> Result<(Snapshot, Option<PathBuf>)> {
    let snap = pipeline::build(&args.pipeline(quiet))?;
    let path = if save {
        let default_location = output.is_none();
        let p = output.unwrap_or_else(|| snapshot::default_path_for_now(&snap.meta.host));
        snapshot::save(&snap, &p)?;
        if !quiet {
            eprintln!("snapshot saved to {}", p.display());
        }
        if default_location && keep > 0 {
            for old in snapshot::prune(keep) {
                if !quiet {
                    eprintln!("pruned old snapshot {}", old.display());
                }
            }
        }
        Some(p)
    } else {
        None
    };
    Ok((snap, path))
}

/// Interactive frontends use the given snapshot, else the latest one if it
/// is less than an hour old, else a fresh scan.
#[cfg(any(feature = "tui", feature = "web"))]
fn interactive_snapshot(path: Option<PathBuf>, args: &ScanArgs) -> Result<(Snapshot, Option<PathBuf>)> {
    let fresh = || {
        let p = snapshot::latest()?;
        let age = std::fs::metadata(&p).ok()?.modified().ok()?.elapsed().ok()?;
        (age < std::time::Duration::from_secs(3600)).then_some(p)
    };
    if let Some(p) = path.or_else(fresh) {
        eprintln!("using snapshot {}", p.display());
        return Ok((snapshot::load(&p)?, Some(p)));
    }
    scan_and_save(args, None, true, false, DEFAULT_KEEP)
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            #[cfg(feature = "tui")]
            if std::io::stdout().is_terminal() {
                let (snap, path) = interactive_snapshot(None, &cli.scan)?;
                return crate::tui::run(snap, path);
            }
            let (snap, _) = scan_and_save(&cli.scan, None, true, false, DEFAULT_KEEP)?;
            print!("{}", crate::report::text(&snap, color(), 15));
        }
        Some(Cmd::Scan { scan, output, no_save, keep, json, quiet }) => {
            let (snap, _) = scan_and_save(&scan, output, !no_save, quiet && !json, keep)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&crate::report::json(&snap, 25))?);
            } else if !quiet {
                print!("{}", crate::report::text(&snap, color(), 15));
            }
        }
        Some(Cmd::Report { snapshot, json, top }) => {
            let (snap, _) = load(snapshot)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&crate::report::json(&snap, top))?);
            } else {
                print!("{}", crate::report::text(&snap, color(), top));
            }
        }
        #[cfg(feature = "tui")]
        Some(Cmd::Tui { snapshot }) => {
            let (snap, path) = interactive_snapshot(snapshot, &cli.scan)?;
            crate::tui::run(snap, path)?;
        }
        #[cfg(feature = "web")]
        Some(Cmd::Serve { snapshot, port, no_open }) => {
            let (snap, path) = interactive_snapshot(snapshot, &cli.scan)?;
            let args = cli.scan.clone();
            let rescanner: crate::web::api::Rescanner = std::sync::Arc::new(move || {
                eprintln!("re-scanning (requested from the web UI)…");
                let res = scan_and_save(&args, None, true, true, DEFAULT_KEEP);
                match &res {
                    Ok((_, Some(p))) => eprintln!("re-scan done, snapshot saved to {}", p.display()),
                    Ok(_) => eprintln!("re-scan done"),
                    Err(e) => eprintln!("re-scan failed: {e:#}"),
                }
                res
            });
            crate::web::serve(snap, path, port, !no_open, Some(rescanner))?;
        }
        Some(Cmd::Diff { old, new, threshold, json }) => {
            let threshold = crate::providers::parse_size(&threshold).context("bad --threshold")?;
            let (old, new) = match (old, new) {
                (Some(o), Some(n)) => (o, n),
                (Some(o), None) => (o, resolve(None)?),
                _ => {
                    let l = snapshot::list();
                    if l.len() < 2 {
                        bail!("need two snapshots to diff ({} saved)", l.len());
                    }
                    (l[l.len() - 2].clone(), l[l.len() - 1].clone())
                }
            };
            let d = model::diff::diff(&snapshot::load(&old)?, &snapshot::load(&new)?, threshold);
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print!("{}", crate::report::diff_text(&d, color()));
            }
        }
        Some(Cmd::Export { snapshot, path, one_file_system, output }) => {
            let (snap, _) = load(snapshot)?;
            let node = snap.lookup(&path).with_context(|| format!("{path} is not in the snapshot"))?;
            match output {
                Some(p) => {
                    let mut f = std::io::BufWriter::new(std::fs::File::create(&p)?);
                    model::ncdu::export(&snap, node, !one_file_system, &mut f)?;
                }
                None => model::ncdu::export(&snap, node, !one_file_system, &mut std::io::stdout().lock())?,
            }
        }
        Some(Cmd::Providers { json }) => {
            let mut opts = cli.scan.pipeline(false);
            opts.no_walk = true;
            let snap = pipeline::build(&opts)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snap.providers)?);
            } else {
                for p in &snap.providers {
                    println!(
                        "{:<22} {:<9} {:>6}ms  {}",
                        p.name,
                        format!("{:?}", p.coverage),
                        p.duration_ms,
                        p.notes.join("; ")
                    );
                }
                let n = snap.entities.len();
                println!("\n{n} entities discovered (sizes need a full scan: `diskeye scan`)");
            }
        }
        Some(Cmd::Clean { ids, safe, snapshot, list, dry_run, yes }) => clean(ids, safe, snapshot, list, dry_run, yes)?,
        Some(Cmd::Snapshots) => {
            for p in snapshot::list() {
                let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                println!("{:>10}  {}", model::fmt_size(size), p.display());
            }
        }
        Some(Cmd::Demo { dir }) => {
            let (path, _) = crate::demo::write(&dir)?;
            println!("{}", path.display());
        }
    }
    Ok(())
}

fn clean(ids: Vec<u32>, safe: bool, snapshot: Option<PathBuf>, list: bool, dry_run: bool, yes: bool) -> Result<()> {
    let (snap, _) = load(snapshot)?;
    let items = crate::views::reclaim(&snap);
    if list || (ids.is_empty() && !safe) {
        for it in &items {
            let e = &snap.entities[it.entity as usize];
            println!(
                "{:>4}  {:<7} {:>10}  {} · {}  — {}",
                e.id,
                it.risk.label(),
                model::fmt_size(it.bytes),
                e.group,
                e.name,
                it.reason
            );
        }
        if items.is_empty() {
            println!("nothing reclaimable found in this snapshot");
        }
        return Ok(());
    }
    let mut chosen = Vec::new();
    for id in ids {
        let it = items.iter().find(|i| i.entity == id).with_context(|| format!("no reclaimable item with id {id}"))?;
        chosen.push(it);
    }
    if safe {
        chosen.extend(items.iter().filter(|i| i.risk == model::Risk::Safe && i.action.is_some()));
    }
    chosen.sort_by_key(|i| i.entity);
    chosen.dedup_by_key(|i| i.entity);

    // Show everything first; drop items that can't run.
    let mut runnable = Vec::new();
    for it in chosen {
        let e = &snap.entities[it.entity as usize];
        println!("{} · {}  ({}, ~{})", e.group, e.name, it.risk.label(), model::fmt_size(it.bytes));
        println!("  {}", it.reason);
        let Some(action) = &it.action else {
            println!("  skipped: no automatic action");
            continue;
        };
        for line in crate::actions::describe(action) {
            println!("  • {line}");
        }
        match crate::actions::preflight(action) {
            Ok(()) => runnable.push((it, action)),
            Err(err) => println!("  skipped: cannot run: {err:#}"),
        }
    }
    if runnable.is_empty() {
        bail!("nothing to run");
    }
    let total: u64 = runnable.iter().map(|(it, _)| it.bytes).sum();
    if dry_run {
        println!("(dry run — would run {} item(s), ~{}; nothing changed)", runnable.len(), model::fmt_size(total));
        return Ok(());
    }
    let strict = crate::util::is_root() || runnable.iter().any(|(it, _)| it.risk == model::Risk::Danger);
    if strict || !yes {
        print!(
            "Run {} item(s), freeing ~{}? Type `{}` to confirm: ",
            runnable.len(),
            model::fmt_size(total),
            crate::actions::confirm_word(strict)
        );
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !crate::actions::confirmed(&line, strict) {
            println!("aborted; nothing changed");
            return Ok(());
        }
    }
    let mut failed = Vec::new();
    for (it, action) in &runnable {
        let name = &snap.entities[it.entity as usize].name;
        match crate::actions::execute(action, it.risk, it.bytes) {
            Ok(()) => println!("done: {name}"),
            Err(err) => {
                println!("failed: {name}: {err:#}");
                failed.push(name.as_str());
            }
        }
    }
    if !failed.is_empty() {
        bail!("{} of {} item(s) failed: {}", failed.len(), runnable.len(), failed.join(", "));
    }
    println!("done. Re-scan to see the new totals.");
    Ok(())
}
