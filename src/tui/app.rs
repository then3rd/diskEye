//! Application state and the key-event state machine (no terminal I/O here,
//! so it can be driven directly from tests).

use super::diffview::DiffState;
use super::files::{FilesState, Sort};
use super::lineview::{LineView, Row, Target};
use super::reclaim::ReclaimState;
use super::workloads::{self, WRow, WorkState};
use super::{diffview, physical, reconcile, treemap};
use crate::model::{Coverage, NodeId, Reconcile, Risk, Snapshot};
use crate::views::{self, Metric, PhysRow, ReclaimItem, WorkloadGroup};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Physical,
    Files,
    Workloads,
    Reclaim,
    Reconcile,
    Diff,
}

impl Tab {
    pub const ALL: [Tab; 6] = [Tab::Physical, Tab::Files, Tab::Workloads, Tab::Reclaim, Tab::Reconcile, Tab::Diff];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Physical => "Physical",
            Tab::Files => "Files",
            Tab::Workloads => "Workloads",
            Tab::Reclaim => "Reclaim",
            Tab::Reconcile => "Reconcile",
            Tab::Diff => "Diff",
        }
    }
    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|&t| t == self).unwrap()
    }
}

pub enum Popup {
    Help,
    Preview {
        items: Vec<usize>,
        checks: Vec<Result<(), String>>,
        scroll: u16,
    },
    /// `strict`: needs [`crate::actions::STRICT_WORD`] rather than `yes`.
    Confirm {
        items: Vec<usize>,
        strict: bool,
        input: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

pub struct App {
    pub snap: Arc<Snapshot>,
    pub path: Option<PathBuf>,
    pub targets: HashMap<String, NodeId>,
    pub by_node: HashMap<NodeId, Vec<u32>>,
    pub phys_rows: Vec<PhysRow>,
    pub hotspots: Vec<NodeId>,
    pub groups: Vec<WorkloadGroup>,
    /// Child entities per entity, largest first.
    pub kids: HashMap<u32, Vec<u32>>,
    pub reclaim: Vec<ReclaimItem>,
    /// Entities whose action ran successfully this session.
    pub done: HashSet<u32>,
    pub reconcile: Vec<Reconcile>,
    /// (used, free-for-users, total) over scanned filesystems.
    pub totals: (u64, u64, u64),
    pub tab: Tab,
    pub popup: Option<Popup>,
    pub status: Option<(String, Level)>,
    pub files: FilesState,
    pub phys: LineView,
    pub work: WorkState,
    pub recl: ReclaimState,
    pub reconc: LineView,
    pub diff: DiffState,
    pub quit: bool,
    /// Reclaim indices confirmed for execution; the run loop picks this up.
    pub pending_exec: Option<Vec<usize>>,
    /// Whether *this process* runs as root (stricter confirmations).
    pub run_as_root: bool,
    /// Width of the last drawn body, for row builders used by key handling.
    pub width: u16,
}

impl App {
    pub fn new(snap: Snapshot, path: Option<PathBuf>) -> Self {
        let snap = Arc::new(snap);
        let targets = views::mount_targets(&snap);
        let by_node = crate::model::attribution::claims_by_node(&snap);
        let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
        for e in &snap.entities {
            if let Some(p) = e.parent {
                kids.entry(p).or_default().push(e.id);
            }
        }
        for v in kids.values_mut() {
            v.sort_by_key(|&i| std::cmp::Reverse(snap.entities[i as usize].total_bytes()));
        }
        let mut seen = HashSet::new();
        let mut totals = (0u64, 0u64, 0u64);
        for f in snap.filesystems.iter().filter(|f| f.root_node.is_some()) {
            if let Some(sv) = f.statvfs
                && seen.insert((f.dev_major, f.dev_minor))
            {
                totals.0 += sv.used();
                totals.1 += sv.avail;
                totals.2 += sv.total;
            }
        }
        let files = FilesState::new(&snap);
        App {
            phys_rows: views::physical(&snap),
            hotspots: views::hotspots(&snap, 25),
            groups: views::workloads(&snap),
            reclaim: views::reclaim(&snap),
            reconcile: views::reconcile(&snap),
            kids,
            targets,
            by_node,
            done: HashSet::new(),
            totals,
            tab: Tab::Physical,
            popup: None,
            status: None,
            files,
            phys: LineView::default(),
            work: WorkState::default(),
            recl: ReclaimState::default(),
            reconc: LineView::default(),
            diff: DiffState::default(),
            quit: false,
            pending_exec: None,
            run_as_root: crate::util::is_root(),
            width: 100,
            snap,
            path,
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>, level: Level) {
        self.status = Some((msg.into(), level));
    }

    /// Providers with partial/denied coverage, and unreadable directories.
    pub fn coverage_gaps(&self) -> (usize, u64) {
        let p =
            self.snap.providers.iter().filter(|p| matches!(p.coverage, Coverage::Partial | Coverage::Denied)).count();
        let d = self.snap.filesystems.iter().map(|f| f.denied_dirs).sum();
        (p, d)
    }

    pub fn rows_for(&self, tab: Tab) -> Vec<Row> {
        match tab {
            Tab::Physical => physical::rows(self, self.width),
            Tab::Reconcile => reconcile::rows(self, self.width),
            Tab::Diff => diffview::rows(self, self.width),
            _ => Vec::new(),
        }
    }

    /// Background work (diff loading). Returns true if a redraw is needed.
    pub fn tick(&mut self) -> bool {
        if self.tab == Tab::Diff || self.diff.busy() {
            let path = self.path.clone();
            return self.diff.tick(&self.snap, path.as_deref());
        }
        false
    }

    // ------------------------------------------------------------ jumps

    pub fn goto(&mut self, t: Target) {
        match t {
            Target::Node(n) => {
                let open = self.snap.tree.node(n).is_dir();
                let (snap, targets) = (&self.snap, &self.targets);
                self.files.jump(snap, targets, n, open);
                self.tab = Tab::Files;
            }
            Target::Entity(id) => {
                workloads::reveal(self, id);
                self.tab = Tab::Workloads;
            }
        }
    }

    fn reclaim_index(&self, entity: u32) -> Option<usize> {
        self.reclaim.iter().position(|r| r.entity == entity)
    }

    /// Entities owning `node` (nearest claim) and their ancestors, most specific first.
    fn owner_chain(&self, node: NodeId) -> Vec<u32> {
        let mut v = Vec::new();
        for e in crate::model::attribution::owners(&self.snap, &self.by_node, node) {
            let mut cur = Some(e);
            while let Some(c) = cur {
                if !v.contains(&c) {
                    v.push(c);
                }
                cur = self.snap.entities.get(c as usize).and_then(|x| x.parent);
            }
        }
        v
    }

    pub fn open_preview(&mut self, items: Vec<usize>) {
        let checks = super::reclaim::preflight_all(self, &items);
        self.popup = Some(Popup::Preview { items, checks, scroll: 0 });
    }

    /// Ask for confirmation to run `items` (indices into `reclaim`).
    pub fn request_exec(&mut self, items: Vec<usize>) {
        let items: Vec<usize> = items.into_iter().filter(|&i| !self.done.contains(&self.reclaim[i].entity)).collect();
        if items.is_empty() {
            self.set_status("already run this session — re-scan to refresh", Level::Warn);
            return;
        }
        for &i in &items {
            let it = &self.reclaim[i];
            let name = self.snap.entities[it.entity as usize].name.clone();
            let Some(a) = &it.action else {
                self.set_status(format!("{name}: no automatic action ({})", it.reason), Level::Warn);
                return;
            };
            if let Err(e) = crate::actions::preflight(a) {
                self.set_status(format!("{name}: cannot run: {e:#}"), Level::Error);
                return;
            }
        }
        let strict = self.run_as_root || items.iter().any(|&i| self.reclaim[i].risk == Risk::Danger);
        self.popup = Some(Popup::Confirm { items, strict, input: String::new() });
    }

    /// Record the outcome of running reclaim item `i`.
    pub fn mark_done(&mut self, i: usize) {
        let e = self.reclaim[i].entity;
        self.done.insert(e);
        self.recl.marked.remove(&i);
    }

    // ------------------------------------------------------------ keys

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        self.status = None;
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        if self.popup.is_some() {
            self.on_popup_key(k);
            return;
        }
        if self.tab == Tab::Files && self.files.filter_editing {
            self.on_filter_key(k);
            return;
        }
        match k.code {
            KeyCode::Char('q') => {
                self.quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.popup = Some(Popup::Help);
                return;
            }
            KeyCode::Char(c @ '1'..='6') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
                return;
            }
            KeyCode::Tab => {
                self.tab = Tab::ALL[(self.tab.index() + 1) % 6];
                return;
            }
            KeyCode::BackTab => {
                self.tab = Tab::ALL[(self.tab.index() + 5) % 6];
                return;
            }
            _ => {}
        }
        let handled = match self.tab {
            Tab::Files => self.on_files_key(k),
            Tab::Workloads => self.on_work_key(k),
            Tab::Reclaim => self.on_reclaim_key(k),
            Tab::Physical | Tab::Reconcile | Tab::Diff => self.on_lines_key(k),
        };
        if !handled && k.code == KeyCode::Esc {
            self.quit = true;
        }
    }

    fn on_popup_key(&mut self, k: KeyEvent) {
        let Some(p) = self.popup.as_mut() else { return };
        match p {
            Popup::Help => self.popup = None,
            Popup::Preview { items, scroll, .. } => match k.code {
                KeyCode::Char('x') => {
                    let items = items.clone();
                    self.popup = None;
                    self.request_exec(items);
                }
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                _ => self.popup = None,
            },
            Popup::Confirm { items, strict, input } => match k.code {
                KeyCode::Esc => {
                    self.popup = None;
                    self.set_status("cancelled — nothing changed", Level::Info);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Enter => {
                    if crate::actions::confirmed(input, *strict) {
                        self.pending_exec = Some(items.clone());
                        self.popup = None;
                    } else {
                        self.popup = None;
                        self.set_status("confirmation did not match — nothing changed", Level::Warn);
                    }
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            },
        }
    }

    fn on_filter_key(&mut self, k: KeyEvent) {
        let (snap, targets) = (&self.snap, &self.targets);
        let f = &mut self.files;
        match k.code {
            KeyCode::Esc => {
                f.filter.clear();
                f.filter_editing = false;
                f.sel = 0;
                f.offset = 0;
            }
            KeyCode::Enter => f.filter_editing = false,
            KeyCode::Backspace => {
                f.filter.pop();
                f.sel = 0;
                f.offset = 0;
            }
            KeyCode::Up => f.move_by(snap, targets, -1),
            KeyCode::Down => f.move_by(snap, targets, 1),
            KeyCode::Char(c) => {
                f.filter.push(c);
                f.sel = 0;
                f.offset = 0;
            }
            _ => {}
        }
    }

    fn on_files_key(&mut self, k: KeyEvent) -> bool {
        let (snap, targets) = (&self.snap, &self.targets);
        let f = &mut self.files;
        let page = f.height.max(2) as isize - 1;
        if f.treemap {
            let dir = match k.code {
                KeyCode::Left | KeyCode::Char('h') => Some(treemap::Dir::Left),
                KeyCode::Right | KeyCode::Char('l') => Some(treemap::Dir::Right),
                KeyCode::Up | KeyCode::Char('k') => Some(treemap::Dir::Up),
                KeyCode::Down | KeyCode::Char('j') => Some(treemap::Dir::Down),
                _ => None,
            };
            if let Some(d) = dir {
                self.treemap_move(d);
                return true;
            }
        }
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => f.move_by(snap, targets, -1),
            KeyCode::Down | KeyCode::Char('j') => f.move_by(snap, targets, 1),
            KeyCode::PageUp => f.move_by(snap, targets, -page),
            KeyCode::PageDown => f.move_by(snap, targets, page),
            KeyCode::Home => f.move_by(snap, targets, isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => f.move_by(snap, targets, isize::MAX / 2),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(e) = f.selected(snap, targets)
                    && let Err(msg) = f.descend(snap, targets, e)
                {
                    self.set_status(msg, Level::Warn);
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace | KeyCode::Char('u') => {
                if !f.up(snap, targets) {
                    self.set_status("already at the top", Level::Info);
                }
            }
            KeyCode::Char('s') => {
                let cur = f.selected(snap, targets);
                f.metric = match f.metric {
                    Metric::Alloc => Metric::Apparent,
                    Metric::Apparent => Metric::Items,
                    Metric::Items => Metric::Alloc,
                };
                f.sort = Sort::Size;
                if let Some(c) = cur {
                    f.select_node(snap, targets, c);
                }
                let m = f.metric.label();
                self.set_status(format!("showing {m}"), Level::Info);
            }
            KeyCode::Char('n') => {
                let cur = f.selected(snap, targets);
                f.sort = if f.sort == Sort::Name { Sort::Size } else { Sort::Name };
                if let Some(c) = cur {
                    f.select_node(snap, targets, c);
                }
            }
            KeyCode::Char('/') => {
                f.filter_editing = true;
            }
            KeyCode::Char('t') => f.treemap = !f.treemap,
            KeyCode::Char('g') => {
                if let Some(e) = f.selected(snap, targets) {
                    let chain = self.owner_chain(e);
                    match chain.first() {
                        Some(&id) => self.goto(Target::Entity(id)),
                        None => self.set_status("not owned by any detected workload", Level::Info),
                    }
                }
            }
            KeyCode::Char('d') => {
                if let Some(e) = f.selected(snap, targets) {
                    let chain = self.owner_chain(e);
                    match chain.iter().find_map(|&id| self.reclaim_index(id)) {
                        Some(i) => self.open_preview(vec![i]),
                        None if chain.is_empty() => {
                            self.set_status("no cleanup action: not owned by any detected workload", Level::Info)
                        }
                        None => self.set_status("the owning workload has no cleanup action", Level::Info),
                    }
                }
            }
            KeyCode::Esc => {
                if !f.filter.is_empty() {
                    f.filter.clear();
                    f.sel = 0;
                    f.offset = 0;
                } else if f.treemap {
                    f.treemap = false;
                } else {
                    return false;
                }
            }
            _ => return false,
        }
        true
    }

    fn treemap_move(&mut self, d: treemap::Dir) {
        let c = super::files::Ctx { snap: &self.snap, targets: &self.targets, by_node: &self.by_node };
        let f = &mut self.files;
        let area = f.tm_area;
        let items = super::files::treemap_items(f, &c, area);
        let rects = treemap::layout(&items.iter().map(|x| x.1).collect::<Vec<_>>(), area);
        let entries = f.entries(c.snap, c.targets);
        let cur = entries.get(f.sel).copied();
        let from =
            items.iter().position(|x| x.0.is_some() && x.0 == cur).or_else(|| items.iter().position(|x| x.0.is_none()));
        let Some(from) = from else { return };
        if let Some(to) = treemap::neighbor(&rects, from, d) {
            let node = match items[to].0 {
                Some(n) => Some(n),
                // "Others" block: select the largest entry not drawn individually.
                None => entries.iter().copied().find(|e| !items.iter().any(|x| x.0 == Some(*e))),
            };
            if let Some(n) = node {
                f.select_node(c.snap, c.targets, n);
            }
        }
    }

    fn on_work_key(&mut self, k: KeyEvent) -> bool {
        if self.groups.is_empty() {
            return false;
        }
        if self.work.focus_paths {
            let rows = workloads::detail_rows(self);
            let st = &mut self.work.detail;
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => st.step(&rows, -1),
                KeyCode::Down | KeyCode::Char('j') => st.step(&rows, 1),
                KeyCode::Enter | KeyCode::Char('g') => match st.current(&rows).map(|r| r.target) {
                    Some(Some(t)) => self.goto(t),
                    Some(None) => self.set_status("this path is not in the scanned tree", Level::Warn),
                    None => {}
                },
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.work.focus_paths = false,
                _ => return false,
            }
            return true;
        }
        let rows = workloads::rows(self);
        let n = rows.len();
        let page = self.work.height.max(2) as isize - 1;
        let mv = |st: &mut WorkState, d: isize| {
            st.sel = (st.sel as isize + d).clamp(0, n.saturating_sub(1) as isize) as usize;
        };
        let cur = rows.get(self.work.sel).copied();
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => mv(&mut self.work, -1),
            KeyCode::Down | KeyCode::Char('j') => mv(&mut self.work, 1),
            KeyCode::PageUp => mv(&mut self.work, -page),
            KeyCode::PageDown => mv(&mut self.work, page),
            KeyCode::Home => self.work.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.work.sel = n.saturating_sub(1),
            KeyCode::Char(' ') => self.toggle(cur),
            KeyCode::Enter => match cur {
                Some(WRow::Entity { id, .. }) => {
                    let e = &self.snap.entities[id as usize];
                    if e.paths.iter().any(|p| !e.unresolved_paths.contains(p)) {
                        self.work.focus_paths = true;
                        self.work.detail = LineView::default();
                    } else if workloads::has_children(self, id) {
                        self.toggle(cur);
                    } else {
                        self.set_status("no scanned paths for this entity", Level::Info);
                    }
                }
                _ => self.toggle(cur),
            },
            KeyCode::Right | KeyCode::Char('l') => match cur {
                Some(WRow::Group(g)) => {
                    self.work.collapsed.remove(&g);
                }
                Some(WRow::Entity { id, .. }) if workloads::has_children(self, id) => {
                    self.work.expanded.insert(id);
                }
                _ => {}
            },
            KeyCode::Left | KeyCode::Char('h') => match cur {
                Some(WRow::Group(g)) => {
                    self.work.collapsed.insert(g);
                }
                Some(WRow::Entity { id, depth }) => {
                    if self.work.expanded.remove(&id) {
                    } else if let Some(p) = (0..self.work.sel).rev().find(|&i| match rows[i] {
                        WRow::Group(_) => depth == 1,
                        WRow::Entity { depth: d, .. } => d + 1 == depth,
                    }) {
                        self.work.sel = p;
                    }
                }
                None => {}
            },
            KeyCode::Char('g') => {
                if let Some(WRow::Entity { id, .. }) = cur {
                    let e = &self.snap.entities[id as usize];
                    let t = e.paths.iter().find_map(|p| self.snap.lookup_static(p));
                    match t {
                        Some(n) => self.goto(Target::Node(n)),
                        None => self.set_status("no scanned paths for this entity", Level::Info),
                    }
                }
            }
            KeyCode::Char('d') => {
                if let Some(WRow::Entity { id, .. }) = cur {
                    match self.reclaim_index(id) {
                        Some(i) => self.open_preview(vec![i]),
                        None => self.set_status("no cleanup action for this entity", Level::Info),
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn toggle(&mut self, row: Option<WRow>) {
        match row {
            Some(WRow::Group(g)) => {
                if !self.work.collapsed.remove(&g) {
                    self.work.collapsed.insert(g);
                }
            }
            Some(WRow::Entity { id, .. }) if workloads::has_children(self, id) => {
                // Toggle: remove if present, otherwise insert.
                let set = &mut self.work.expanded;
                let _ = set.remove(&id) || set.insert(id);
            }
            _ => {}
        }
    }

    /// The marked reclaim items, or the selected one when nothing is marked.
    fn recl_targets(&self) -> Vec<usize> {
        if self.recl.marked.is_empty() {
            vec![self.recl.sel]
        } else {
            let mut v: Vec<usize> = self.recl.marked.iter().copied().collect();
            v.sort_unstable();
            v
        }
    }

    fn on_reclaim_key(&mut self, k: KeyEvent) -> bool {
        let n = self.reclaim.len();
        if n == 0 {
            return false;
        }
        let page = self.recl.height.max(2) as isize - 1;
        let st = &mut self.recl;
        let mut mv = |d: isize| st.sel = (st.sel as isize + d).clamp(0, n as isize - 1) as usize;
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => mv(-1),
            KeyCode::Down | KeyCode::Char('j') => mv(1),
            KeyCode::PageUp => mv(-page),
            KeyCode::PageDown => mv(page),
            KeyCode::Home => mv(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => mv(isize::MAX / 2),
            KeyCode::Char(' ') => {
                let i = self.recl.sel;
                if !self.done.contains(&self.reclaim[i].entity) && !self.recl.marked.remove(&i) {
                    self.recl.marked.insert(i);
                }
                self.recl.sel = (i + 1).min(n - 1);
            }
            KeyCode::Char(c @ ('a' | 'A')) => {
                // `a` toggles every safe item, `A` every item that has an action.
                let safe: Vec<usize> = (0..n)
                    .filter(|&i| {
                        let it = &self.reclaim[i];
                        (if c == 'a' { it.risk == Risk::Safe } else { it.action.is_some() })
                            && !self.done.contains(&it.entity)
                    })
                    .collect();
                if safe.iter().all(|i| self.recl.marked.contains(i)) {
                    for i in safe {
                        self.recl.marked.remove(&i);
                    }
                } else {
                    self.recl.marked.extend(safe);
                }
            }
            KeyCode::Enter | KeyCode::Char('d') => {
                let items = self.recl_targets();
                self.open_preview(items);
            }
            KeyCode::Char('x') => {
                let items = self.recl_targets();
                self.request_exec(items);
            }
            KeyCode::Char('g') => {
                let id = self.reclaim[self.recl.sel].entity;
                self.goto(Target::Entity(id));
            }
            KeyCode::Esc => {
                if self.recl.marked.is_empty() {
                    return false;
                }
                self.recl.marked.clear();
            }
            _ => return false,
        }
        true
    }

    fn on_lines_key(&mut self, k: KeyEvent) -> bool {
        if self.tab == Tab::Diff {
            match k.code {
                KeyCode::Char('[') => {
                    self.diff.older();
                    return true;
                }
                KeyCode::Char(']') => {
                    self.diff.newer();
                    return true;
                }
                _ => {}
            }
        }
        let rows = self.rows_for(self.tab);
        let view = match self.tab {
            Tab::Physical => &mut self.phys,
            Tab::Reconcile => &mut self.reconc,
            _ => &mut self.diff.view,
        };
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => view.step(&rows, -1),
            KeyCode::Down | KeyCode::Char('j') => view.step(&rows, 1),
            KeyCode::PageUp => view.page(&rows, false),
            KeyCode::PageDown => view.page(&rows, true),
            KeyCode::Home => view.home(&rows),
            KeyCode::End | KeyCode::Char('G') => view.end(&rows),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('g') => {
                match view.current(&rows).and_then(|r| r.target) {
                    Some(t) => self.goto(t),
                    None => self.set_status("nothing to open here", Level::Info),
                }
            }
            _ => return false,
        }
        true
    }
}
