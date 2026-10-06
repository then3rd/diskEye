//! Interactive terminal UI: Physical, Files (with treemap), Workloads,
//! Reclaim, Reconcile and Diff views over one snapshot.

mod app;
mod diffview;
mod files;
mod lineview;
mod physical;
mod reclaim;
mod reconcile;
mod theme;
mod treemap;
mod ui;
mod workloads;

#[cfg(test)]
mod tests;

use crate::model::{Snapshot, fmt_size};
use app::{App, Level};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event};
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};
use std::io::{Stdout, Write, stdout};
use std::path::PathBuf;
use std::time::Duration;

type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn run(snap: Snapshot, path: Option<PathBuf>) -> anyhow::Result<()> {
    let mut app = App::new(snap, path);
    install_panic_hook();
    let mut terminal = enter()?;
    let res = event_loop(&mut terminal, &mut app);
    leave();
    res
}

fn enter() -> std::io::Result<Term> {
    enable_raw_mode()?;
    if let Err(e) = execute!(stdout(), EnterAlternateScreen) {
        leave();
        return Err(e);
    }
    Terminal::new(CrosstermBackend::new(stdout()))
}

fn leave() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, cursor::Show);
}

/// Restore the terminal before the default panic message is printed.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        prev(info);
    }));
}

fn event_loop(terminal: &mut Term, app: &mut App) -> anyhow::Result<()> {
    let mut dirty = true;
    while !app.quit {
        if app.tick() {
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| ui::draw(app, f))?;
            dirty = false;
        }
        let timeout = if app.diff.busy() { Duration::from_millis(100) } else { Duration::from_millis(500) };
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(k) => {
                    app.on_key(k);
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
        if let Some(items) = app.pending_exec.take() {
            run_actions(terminal, app, items)?;
            dirty = true;
        }
    }
    Ok(())
}

/// Leave the TUI, run the confirmed actions with their output visible, wait
/// for Enter, then come back.
fn run_actions(terminal: &mut Term, app: &mut App, items: Vec<usize>) -> anyhow::Result<()> {
    leave();
    let mut ok = 0;
    let mut freed = 0;
    let mut failed = Vec::new();
    println!();
    for &i in &items {
        let it = app.reclaim[i].clone();
        let e = &app.snap.entities[it.entity as usize];
        println!("\x1b[1m▶ {} · {}\x1b[0m  ({}, ~{})", e.group, e.name, it.risk.label(), fmt_size(it.bytes));
        let Some(action) = &it.action else { continue };
        for l in crate::actions::describe(action) {
            println!("  • {l}");
        }
        let _ = std::io::stdout().flush();
        match crate::actions::execute(action, it.risk, it.bytes) {
            Ok(()) => {
                println!("  \x1b[32mdone\x1b[0m");
                ok += 1;
                freed += it.bytes;
                app.mark_done(i);
            }
            Err(err) => {
                println!("  \x1b[31mfailed: {err:#}\x1b[0m");
                failed.push(e.name.clone());
            }
        }
    }
    println!(
        "\n{ok} of {} action(s) succeeded (~{} freed). Sizes in this view come from the snapshot and stay stale until you re-scan (`diskeye scan`).",
        items.len(),
        fmt_size(freed)
    );
    print!("Press Enter to return to diskeye… ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    *terminal = enter()?;
    terminal.clear()?;
    if failed.is_empty() {
        app.set_status(
            format!("{ok} action(s) done, ~{} freed — re-scan to update sizes", fmt_size(freed)),
            Level::Info,
        );
    } else {
        app.set_status(format!("{} failed: {}", failed.len(), failed.join(", ")), Level::Error);
    }
    Ok(())
}
