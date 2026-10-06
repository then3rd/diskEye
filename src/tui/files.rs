//! The Files tab: an ncdu-style browser over the unified tree (filesystem
//! roots stitched into their mountpoints), with an optional treemap.

use super::theme;
use super::treemap;
use crate::model::tree::{Kind, flags};
use crate::model::{NodeId, Snapshot};
use crate::views::{self, Metric};
use std::collections::HashMap;
use std::rc::Rc;

/// A directory being shown. `None` is the virtual top level (several roots).
pub type Dir = Option<NodeId>;

#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub dir: Dir,
    /// The entry that was selected (and descended into) in `dir`.
    pub entry: NodeId,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sort {
    Size,
    Name,
}

pub struct FilesState {
    pub metric: Metric,
    pub sort: Sort,
    pub top: Vec<NodeId>,
    pub cur: Dir,
    pub stack: Vec<Frame>,
    pub sel: usize,
    pub offset: usize,
    pub height: usize,
    pub filter: String,
    pub filter_editing: bool,
    pub treemap: bool,
    /// Area the treemap was last drawn in (for arrow-key navigation).
    pub tm_area: Rect,
    sorted: HashMap<(Dir, u8, Sort), Rc<Vec<NodeId>>>,
    view: Option<(ViewKey, Rc<Vec<NodeId>>)>,
    total: Option<((Dir, u8), u64)>,
}

const CACHE_DIRS: usize = 48;

/// Directory, metric, sort order and filter of the cached filtered listing.
type ViewKey = (Dir, Metric, Sort, String);

impl FilesState {
    pub fn new(snap: &Snapshot) -> Self {
        let top = views::top_roots(snap);
        let cur = if top.len() == 1 { Some(top[0]) } else { None };
        FilesState {
            metric: Metric::Alloc,
            sort: Sort::Size,
            top,
            cur,
            stack: Vec::new(),
            sel: 0,
            offset: 0,
            height: 20,
            filter: String::new(),
            filter_editing: false,
            treemap: false,
            tm_area: Rect::new(0, 0, 80, 20),
            sorted: HashMap::new(),
            view: None,
            total: None,
        }
    }

    /// Children of `dir`, sorted by the current metric / name. Cached per directory.
    pub fn sorted(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>, dir: Dir) -> Rc<Vec<NodeId>> {
        let key = (dir, self.metric as u8, self.sort);
        if let Some(v) = self.sorted.get(&key) {
            return v.clone();
        }
        let ids: Vec<NodeId> = match dir {
            None => self.top.clone(),
            Some(d) => snap.tree.children(d).collect(),
        };
        let v = sort_entries(snap, targets, ids, self.metric, self.sort);
        if self.sorted.len() >= CACHE_DIRS {
            self.sorted.clear();
        }
        let v = Rc::new(v);
        self.sorted.insert(key, v.clone());
        v
    }

    /// Entries of the current directory after filtering.
    pub fn entries(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>) -> Rc<Vec<NodeId>> {
        let key = (self.cur, self.metric, self.sort, self.filter.clone());
        if let Some((k, v)) = &self.view
            && *k == key
        {
            return v.clone();
        }
        let all = self.sorted(snap, targets, self.cur);
        let v = if self.filter.is_empty() {
            all
        } else {
            let needle = self.filter.to_lowercase();
            Rc::new(all.iter().copied().filter(|&id| matches_filter(snap, id, &needle)).collect())
        };
        self.view = Some((key, v.clone()));
        v
    }

    pub fn selected(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>) -> Option<NodeId> {
        let e = self.entries(snap, targets);
        e.get(self.sel).copied()
    }

    /// Size of the current directory under the current metric including the
    /// filesystems mounted directly below it (100% for the bars).
    pub fn dir_total(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>) -> u64 {
        let key = (self.cur, self.metric as u8);
        if let Some((k, v)) = self.total
            && k == key
        {
            return v;
        }
        let m = self.metric;
        let v = match self.cur {
            Some(d) => {
                let stitched: u64 = snap
                    .tree
                    .children(d)
                    .filter(|&c| snap.tree.node(c).has(flags::MOUNTPOINT))
                    .map(|c| views::effective(snap, targets, c, m).saturating_sub(m.of(snap, c)))
                    .sum();
                m.of(snap, d) + stitched
            }
            None => self.top.iter().map(|&r| views::effective(snap, targets, r, m)).sum(),
        };
        self.total = Some((key, v));
        v
    }

    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.sel = 0;
            self.offset = 0;
            return;
        }
        self.sel = self.sel.min(len - 1);
        let h = self.height.max(1);
        if self.sel < self.offset {
            self.offset = self.sel;
        } else if self.sel >= self.offset + h {
            self.offset = self.sel + 1 - h;
        }
        self.offset = self.offset.min(len.saturating_sub(h)).min(self.sel);
    }

    pub fn move_by(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>, delta: isize) {
        let len = self.entries(snap, targets).len();
        if len == 0 {
            return;
        }
        self.sel = (self.sel as isize + delta).clamp(0, len as isize - 1) as usize;
        self.clamp(len);
    }

    pub fn select_node(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>, node: NodeId) -> bool {
        let e = self.entries(snap, targets);
        if let Some(i) = e.iter().position(|&n| n == node) {
            self.sel = i;
            // Keep some context above the restored selection.
            if self.sel < self.offset || self.sel >= self.offset + self.height.max(1) {
                self.offset = self.sel.saturating_sub(self.height.max(1) / 3);
            }
            self.clamp(e.len());
            true
        } else {
            false
        }
    }

    /// Descend into the entry. Returns an error message when that isn't possible.
    pub fn descend(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>, entry: NodeId) -> Result<(), String> {
        let n = snap.tree.node(entry);
        if n.kind != Kind::Dir {
            return Err(format!("{} is not a directory", snap.tree.name(entry)));
        }
        let target = match views::mount_target(snap, targets, entry) {
            Some(t) => t,
            None if n.has(flags::MOUNTPOINT) => {
                return Err("the filesystem mounted here was not scanned".into());
            }
            None => entry,
        };
        if n.has(flags::DENIED) && n.child_count == 0 {
            return Err("permission denied while scanning this directory — re-run with sudo".into());
        }
        self.stack.push(Frame { dir: self.cur, entry, offset: self.offset });
        self.cur = Some(target);
        self.sel = 0;
        self.offset = 0;
        self.filter.clear();
        self.filter_editing = false;
        Ok(())
    }

    /// Go to the parent directory, restoring its previous selection.
    pub fn up(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>) -> bool {
        self.filter.clear();
        self.filter_editing = false;
        if let Some(f) = self.stack.pop() {
            self.cur = f.dir;
            self.offset = f.offset;
            if !self.select_node(snap, targets, f.entry) {
                self.sel = 0;
            }
            let len = self.entries(snap, targets).len();
            self.clamp(len);
            return true;
        }
        // Jumped straight into a root: the virtual top lists all roots.
        if let Some(c) = self.cur
            && self.top.len() > 1
        {
            self.cur = None;
            self.offset = 0;
            self.select_node(snap, targets, c);
            return true;
        }
        false
    }

    /// Show `node`: open it when it's a directory and `open` is set, otherwise
    /// open its parent with it selected. Rebuilds the stack through mount stitching.
    pub fn jump(&mut self, snap: &Snapshot, targets: &HashMap<String, NodeId>, node: NodeId, open: bool) {
        let n = snap.tree.node(node);
        let (dir, entry) = if open && n.kind == Kind::Dir {
            (Some(node), None)
        } else {
            match self.display_parent(snap, node) {
                Some((d, e)) => (d, Some(e)),
                None => (Some(node), None),
            }
        };
        // Frames from the top down to `dir`.
        let mut frames = Vec::new();
        if let Some(d) = dir {
            let mut x = d;
            while let Some((pd, pe)) = self.display_parent(snap, x) {
                frames.push(Frame { dir: pd, entry: pe, offset: 0 });
                match pd {
                    Some(p) => x = p,
                    None => break,
                }
                if frames.len() > 4096 {
                    break;
                }
            }
        }
        frames.reverse();
        self.filter.clear();
        self.filter_editing = false;
        self.stack.clear();
        // Restore sensible scroll positions on the way back up.
        for f in frames {
            self.cur = f.dir;
            self.offset = 0;
            self.select_node(snap, targets, f.entry);
            self.stack.push(Frame { offset: self.offset, ..f });
        }
        self.cur = dir;
        self.sel = 0;
        self.offset = 0;
        if let Some(e) = entry {
            self.select_node(snap, targets, e);
        }
    }

    /// The directory that displays `node` as an entry, and the entry itself
    /// (a mountpoint node stands in for the root of the filesystem mounted there).
    fn display_parent(&self, snap: &Snapshot, node: NodeId) -> Option<(Dir, NodeId)> {
        if let Some(p) = snap.tree.parent(node) {
            return Some((Some(p), node));
        }
        // `node` is a filesystem root.
        if let Some(mp) = mountpoint_of_root(snap, node) {
            return snap.tree.parent(mp).map(|p| (Some(p), mp));
        }
        (self.top.len() > 1 && self.top.contains(&node)).then_some((None, node))
    }

    pub fn path_label(&self, snap: &Snapshot) -> String {
        match self.cur {
            Some(d) => snap.tree.path(d),
            None => "all scanned filesystems".into(),
        }
    }
}

fn matches_filter(snap: &Snapshot, id: NodeId, needle_lower: &str) -> bool {
    let name = snap.tree.name_bytes(id);
    if needle_lower.is_ascii() {
        let nb = needle_lower.as_bytes();
        if nb.len() > name.len() {
            return false;
        }
        name.windows(nb.len()).any(|w| w.eq_ignore_ascii_case(nb))
    } else {
        String::from_utf8_lossy(name).to_lowercase().contains(needle_lower)
    }
}

pub fn sort_entries(
    snap: &Snapshot,
    targets: &HashMap<String, NodeId>,
    ids: Vec<NodeId>,
    m: Metric,
    sort: Sort,
) -> Vec<NodeId> {
    match sort {
        Sort::Size => {
            let mut keyed: Vec<(u64, NodeId)> =
                ids.into_iter().map(|c| (views::effective(snap, targets, c, m), c)).collect();
            keyed.sort_unstable_by(|a, b| {
                b.0.cmp(&a.0).then_with(|| snap.tree.name_bytes(a.1).cmp(snap.tree.name_bytes(b.1)))
            });
            keyed.into_iter().map(|(_, c)| c).collect()
        }
        Sort::Name => {
            let mut v = ids;
            // Directories first, then case-insensitive name.
            v.sort_unstable_by(|&a, &b| {
                let (na, nb) = (snap.tree.node(a), snap.tree.node(b));
                (na.kind != Kind::Dir).cmp(&(nb.kind != Kind::Dir)).then_with(|| {
                    let (x, y) = (snap.tree.name_bytes(a), snap.tree.name_bytes(b));
                    x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()).then(x.cmp(y))
                })
            });
            v
        }
    }
}

/// The mountpoint node (in another scanned filesystem) where the filesystem rooted at `root` is mounted.
pub fn mountpoint_of_root(snap: &Snapshot, root: NodeId) -> Option<NodeId> {
    let fs = snap.filesystems.iter().find(|f| f.root_node == Some(root))?;
    let path = fs.scan_root.as_str();
    let parent = snap
        .filesystems
        .iter()
        .filter(|f| f.root_node.is_some() && f.root_node != Some(root))
        .filter(|f| f.scan_root != path && crate::scan::mounts::is_path_under(path, &f.scan_root))
        .max_by_key(|f| f.scan_root.len())?;
    let mut node = parent.root_node?;
    let rest = path[parent.scan_root.len()..].trim_start_matches('/');
    for comp in rest.split('/').filter(|c| !c.is_empty()) {
        node = snap.tree.child_by_name(node, comp.as_bytes())?;
    }
    Some(node)
}

// ------------------------------------------------------------------ render

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

pub struct Ctx<'a> {
    pub snap: &'a Snapshot,
    pub targets: &'a HashMap<String, NodeId>,
    pub by_node: &'a HashMap<NodeId, Vec<u32>>,
}

/// Treemap blocks for the current entries: (entry or None for "others", size).
pub fn treemap_items(st: &mut FilesState, c: &Ctx, area: Rect) -> Vec<(Option<NodeId>, u64)> {
    let entries = st.entries(c.snap, c.targets);
    let m = st.metric;
    let mut sized: Vec<(NodeId, u64)> =
        entries.iter().map(|&e| (e, views::effective(c.snap, c.targets, e, m))).filter(|(_, s)| *s > 0).collect();
    if st.sort == Sort::Name {
        sized.sort_by_key(|x| std::cmp::Reverse(x.1));
    }
    let total: u64 = sized.iter().map(|x| x.1).sum();
    let cells = (area.width as u64 * area.height as u64).max(1);
    let mut out = Vec::new();
    let mut rest = 0u64;
    for (e, s) in sized {
        // Keep blocks of at least ~3 cells, at most a few hundred.
        if out.len() < 300 && (s as u128 * cells as u128) >= 3 * total as u128 {
            out.push((Some(e), s));
        } else {
            rest += s;
        }
    }
    if rest > 0 {
        out.push((None, rest));
    }
    out
}

pub fn render(st: &mut FilesState, c: &Ctx, area: Rect, buf: &mut Buffer) {
    if area.height < 4 {
        return;
    }
    let snap = c.snap;
    let m = st.metric;
    // Line 0: location + totals.
    let total = st.dir_total(snap, c.targets);
    let entries = st.entries(snap, c.targets);
    let sort = match st.sort {
        Sort::Size => format!("by {}", m.label()),
        Sort::Name => "by name".into(),
    };
    let head = Line::from(vec![
        Span::styled(" ", Style::new()),
        Span::styled(
            theme::trunc_left(&st.path_label(snap), (area.width as usize).saturating_sub(40).max(10)),
            theme::bold(),
        ),
        Span::styled(format!("  {}  ", m.fmt(total)), theme::accent()),
        Span::styled(format!("{} entries, sorted {sort}", entries.len()), theme::dim()),
    ]);
    buf.set_line(area.x, area.y, &head, area.width);
    let mut list_area = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    // Filter line.
    if st.filter_editing || !st.filter.is_empty() {
        let l = Line::from(vec![
            Span::styled(" filter: ", theme::warn()),
            Span::styled(st.filter.clone(), theme::bold()),
            Span::styled(if st.filter_editing { "▏" } else { "" }, theme::bold()),
            Span::styled(
                if st.filter_editing { "  (Enter: keep, Esc: clear)" } else { "  (/ edit, Esc clear)" },
                theme::dim(),
            ),
        ]);
        buf.set_line(list_area.x, list_area.y, &l, list_area.width);
        list_area.y += 1;
        list_area.height = list_area.height.saturating_sub(1);
    }
    // Info lines about the selection at the bottom.
    let info_h = if list_area.height >= 8 { 2 } else { 0 };
    let body = Rect::new(list_area.x, list_area.y, list_area.width, list_area.height - info_h);
    let info = Rect::new(list_area.x, list_area.y + body.height, list_area.width, info_h);

    if entries.is_empty() {
        let msg = if !st.filter.is_empty() {
            "no entries match the filter".to_string()
        } else if let Some(d) = st.cur {
            let n = snap.tree.node(d);
            if n.has(flags::DENIED) {
                "permission denied — contents unknown (re-run with sudo)".into()
            } else {
                "empty directory".into()
            }
        } else {
            "nothing was scanned".into()
        };
        buf.set_string(body.x + 2, body.y + 1, msg, theme::dim());
        return;
    }

    if st.treemap {
        render_treemap(st, c, body, buf);
    } else {
        render_list(st, c, &entries, total, body, buf);
    }
    if info_h > 0
        && let Some(sel) = entries.get(st.sel).copied()
    {
        render_info(c, sel, info, buf);
    }
}

fn render_list(st: &mut FilesState, c: &Ctx, entries: &[NodeId], total: u64, area: Rect, buf: &mut Buffer) {
    let snap = c.snap;
    let m = st.metric;
    st.height = area.height as usize;
    st.clamp(entries.len());
    let w = area.width as usize;
    let show_items = w >= 72;
    let show_age = w >= 64;
    for (row, &id) in entries.iter().enumerate().skip(st.offset).take(area.height as usize) {
        let y = area.y + (row - st.offset) as u16;
        let n = snap.tree.node(id);
        let v = views::effective(snap, c.targets, id, m);
        let frac = if total > 0 { v as f64 / total as f64 } else { 0.0 };
        let mut spans = vec![
            Span::raw(format!("{:>10} ", m.fmt(v))),
            Span::styled(format!("{:>5.1}% ", frac * 100.0), theme::dim()),
        ];
        spans.extend(theme::bar(frac, 10, theme::accent()));
        let target = views::mount_target(snap, c.targets, id);
        let shown = target.unwrap_or(id);
        if show_items {
            let items = if n.kind == Kind::Dir { snap.tree.node(shown).items.to_string() } else { String::new() };
            spans.push(Span::styled(format!(" {items:>8}"), theme::dim()));
        }
        if show_age {
            spans.push(Span::styled(format!(" {:>4}", theme::age(snap.tree.node(shown).mtime)), theme::dim()));
        }
        spans.push(Span::raw("  "));
        let name = snap.tree.name(id).into_owned();
        let (suffix, name_style) = match n.kind {
            Kind::Dir if n.has(flags::MOUNTPOINT) => ("/", theme::bold().fg(ratatui::style::Color::Blue)),
            Kind::Dir => ("/", theme::bold()),
            Kind::Symlink => ("@", theme::dim()),
            Kind::Special => ("=", theme::dim()),
            Kind::File => ("", Style::new()),
        };
        let name_style = if n.has(flags::HARDLINK_DUP) { name_style.patch(theme::dim()) } else { name_style };
        spans.push(Span::styled(format!("{name}{suffix}"), name_style));
        for b in views::badges(snap, c.by_node, id) {
            let st = if c.by_node.contains_key(&id) && !b.starts_with("mountpoint") && b.contains(": ") {
                theme::owner()
            } else if b.starts_with("permission") || b == "incomplete" {
                theme::warn()
            } else {
                theme::dim()
            };
            spans.push(Span::styled(format!(" [{b}]"), st));
        }
        if n.has(flags::MOUNTPOINT) && target.is_none() {
            spans.push(Span::styled(" (not scanned)", theme::dim()));
        }
        let line = Line::from(spans);
        buf.set_line(area.x, y, &line, area.width.saturating_sub(1));
        if row == st.sel {
            buf.set_style(Rect::new(area.x, y, area.width.saturating_sub(1), 1), theme::selected());
        }
    }
    if entries.len() > area.height as usize {
        super::lineview::draw_scroll_hint(buf, area, st.offset, entries.len());
    }
}

fn render_treemap(st: &mut FilesState, c: &Ctx, area: Rect, buf: &mut Buffer) {
    let snap = c.snap;
    st.tm_area = area;
    let items = treemap_items(st, c, area);
    let sizes: Vec<u64> = items.iter().map(|x| x.1).collect();
    let rects = treemap::layout(&sizes, area);
    let entries = st.entries(snap, c.targets);
    let cur = entries.get(st.sel).copied();
    let mut sel = items.iter().position(|x| x.0.is_some() && x.0 == cur);
    if sel.is_none() {
        // Selection is hidden in "others".
        sel = items.iter().position(|x| x.0.is_none());
    }
    let labels: Vec<(String, String)> = items
        .iter()
        .map(|(e, s)| match e {
            Some(id) => {
                let n = snap.tree.node(*id);
                let suf = if n.kind == Kind::Dir { "/" } else { "" };
                (format!("{}{suf}", snap.tree.name(*id)), st.metric.fmt(*s))
            }
            None => (format!("{} smaller", entries.len() + 1 - items.len()), st.metric.fmt(*s)),
        })
        .collect();
    let blocks: Vec<treemap::Block> = items
        .iter()
        .zip(&rects)
        .zip(&labels)
        .map(|((it, r), (l, s))| treemap::Block { rect: *r, label: l, sub: s, other: it.0.is_none() })
        .collect();
    treemap::paint(buf, &blocks, sel);
}

fn render_info(c: &Ctx, id: NodeId, area: Rect, buf: &mut Buffer) {
    let snap = c.snap;
    let shown = views::mount_target(snap, c.targets, id).unwrap_or(id);
    let n = snap.tree.node(shown);
    let path = snap.tree.path(id);
    let line1 = Line::from(vec![
        Span::styled(" ", Style::new()),
        Span::styled(theme::trunc_left(&path, area.width.saturating_sub(2) as usize), theme::bold()),
    ]);
    let mut spans = vec![Span::styled(
        format!(
            " disk {} · apparent {} · {} items · modified {}",
            crate::model::fmt_size(views::effective(snap, c.targets, id, Metric::Alloc)),
            crate::model::fmt_size(views::effective(snap, c.targets, id, Metric::Apparent)),
            n.items,
            if n.mtime > 0 { crate::util::timestamp_human(n.mtime) } else { "-".into() }
        ),
        theme::dim(),
    )];
    let owners = crate::model::attribution::owners(snap, c.by_node, id);
    if let Some(&e) = owners.first() {
        let e = &snap.entities[e as usize];
        spans.push(Span::styled(format!(" · owned by {}: {}", views::kind_label(&e.kind), e.name), theme::owner()));
        if e.reclaim.as_ref().is_some_and(|r| r.action.is_some()) {
            spans.push(Span::styled(" (d: cleanup)", theme::good()));
        }
    }
    buf.set_line(area.x, area.y, &line1, area.width);
    buf.set_line(area.x, area.y + 1, &Line::from(spans), area.width);
}
