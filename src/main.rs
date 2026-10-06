mod actions;
mod cli;
mod demo;
mod model;
mod pipeline;
mod providers;
mod report;
mod scan;
#[cfg(feature = "tui")]
mod tui;
mod util;
mod views;
#[cfg(feature = "web")]
mod web;

fn main() {
    // The hidden-under-mounts helper must run before any threads start.
    if std::env::args().nth(1).as_deref() == Some(scan::hidden::SUBCOMMAND) {
        if let Err(e) = scan::hidden::helper_main() {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = cli::main() {
        eprintln!("diskeye: {e:#}");
        std::process::exit(1);
    }
}
