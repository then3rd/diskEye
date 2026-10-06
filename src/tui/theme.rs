//! Styles and small formatting helpers. Only the terminal's named palette
//! colours plus bold/dim/reverse are used (legible on dark and light themes);
//! RGB is reserved for the treemap's block backgrounds.

use crate::model::Risk;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

pub fn heading() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

pub fn good() -> Style {
    Style::new().fg(Color::Green)
}

pub fn warn() -> Style {
    Style::new().fg(Color::Yellow)
}

pub fn bad() -> Style {
    Style::new().fg(Color::Red)
}

pub fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

pub fn owner() -> Style {
    Style::new().fg(Color::Magenta)
}

pub fn risk(r: Risk) -> Style {
    match r {
        Risk::Safe => good(),
        Risk::Review => warn(),
        Risk::Danger => bad().add_modifier(Modifier::BOLD),
    }
}

/// Colour for a fill fraction (filesystem usage).
pub fn usage(frac: f64) -> Style {
    if frac >= 0.9 {
        bad()
    } else if frac >= 0.8 {
        warn()
    } else {
        accent()
    }
}

/// A proportional bar as two spans (filled, empty) of exactly `width` cells.
pub fn bar(frac: f64, width: usize, style: Style) -> Vec<Span<'static>> {
    let frac = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
    let eighths = (frac * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;
    let mut filled = "█".repeat(full.min(width));
    let mut used = full.min(width);
    if rem > 0 && used < width {
        filled.push(['▏', '▎', '▍', '▌', '▋', '▊', '▉'][rem - 1]);
        used += 1;
    }
    vec![Span::styled(filled, style), Span::styled("·".repeat(width - used), dim())]
}

/// Truncate to `n` display columns with an ellipsis (counts chars; good enough
/// for labels, ratatui clips wide glyphs safely).
pub fn trunc(s: &str, n: usize) -> String {
    crate::report::truncate(s, n)
}

/// Keep the end of a path visible: `…/some/deep/dir`.
pub fn trunc_left(s: &str, n: usize) -> String {
    let c = s.chars().count();
    if c <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let tail: String = s.chars().skip(c - (n - 1)).collect();
    format!("…{tail}")
}

pub fn pad(s: &str, n: usize) -> String {
    let t = trunc(s, n);
    let c = t.chars().count();
    format!("{t}{}", " ".repeat(n.saturating_sub(c)))
}

pub fn age(mtime: i64) -> String {
    if mtime <= 0 { "-".into() } else { crate::util::age_human(mtime) }
}
