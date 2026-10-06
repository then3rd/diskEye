//! The Diff tab: compare this snapshot with another saved one. Loading and
//! diffing run on a background thread the first time the tab is shown.

use super::app::App;
use super::lineview::{LineView, Row, Target};
use super::theme;
use crate::model::diff::DiffReport;
use crate::model::{Snapshot, fmt_signed, fmt_size};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

pub const THRESHOLD: u64 = 100 << 20;

#[derive(Default)]
pub struct DiffState {
    /// Other saved snapshots, oldest first. `None` until the tab is first shown.
    pub candidates: Option<Vec<PathBuf>>,
    pub idx: usize,
    pub results: HashMap<usize, Result<Rc<DiffReport>, String>>,
    pub loading: Option<(usize, Receiver<Result<DiffReport, String>>)>,
    pub view: LineView,
}

/// Baseline candidates and the default pick: the snapshot just before `current`
/// when it's in the list, otherwise the latest other one.
pub fn pick_candidates(all: Vec<PathBuf>, current: Option<&Path>) -> (Vec<PathBuf>, usize) {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let cur = current.map(canon);
    let pos = cur.as_ref().and_then(|c| all.iter().position(|p| &canon(p) == c));
    let others: Vec<PathBuf> =
        all.iter().enumerate().filter(|(i, _)| Some(*i) != pos).map(|(_, p)| p.clone()).collect();
    let idx = match pos {
        Some(p) if p > 0 => p - 1,
        _ => others.len().saturating_sub(1),
    };
    (others, idx)
}

impl DiffState {
    /// Advance background work; returns true when something changed.
    pub fn tick(&mut self, snap: &Arc<Snapshot>, current: Option<&Path>) -> bool {
        let mut changed = false;
        if self.candidates.is_none() {
            let (c, i) = pick_candidates(crate::model::snapshot::list(), current);
            self.candidates = Some(c);
            self.idx = i;
            changed = true;
        }
        if let Some((i, rx)) = &self.loading {
            match rx.try_recv() {
                Ok(r) => {
                    self.results.insert(*i, r.map(Rc::new));
                    self.loading = None;
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.results.insert(*i, Err("diff worker crashed".into()));
                    self.loading = None;
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let cands = self.candidates.as_ref().unwrap();
        if self.loading.is_none() && !cands.is_empty() && !self.results.contains_key(&self.idx) {
            let path = cands[self.idx].clone();
            let snap = snap.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let r = crate::model::snapshot::load(&path)
                    .map(|old| crate::model::diff::diff(&old, &snap, THRESHOLD))
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(r);
            });
            self.loading = Some((self.idx, rx));
            changed = true;
        }
        changed
    }

    pub fn busy(&self) -> bool {
        self.loading.is_some()
    }

    pub fn older(&mut self) {
        if self.idx > 0 {
            self.idx -= 1;
            self.view = LineView::default();
        }
    }

    pub fn newer(&mut self) {
        if let Some(c) = &self.candidates
            && self.idx + 1 < c.len()
        {
            self.idx += 1;
            self.view = LineView::default();
        }
    }
}

fn delta_style(d: i64) -> ratatui::style::Style {
    if d > 0 { theme::bad() } else { theme::good() }
}

pub fn rows(app: &App, _width: u16) -> Vec<Row> {
    let st = &app.diff;
    let snap = &app.snap;
    let mut out = Vec::new();
    let Some(cands) = &st.candidates else {
        out.push(Row::text(Line::styled(" loading…", theme::dim())));
        return out;
    };
    if cands.is_empty() {
        out.push(Row::text(Line::styled(" No other snapshot to compare with.", theme::bold())));
        out.push(Row::text(Line::styled(
            format!(
                " Snapshots are saved in {} on every `diskeye scan`; scan again later to see what changed.",
                crate::model::snapshot::default_dir().display()
            ),
            theme::dim(),
        )));
        return out;
    }
    let base = &cands[st.idx];
    let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    out.push(Row::text(Line::from(vec![
        Span::styled(" baseline ", theme::dim()),
        Span::styled(name, theme::bold()),
        Span::styled(format!("   ({} of {})   [ older · ] newer", st.idx + 1, cands.len()), theme::dim()),
    ])));
    match st.results.get(&st.idx) {
        None => {
            out.push(Row::blank());
            out.push(Row::text(Line::styled(
                format!(" loading {} and comparing… (large snapshots take a few seconds)", base.display()),
                theme::warn(),
            )));
        }
        Some(Err(e)) => {
            out.push(Row::blank());
            out.push(Row::text(Line::styled(format!(" could not compare: {e}"), theme::bad())));
        }
        Some(Ok(d)) => {
            out.push(Row::text(Line::styled(
                format!(
                    " {} → {}   (changes ≥ {})",
                    crate::util::timestamp_human(d.old_time),
                    crate::util::timestamp_human(d.new_time),
                    fmt_size(THRESHOLD)
                ),
                theme::dim(),
            )));
            if d.old_time > d.new_time {
                out.push(Row::text(Line::styled(
                    " note: the baseline is newer than this snapshot, so growth shows as negative",
                    theme::warn(),
                )));
            }
            out.push(Row::blank());
            out.push(Row::text(Line::styled(" FILESYSTEMS", theme::heading())));
            for f in d.filesystems.iter().filter(|f| f.old_used > 0 || f.new_used > 0) {
                let dl = f.new_used as i64 - f.old_used as i64;
                let target = snap
                    .filesystems
                    .iter()
                    .find(|x| x.mount_point == f.mount_point)
                    .and_then(|x| x.root_node)
                    .map(Target::Node);
                out.push(Row::item(
                    Line::from(vec![
                        Span::raw(format!(
                            "   {} {:>10} → {:>10}  ",
                            theme::pad(&f.mount_point, 28),
                            fmt_size(f.old_used),
                            fmt_size(f.new_used)
                        )),
                        Span::styled(format!("{:>11}", fmt_signed(dl)), delta_style(dl)),
                    ]),
                    target,
                ));
            }
            out.push(Row::blank());
            out.push(Row::text(Line::styled(" WHERE IT CHANGED  (Enter opens the path in Files)", theme::heading())));
            if d.hotspots.is_empty() {
                out.push(Row::text(Line::styled("   no directory changed by more than the threshold", theme::dim())));
            }
            for h in &d.hotspots {
                let target = snap.lookup(&h.path).map(Target::Node);
                let mut spans = vec![
                    Span::styled(format!("   {:>11}", fmt_signed(h.delta())), delta_style(h.delta())),
                    Span::raw(format!("  {}", h.path)),
                    Span::styled(format!("  {} → {}", fmt_size(h.old), fmt_size(h.new)), theme::dim()),
                ];
                if let Some(o) = &h.owner {
                    spans.push(Span::styled(format!("  [{o}]"), theme::owner()));
                }
                if target.is_none() {
                    spans.push(Span::styled("  (gone)", theme::dim()));
                }
                out.push(Row::item(Line::from(spans), target));
            }
            if !d.entities.is_empty() {
                out.push(Row::blank());
                out.push(Row::text(Line::styled(" WORKLOADS", theme::heading())));
                for e in &d.entities {
                    let dl = e.new as i64 - e.old as i64;
                    let target = snap
                        .entities
                        .iter()
                        .find(|x| x.group == e.group && x.kind == e.kind && x.name == e.name)
                        .map(|x| Target::Entity(x.id));
                    out.push(Row::item(
                        Line::from(vec![
                            Span::styled(format!("   {:>11}", fmt_signed(dl)), delta_style(dl)),
                            Span::raw(format!("  {} · {}", e.group, e.name)),
                            Span::styled(format!("  {} → {}", fmt_size(e.old), fmt_size(e.new)), theme::dim()),
                        ]),
                        target,
                    ));
                }
            }
        }
    }
    out
}
