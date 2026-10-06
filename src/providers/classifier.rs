//! Path classifier: rules from the built-in `rules/builtin.toml` plus the
//! user's `~/.config/diskeye/rules.toml` label caches, package caches, build
//! artifacts, models and trash with a risk and a cleanup action.
//!
//! Path rules (`[[path]]`) are globs with `~`/`{mount}`/`{kernel}` expansion;
//! directory-name rules (`[[dir]]`, e.g. `node_modules`) are matched anywhere
//! in one pass over the tree. Nothing already owned by an earlier provider or
//! an earlier rule is claimed again, so totals never double count.
//!
//! Also home to small tree helpers (claim index, globbing) shared by the
//! VM, flatpak and journald providers.

use super::{Ctx, Outcome, Provider};
use crate::model::tree::{Kind, flags};
use crate::model::{ActionSpec, ActionStep, Entity, FileTree, NodeId, Reclaim, Risk, Snapshot};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};

const BUILTIN: &str = include_str!("../../rules/builtin.toml");
/// Children shown per aggregated rule; the rest are folded into one entity.
const MAX_CHILDREN: usize = 500;
/// Path-rule matches smaller than this are claimed but not reported.
const MIN_MATCH: u64 = 1 << 20;
/// Directory-name rules never look inside these (package-owned files).
const SYSTEM_PREFIXES: &[&str] = &[
    "/usr/",
    "/etc/",
    "/boot/",
    "/efi/",
    "/opt/",
    "/snap/",
    "/nix/",
    "/bin/",
    "/sbin/",
    "/lib/",
    "/lib32/",
    "/lib64/",
    "/proc/",
    "/sys/",
    "/dev/",
    "/var/lib/",
    "/var/cache/",
    "/var/db/",
    "/var/snap/",
    "/var/spool/",
    "/var/log/",
];

// ---------------------------------------------------------------- shared helpers

/// Tree nodes owned by entities so far, plus every ancestor of one, so
/// "is this inside a claim" and "does this contain a claim" are O(depth).
#[derive(Debug, Default, Clone)]
pub(crate) struct Claimed {
    nodes: HashSet<NodeId>,
    anc: HashSet<NodeId>,
}

impl Claimed {
    /// Index the paths of every entity already in the snapshot.
    pub fn from_entities(snap: &Snapshot) -> Self {
        let mut c = Claimed::default();
        for p in snap.entities.iter().flat_map(|e| &e.paths) {
            if let Some(n) = snap.lookup_static(p) {
                c.add(&snap.tree, n);
            }
        }
        c
    }

    pub fn add(&mut self, tree: &FileTree, node: NodeId) {
        if self.nodes.insert(node) {
            for a in tree.ancestors(node).skip(1) {
                if !self.anc.insert(a) {
                    break;
                }
            }
        }
    }

    /// `node` or one of its ancestors is claimed.
    pub fn covers(&self, tree: &FileTree, node: NodeId) -> bool {
        tree.ancestors(node).any(|a| self.nodes.contains(&a))
    }

    /// The smallest set of nodes covering `node`'s subtree minus claimed subtrees.
    pub fn subtract(&self, tree: &FileTree, node: NodeId) -> Vec<NodeId> {
        if self.covers(tree, node) {
            return vec![];
        }
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if self.nodes.contains(&n) {
                continue;
            }
            if self.anc.contains(&n) {
                stack.extend(tree.children(n));
            } else {
                out.push(n);
            }
        }
        out
    }
}

/// `*` / `?` wildcard match of one path component.
pub(crate) fn wild_match(pat: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((sp, sn)) = star {
            p = sp + 1;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == b'*')
}

/// Follow a mountpoint placeholder to the root of the filesystem mounted there.
fn through_mount(snap: &Snapshot, n: NodeId) -> NodeId {
    if snap.tree.node(n).has(flags::MOUNTPOINT) { snap.lookup_static(&snap.tree.path(n)).unwrap_or(n) } else { n }
}

/// Resolve an absolute glob (wildcards within components) against the tree.
pub(crate) fn glob(snap: &Snapshot, pattern: &str) -> Vec<NodeId> {
    let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
    let wild = comps.iter().position(|c| c.contains(['*', '?'])).unwrap_or(comps.len());
    let Some(start) = snap.lookup_static(&format!("/{}", comps[..wild].join("/"))) else { return vec![] };
    let tree = &snap.tree;
    let mut cur = vec![through_mount(snap, start)];
    for c in &comps[wild..] {
        let mut next = Vec::new();
        for n in cur {
            if c.contains(['*', '?']) {
                next.extend(tree.children(n).filter(|&ch| wild_match(c.as_bytes(), tree.name_bytes(ch))));
            } else {
                next.extend(tree.child_by_name(n, c.as_bytes()));
            }
        }
        cur = next.into_iter().map(|n| through_mount(snap, n)).collect();
    }
    cur
}

/// Users whose `~` paths to inspect: (name, home). Root's home is added when root.
pub(crate) fn homes(ctx: &Ctx) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> =
        ctx.users.iter().map(|(_, n, h)| (n.clone(), h.to_string_lossy().trim_end_matches('/').to_string())).collect();
    if ctx.is_root && !ctx.users.iter().any(|(u, _, _)| *u == 0) {
        v.push(("root".into(), "/root".into()));
    }
    v.retain(|(_, h)| !h.is_empty() && h != "/");
    v
}

/// " (user)" when several users are inspected, so per-user entities stay distinct.
pub(crate) fn user_suffix(ctx: &Ctx, user: &str) -> String {
    if homes(ctx).len() > 1 { format!(" ({user})") } else { String::new() }
}

/// Shorten a path under a known home to `~/...` (`~user/...` with several users).
pub(crate) fn abbrev(homes: &[(String, String)], path: &str) -> String {
    for (u, h) in homes {
        if let Some(rest) = path.strip_prefix(h.as_str()).filter(|r| r.is_empty() || r.starts_with('/')) {
            return if homes.len() > 1 { format!("~{u}{rest}") } else { format!("~{rest}") };
        }
    }
    path.to_string()
}

pub(crate) fn alloc_of(tree: &FileTree, nodes: &[NodeId]) -> u64 {
    nodes.iter().map(|&n| tree.node(n).alloc).sum()
}

// ---------------------------------------------------------------- rules

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleRisk {
    Safe,
    #[default]
    Review,
    Danger,
}

impl From<RuleRisk> for Risk {
    fn from(r: RuleRisk) -> Risk {
        match r {
            RuleRisk::Safe => Risk::Safe,
            RuleRisk::Review => Risk::Review,
            RuleRisk::Danger => Risk::Danger,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    #[default]
    None,
    /// Delete the contents of matched directories.
    Empty,
    Trash,
    Delete,
    Command,
}

/// One classifier rule; `[[path]]` rules use `paths`, `[[dir]]` rules `names`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    pub group: String,
    pub kind: String,
    #[serde(default)]
    pub risk: RuleRisk,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub action: RuleAction,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub root: bool,
    pub requires: Option<String>,
    /// `chmod -R u+w` before emptying (read-only caches such as Go modules).
    #[serde(default)]
    pub writable: bool,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Skip a match that contains an entry with one of these names.
    #[serde(default)]
    pub skip_if_contains: Vec<String>,
    /// One child entity per entry of the matched directory.
    #[serde(default)]
    pub split: bool,
    /// Report all matches as one entity instead of one child per match.
    #[serde(default)]
    pub aggregate: bool,
    /// Special reclaim estimators: "paccache".
    pub estimate: Option<String>,
    pub min_size: Option<u64>,
    #[serde(default)]
    pub names: Vec<String>,
    /// The match's parent must contain one of these (wildcards allowed).
    #[serde(default)]
    pub sibling: Vec<String>,
    /// The match must contain one of these (wildcards allowed).
    #[serde(default)]
    pub contains: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleFile {
    #[serde(default)]
    pub disable: Vec<String>,
    #[serde(default)]
    pub path: Vec<Rule>,
    #[serde(default)]
    pub dir: Vec<Rule>,
}

/// User rules first (they win), then built-ins not overridden or disabled.
pub fn merge(builtin: RuleFile, user: RuleFile) -> (Vec<Rule>, Vec<Rule>) {
    let mut drop: HashSet<String> = user.disable.iter().cloned().collect();
    drop.extend(user.path.iter().chain(&user.dir).map(|r| r.name.clone()));
    let keep = |v: Vec<Rule>| v.into_iter().filter(|r| !drop.contains(&r.name)).collect::<Vec<_>>();
    let mut path = user.path;
    path.extend(keep(builtin.path));
    let mut dir = user.dir;
    dir.extend(keep(builtin.dir));
    path.retain(|r| !r.paths.is_empty());
    dir.retain(|r| !r.names.is_empty());
    (path, dir)
}

pub struct Classifier {
    builtin: &'static str,
}

impl Classifier {
    pub fn builtin() -> Self {
        Classifier { builtin: BUILTIN }
    }
}

/// Expansion context for rule patterns.
struct Env<'a> {
    snap: &'a Snapshot,
    homes: Vec<(String, String)>,
    mounts: Vec<String>,
    kernel: String,
    has: &'a dyn Fn(&str) -> bool,
}

impl Env<'_> {
    /// Expand `~`, `{mount}`, `{kernel}`: (home index for `~` patterns, pattern).
    fn expand(&self, pat: &str) -> Vec<(Option<usize>, String)> {
        let pat = pat.replace("{kernel}", &self.kernel);
        let mut v: Vec<(Option<usize>, String)> = if let Some(rest) = pat.strip_prefix('~') {
            self.homes.iter().enumerate().map(|(i, (_, h))| (Some(i), format!("{h}{rest}"))).collect()
        } else {
            vec![(None, pat)]
        };
        if v.iter().any(|(_, p)| p.contains("{mount}")) {
            v = v
                .into_iter()
                .flat_map(|(u, p)| {
                    self.mounts
                        .iter()
                        .map(move |m| (u, p.replace("{mount}", m.trim_end_matches('/'))))
                        .collect::<Vec<_>>()
                })
                .collect();
        }
        v
    }
}

/// One output entity before it is added: name, nodes, reclaim nodes.
struct Item {
    name: String,
    nodes: Vec<NodeId>,
}

fn path_of(tree: &FileTree, n: NodeId) -> String {
    tree.path(n)
}

/// Build the reclaim (risk, reason, action) for nodes matched by `rule`.
fn reclaim_for(rule: &Rule, env: &Env, nodes: &[NodeId], match_paths: &[String], estimate: Option<u64>) -> Reclaim {
    let tree = &env.snap.tree;
    let mut reason = rule.reason.clone();
    let steps: Vec<ActionStep> = match rule.action {
        RuleAction::None => vec![],
        RuleAction::Command => {
            let ok = !rule.command.is_empty() && rule.requires.as_deref().is_none_or(|p| (env.has)(p));
            if !ok {
                if let Some(p) = &rule.requires {
                    reason.push_str(&format!(" (install {p} for a cleanup command)"));
                }
                vec![]
            } else if rule.command.iter().any(|a| a.contains("{path}")) {
                match_paths
                    .iter()
                    .map(|p| ActionStep::Command {
                        argv: rule.command.iter().map(|a| a.replace("{path}", p)).collect(),
                        root: rule.root,
                    })
                    .collect()
            } else {
                vec![ActionStep::Command { argv: rule.command.clone(), root: rule.root }]
            }
        }
        RuleAction::Empty | RuleAction::Trash | RuleAction::Delete => {
            let mut steps = Vec::new();
            for &n in nodes {
                let path = path_of(tree, n);
                let is_dir = tree.node(n).kind == Kind::Dir;
                match rule.action {
                    RuleAction::Empty if is_dir => {
                        if rule.writable {
                            steps.push(ActionStep::Command {
                                argv: vec!["chmod".into(), "-R".into(), "u+w".into(), path.clone()],
                                root: false,
                            });
                        }
                        steps.push(ActionStep::EmptyDir { path });
                    }
                    RuleAction::Trash => steps.push(ActionStep::DeletePath { path, trash: true }),
                    _ => steps.push(ActionStep::DeletePath { path, trash: false }),
                }
            }
            steps
        }
    };
    let label = match rule.action {
        RuleAction::Command => rule.command.join(" "),
        RuleAction::Empty => format!("empty {}", rule.name),
        RuleAction::Trash => format!("move {} to trash", rule.name),
        _ => format!("delete {}", rule.name),
    };
    Reclaim {
        risk: rule.risk.into(),
        reason,
        estimate,
        action: (!steps.is_empty()).then_some(ActionSpec { label, steps }),
    }
}

/// Bytes `paccache -rk2` would free: all but the two newest versions of each package.
pub fn paccache_estimate(tree: &FileTree, dir: NodeId, keep: usize) -> u64 {
    // name -> version -> (newest mtime, bytes incl. .sig)
    let mut pkgs: HashMap<String, HashMap<String, (i64, u64)>> = HashMap::new();
    for c in tree.children(dir) {
        let n = tree.node(c);
        if n.kind != Kind::File {
            continue;
        }
        let name = tree.name(c);
        let base = name.strip_suffix(".sig").unwrap_or(&name);
        let Some(idx) = base.find(".pkg.tar") else { continue };
        let mut parts = base[..idx].rsplitn(4, '-');
        let (Some(_arch), Some(rel), Some(ver), Some(pkg)) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let e = pkgs.entry(pkg.to_string()).or_default().entry(format!("{ver}-{rel}")).or_default();
        e.0 = e.0.max(n.mtime);
        e.1 += n.alloc;
    }
    pkgs.values()
        .map(|vers| {
            let mut v: Vec<&(i64, u64)> = vers.values().collect();
            v.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
            v.iter().skip(keep).map(|(_, b)| b).sum::<u64>()
        })
        .sum()
}

fn rule_entity(rule: &Rule, name: String, parent: Option<u32>, paths: Vec<String>) -> Entity {
    Entity {
        kind: rule.kind.clone(),
        name,
        provider: "classifier".into(),
        group: rule.group.clone(),
        parent,
        paths,
        ..Default::default()
    }
}

/// Add a parent entity with one child per item (largest first, capped; the
/// rest folded into one child) and claim every node.
fn emit_aggregate(
    snap: &mut Snapshot,
    env: &Env,
    rule: &Rule,
    title: String,
    mut items: Vec<Item>,
    claimed: &mut Claimed,
) {
    items.retain(|i| !i.nodes.is_empty());
    if items.is_empty() {
        return;
    }
    let tree = &env.snap.tree;
    items.sort_by_key(|i| std::cmp::Reverse(alloc_of(tree, &i.nodes)));
    let min = rule.min_size.unwrap_or(MIN_MATCH);
    let cut = if rule.aggregate {
        0
    } else {
        items.iter().take(MAX_CHILDREN).take_while(|i| alloc_of(tree, &i.nodes) >= min).count()
    };
    let rest: Vec<Item> = items.split_off(cut);
    let mut out: Vec<Entity> = Vec::new();
    for it in &items {
        let paths: Vec<String> = it.nodes.iter().map(|&n| path_of(tree, n)).collect();
        let mut e = rule_entity(rule, it.name.clone(), None, paths.clone());
        e.reclaim = Some(reclaim_for(rule, env, &it.nodes, &paths, None));
        out.push(e);
    }
    let folded = (!rest.is_empty()).then(|| {
        let nodes: Vec<NodeId> = rest.iter().flat_map(|i| i.nodes.iter().copied()).collect();
        let paths: Vec<String> = nodes.iter().map(|&n| path_of(tree, n)).collect();
        let name = if rule.aggregate { title.clone() } else { format!("{title}: {} smaller matches", rest.len()) };
        let mut e = rule_entity(rule, name, None, paths.clone());
        e.reclaim = Some(reclaim_for(rule, env, &nodes, &paths, None));
        e.attrs.push(("matches".into(), rest.len().to_string()));
        e
    });
    for it in items.iter().chain(&rest) {
        for &n in &it.nodes {
            claimed.add(tree, n);
        }
    }
    // Aggregate-only rules are a single entity; otherwise a parent with children.
    if rule.aggregate {
        snap.add_entity(folded.into_iter().next().unwrap_or_default());
        return;
    }
    out.extend(folded);
    let parent = snap.add_entity(rule_entity(rule, title, None, vec![]));
    for mut e in out {
        e.parent = Some(parent);
        snap.add_entity(e);
    }
}

/// Apply one path rule, adding its entities and claiming what it matched.
fn apply_path_rule(snap: &mut Snapshot, env: &Env, ctx: &Ctx, rule: &Rule, claimed: &mut Claimed) {
    let tree = &env.snap.tree;
    let excluded: HashSet<NodeId> =
        rule.exclude.iter().flat_map(|p| env.expand(p)).flat_map(|(_, p)| glob(env.snap, &p)).collect();
    let mut per_user: BTreeMap<Option<usize>, Vec<NodeId>> = BTreeMap::new();
    for pat in &rule.paths {
        for (u, p) in env.expand(pat) {
            for n in glob(env.snap, &p) {
                if excluded.contains(&n) {
                    continue;
                }
                if rule
                    .skip_if_contains
                    .iter()
                    .any(|c| tree.children(n).any(|ch| wild_match(c.as_bytes(), tree.name_bytes(ch))))
                {
                    continue;
                }
                per_user.entry(u).or_default().push(n);
            }
        }
    }
    for (u, mut nodes) in per_user {
        nodes.sort_unstable();
        nodes.dedup();
        let set: HashSet<NodeId> = nodes.iter().copied().collect();
        nodes.retain(|&n| !tree.ancestors(n).skip(1).any(|a| set.contains(&a)));
        let suffix = u.map(|i| user_suffix(ctx, &env.homes[i].0)).unwrap_or_default();
        let title = format!("{}{suffix}", rule.name);
        if rule.split {
            let mut items = Vec::new();
            for &m in &nodes {
                for c in tree.children(m) {
                    let name = abbrev(&env.homes, &tree.path(c));
                    items.push(Item { name, nodes: claimed.subtract(tree, c) });
                }
            }
            emit_aggregate(snap, env, rule, title, items, claimed);
            continue;
        }
        let kept: Vec<NodeId> = nodes.iter().flat_map(|&n| claimed.subtract(tree, n)).collect();
        if kept.is_empty() {
            continue;
        }
        for &n in &kept {
            claimed.add(tree, n);
        }
        if alloc_of(tree, &kept) < rule.min_size.unwrap_or(MIN_MATCH) {
            continue;
        }
        let mut rule = rule.clone();
        let estimate = match rule.estimate.as_deref() {
            Some("paccache") => {
                let old: u64 = nodes.iter().map(|&n| paccache_estimate(tree, n, 2)).sum();
                if old == 0 {
                    // Nothing old to prune: the cache holds only current versions.
                    rule.risk = RuleRisk::Review;
                    rule.reason =
                        "cached copies of installed packages; only needed to downgrade or reinstall offline".into();
                    rule.command = vec!["paccache".into(), "-rk0".into()];
                    None
                } else {
                    Some(old)
                }
            }
            _ => None,
        };
        let rule = &rule;
        let match_paths: Vec<String> = nodes.iter().map(|&n| path_of(tree, n)).collect();
        let paths: Vec<String> = kept.iter().map(|&n| path_of(tree, n)).collect();
        let mut e = rule_entity(rule, title, None, paths);
        e.reclaim = Some(reclaim_for(rule, env, &kept, &match_paths, estimate));
        if match_paths.len() == 1 {
            e.attrs.push(("path".into(), match_paths[0].clone()));
        }
        snap.add_entity(e);
    }
}

fn any_child_matches(tree: &FileTree, dir: NodeId, pats: &[String]) -> bool {
    tree.children(dir).any(|c| pats.iter().any(|p| wild_match(p.as_bytes(), tree.name_bytes(c))))
}

/// Directory-name rules: one pass over all nodes, then cheap checks on the
/// (few) candidates. Returns (rule index, node, path) for accepted matches.
pub fn dir_matches(snap: &Snapshot, rules: &[Rule], claimed: &Claimed) -> Vec<(usize, NodeId, String)> {
    let tree = &snap.tree;
    // Quick reject on the first byte unless a pattern starts with a wildcard.
    let mut first = [false; 256];
    for p in rules.iter().flat_map(|r| &r.names) {
        match p.as_bytes().first() {
            Some(b'*') | Some(b'?') | None => first = [true; 256],
            Some(&b) => first[b as usize] = true,
        }
    }
    let accepts = |rule: &Rule, id: NodeId, name: &[u8]| {
        rule.names.iter().any(|p| wild_match(p.as_bytes(), name))
            && (rule.contains.is_empty() || any_child_matches(tree, id, &rule.contains))
            && (rule.sibling.is_empty() || tree.parent(id).is_some_and(|p| any_child_matches(tree, p, &rule.sibling)))
    };
    let mut cands: Vec<(usize, NodeId)> = Vec::new();
    for (i, n) in tree.nodes.iter().enumerate() {
        if n.kind != Kind::Dir || n.name_len == 0 {
            continue;
        }
        let id = i as NodeId;
        let name = tree.name_bytes(id);
        if !first[name[0] as usize] {
            continue;
        }
        if let Some(r) = rules.iter().position(|r| accepts(r, id, name)) {
            cands.push((r, id));
        }
    }
    let set: HashSet<NodeId> = cands.iter().map(|&(_, id)| id).collect();
    cands
        .into_iter()
        .filter(|&(_, id)| {
            !tree.ancestors(id).skip(1).any(|a| set.contains(&a) || tree.name_bytes(a).first() == Some(&b'.'))
        })
        .filter_map(|(r, id)| {
            let path = tree.path(id);
            let sys = SYSTEM_PREFIXES.iter().any(|p| path.starts_with(p));
            (!sys && !claimed.covers(tree, id)).then_some((r, id, path))
        })
        .collect()
}

impl Provider for Classifier {
    fn name(&self) -> &'static str {
        "classifier"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let mut outcome = Outcome::complete();
        let builtin: RuleFile = toml::from_str(self.builtin).unwrap_or_else(|e| {
            outcome.degrade(format!("built-in rules are invalid: {e}"));
            RuleFile::default()
        });
        let user_file = ctx.home.join(".config/diskeye/rules.toml");
        let user: RuleFile = match ctx.runner.read(&user_file.to_string_lossy()) {
            Some(s) => toml::from_str(&s).unwrap_or_else(|e| {
                outcome.degrade(format!("{}: {e}", user_file.display()));
                RuleFile::default()
            }),
            None => RuleFile::default(),
        };
        let (path_rules, dir_rules) = merge(builtin, user);

        let mut claimed = Claimed::from_entities(snap);
        // Work on a read-only view of the tree while adding entities.
        let view = Snapshot {
            tree: std::mem::take(&mut snap.tree),
            filesystems: snap.filesystems.clone(),
            ..Default::default()
        };
        let before = snap.entities.len();
        let has = |p: &str| ctx.runner.has(p);
        let env = Env {
            snap: &view,
            homes: homes(ctx),
            mounts: view.filesystems.iter().filter(|f| f.root_node.is_some()).map(|f| f.scan_root.clone()).collect(),
            kernel: snap.meta.kernel.clone(),
            has: &has,
        };
        for rule in &path_rules {
            apply_path_rule(snap, &env, ctx, rule, &mut claimed);
        }
        let matches = dir_matches(&view, &dir_rules, &claimed);
        let mut by_rule: BTreeMap<usize, Vec<Item>> = BTreeMap::new();
        for (r, id, path) in matches {
            let nodes = claimed.subtract(&view.tree, id);
            by_rule.entry(r).or_default().push(Item { name: abbrev(&env.homes, &path), nodes });
        }
        for (r, items) in by_rule {
            let rule = &dir_rules[r];
            emit_aggregate(snap, &env, rule, rule.name.clone(), items, &mut claimed);
        }
        drop(env);
        snap.tree = view.tree;
        let added = snap.entities.len() - before;
        if added == 0 && outcome.notes.is_empty() {
            return Outcome::absent();
        }
        outcome.note(format!("{} rules, {added} entities", path_rules.len() + dir_rules.len()))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::model::tree::Kind;
    use crate::model::{FileTree, FsInfo, Snapshot};
    use crate::providers::Ctx;
    use crate::providers::runner::FakeRunner;
    use crate::scan::walker::{TmpDir, TmpEntry};

    /// A one-filesystem snapshot rooted at `/` from `(path, bytes)` entries; a
    /// trailing `/` makes a directory. Later entries get newer mtimes.
    pub fn snap_from(entries: &[(&str, u64)]) -> Snapshot {
        let mut root = TmpDir { name: b"/".to_vec().into(), ..Default::default() };
        for (i, (p, size)) in entries.iter().enumerate() {
            let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
            let (dirs, file) =
                if p.ends_with('/') { (&comps[..], None) } else { (&comps[..comps.len() - 1], comps.last()) };
            let mut d = &mut root;
            for c in dirs {
                let idx = match d.dirs.iter().position(|x| &*x.name == c.as_bytes()) {
                    Some(i) => i,
                    None => {
                        d.dirs.push(TmpDir { name: c.as_bytes().into(), ..Default::default() });
                        d.dirs.len() - 1
                    }
                };
                d = &mut d.dirs[idx];
            }
            if let Some(f) = file {
                d.files.push(TmpEntry {
                    name: f.as_bytes().into(),
                    kind: Kind::File,
                    flags: 0,
                    apparent: *size,
                    alloc: *size,
                    mtime: i as i64,
                });
            }
        }
        fn agg(d: &mut TmpDir) {
            for c in d.dirs.iter_mut() {
                agg(c);
            }
            d.alloc = d.files.iter().map(|f| f.alloc).sum::<u64>() + d.dirs.iter().map(|c| c.alloc).sum::<u64>();
            d.apparent = d.alloc;
            d.items = 1 + d.files.len() as u32 + d.dirs.iter().map(|c| c.items).sum::<u32>();
        }
        agg(&mut root);
        let mut tree = FileTree::default();
        let mut info = FsInfo { mount_point: "/".into(), scan_root: "/".into(), ..Default::default() };
        let r = crate::scan::append_tree(&mut tree, root, &mut info);
        info.root_node = Some(r);
        tree.roots.push(r);
        Snapshot { tree, filesystems: vec![info], ..Default::default() }
    }

    pub fn ctx<'a>(runner: &'a FakeRunner, users: &[(&str, &str)]) -> Ctx<'a> {
        Ctx {
            runner,
            is_root: false,
            uid: 1000,
            home: users.first().map(|(_, h)| h.into()).unwrap_or_else(|| "/home/u".into()),
            mounts: &[],
            users: users.iter().enumerate().map(|(i, (n, h))| (1000 + i as u32, n.to_string(), h.into())).collect(),
        }
    }

    pub fn find<'a>(snap: &'a Snapshot, name: &str) -> &'a crate::model::Entity {
        snap.entities.iter().find(|e| e.name == name).unwrap_or_else(|| {
            panic!("no entity {name:?}; have {:?}", snap.entities.iter().map(|e| &e.name).collect::<Vec<_>>())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::model::attribution::attribute;
    use crate::providers::runner::FakeRunner;

    const MB: u64 = 1 << 20;

    #[test]
    fn builtin_rules_parse() {
        let f: RuleFile = toml::from_str(BUILTIN).unwrap();
        assert!(f.path.len() > 20 && f.dir.len() > 5);
        assert!(f.path.iter().all(|r| !r.paths.is_empty()));
        assert!(f.dir.iter().all(|r| !r.names.is_empty()));
        assert_eq!(f.path.last().unwrap().name, "Other ~/.cache");
    }

    #[test]
    fn wildcards() {
        assert!(wild_match(b"*", b"anything"));
        assert!(wild_match(b".Trash-*", b".Trash-1000"));
        assert!(!wild_match(b".Trash-*", b".trash-1000"));
        assert!(wild_match(b"cmake-build-*", b"cmake-build-debug"));
        assert!(wild_match(b"a?c", b"abc"));
        assert!(!wild_match(b"target", b"targets"));
        assert!(wild_match(b"*.tf", b"main.tf"));
    }

    #[test]
    fn claimed_subtract() {
        let snap = snap_from(&[("/a/b/x", 10), ("/a/b/y", 20), ("/a/c/z", 5)]);
        let mut c = Claimed::default();
        let x = snap.lookup("/a/b/x").unwrap();
        c.add(&snap.tree, x);
        let a = snap.lookup("/a").unwrap();
        let mut got: Vec<String> = c.subtract(&snap.tree, a).into_iter().map(|n| snap.tree.path(n)).collect();
        got.sort();
        assert_eq!(got, vec!["/a/b/y", "/a/c"]);
        assert!(c.subtract(&snap.tree, x).is_empty());
        assert!(c.covers(&snap.tree, x));
    }

    #[test]
    fn paccache_keeps_two_newest() {
        let snap = snap_from(&[
            ("/var/cache/pacman/pkg/foo-bar-1.0-1-x86_64.pkg.tar.zst", 100),
            ("/var/cache/pacman/pkg/foo-bar-1.0-1-x86_64.pkg.tar.zst.sig", 1),
            ("/var/cache/pacman/pkg/foo-bar-1.1-1-x86_64.pkg.tar.zst", 110),
            ("/var/cache/pacman/pkg/foo-bar-2.0-1-x86_64.pkg.tar.zst", 120),
            ("/var/cache/pacman/pkg/baz-3-2-any.pkg.tar.zst", 50),
        ]);
        let d = snap.lookup("/var/cache/pacman/pkg").unwrap();
        assert_eq!(paccache_estimate(&snap.tree, d, 2), 101);
    }

    fn sample() -> Snapshot {
        snap_from(&[
            ("/var/cache/pacman/pkg/linux-6.1-1-x86_64.pkg.tar.zst", 100 * MB),
            ("/var/cache/pacman/pkg/linux-6.2-1-x86_64.pkg.tar.zst", 100 * MB),
            ("/var/cache/pacman/pkg/linux-6.3-1-x86_64.pkg.tar.zst", 100 * MB),
            ("/home/u/.cache/pip/http/a", 30 * MB),
            ("/home/u/.cache/google-chrome/Default/Cache/data", 200 * MB),
            ("/home/u/.cache/some-app/blob", 40 * MB),
            ("/home/u/.cache/other-app/blob", 3 * MB),
            ("/home/u/.cache/tiny/blob", 1000),
            ("/home/u/.cache/docker-owned/blob", 70 * MB),
            ("/home/u/.local/share/Trash/files/old.iso", 500 * MB),
            ("/home/u/repos/proj/Cargo.toml", 1000),
            ("/home/u/repos/proj/target/debug/bin", 900 * MB),
            ("/home/u/repos/proj/target/debug/build/x/target/nested", 5 * MB),
            ("/home/u/repos/web/package.json", 1000),
            ("/home/u/repos/web/node_modules/a/index.js", 50 * MB),
            ("/home/u/repos/web/node_modules/a/node_modules/b/index.js", 10 * MB),
            ("/home/u/repos/py/.venv/pyvenv.cfg", 100),
            ("/home/u/repos/py/.venv/lib/x/__pycache__/m.pyc", 2 * MB),
            ("/home/u/repos/py/pkg/__pycache__/m.pyc", MB),
            ("/home/u/repos/notvenv/venv/lib/x", MB),
            ("/home/u/tools/mytool/pyvenv.cfg", 100),
            ("/home/u/tools/mytool/lib/p/__pycache__/x.pyc", 3 * MB),
            ("/home/u/repos/plain/target/x", 7 * MB),
            ("/home/u/.vscode/extensions/e/node_modules/x", 9 * MB),
            ("/usr/lib/node_modules/npm/x", 9 * MB),
            ("/usr/lib/modules/6.1.0-old/kernel/x.ko", 80 * MB),
            ("/usr/lib/modules/6.9.0-cur/kernel/x.ko", 90 * MB),
            ("/usr/lib/modules/6.9.0-cur/vmlinuz", 10 * MB),
            ("/usr/lib/modules/7.0.0-new/kernel/x.ko", 90 * MB),
            ("/usr/lib/modules/7.0.0-new/vmlinuz", 10 * MB),
        ])
    }

    fn run(snap: &mut Snapshot, runner: &FakeRunner) {
        snap.meta.kernel = "6.9.0-cur".into();
        // An earlier provider owns part of ~/.cache.
        snap.add_entity(Entity {
            name: "docker".into(),
            paths: vec!["/home/u/.cache/docker-owned".into()],
            ..Default::default()
        });
        let ctx = ctx(runner, &[("u", "/home/u")]);
        let out = Classifier::builtin().collect(&ctx, snap);
        assert_eq!(out.coverage, crate::model::Coverage::Complete, "{:?}", out.notes);
        attribute(snap);
    }

    #[test]
    fn classifies_sample_tree() {
        let mut snap = sample();
        let runner = FakeRunner::default().with("paccache -rk2", "");
        run(&mut snap, &runner);

        let pac = find(&snap, "pacman package cache");
        assert_eq!(pac.measured_alloc, 300 * MB);
        let r = pac.reclaim.as_ref().unwrap();
        assert_eq!(r.estimate, Some(100 * MB));
        assert_eq!(r.risk, Risk::Safe);
        assert!(matches!(&r.action.as_ref().unwrap().steps[0], ActionStep::Command { root: true, .. }));

        let chrome = find(&snap, "Google Chrome cache");
        assert_eq!(chrome.measured_alloc, 200 * MB);
        assert_eq!(
            chrome.reclaim.as_ref().unwrap().action.as_ref().unwrap().steps,
            vec![ActionStep::EmptyDir { path: "/home/u/.cache/google-chrome".into() }]
        );
        assert_eq!(find(&snap, "pip cache").measured_alloc, 30 * MB);
        assert_eq!(find(&snap, "Trash").measured_alloc, 500 * MB);

        // Catch-all: only children not claimed by specific rules or providers.
        let other = find(&snap, "Other ~/.cache");
        let kids: Vec<&str> =
            snap.entities.iter().filter(|e| e.parent == Some(other.id)).map(|e| e.name.as_str()).collect();
        assert_eq!(kids, vec!["~/.cache/some-app", "~/.cache/other-app", "Other ~/.cache: 1 smaller matches"]);
        assert_eq!(other.measured_alloc, 43 * MB + 1000);

        // Old kernel modules: neither running nor installed.
        let k = find(&snap, "Old kernel modules");
        assert_eq!(k.paths, vec!["/usr/lib/modules/6.1.0-old".to_string()]);

        // Directory rules: nested matches and system / hidden dirs skipped.
        let rust = find(&snap, "Rust build artifacts");
        assert_eq!(rust.measured_alloc, 905 * MB);
        let rust_kids: Vec<&Entity> = snap.entities.iter().filter(|e| e.parent == Some(rust.id)).collect();
        assert_eq!(rust_kids.len(), 1);
        assert_eq!(rust_kids[0].name, "~/repos/proj/target");
        assert_eq!(
            rust_kids[0].reclaim.as_ref().unwrap().action.as_ref().unwrap().steps,
            vec![ActionStep::DeletePath { path: "/home/u/repos/proj/target".into(), trash: true }]
        );
        let nm = find(&snap, "node_modules");
        assert_eq!(nm.measured_alloc, 60 * MB);
        assert_eq!(snap.entities.iter().filter(|e| e.parent == Some(nm.id)).count(), 1);
        // Venvs are found by content, whatever they are called.
        let venv = find(&snap, "Python virtualenvs");
        assert_eq!(venv.measured_alloc, 5 * MB + 200);
        assert!(snap.entities.iter().any(|e| e.parent == Some(venv.id) && e.name == "~/tools/mytool"));
        // __pycache__ inside venvs belongs to the venv; the rest is one aggregate entity.
        let pyc = find(&snap, "Python bytecode (__pycache__)");
        assert_eq!(pyc.measured_alloc, MB);
        assert!(pyc.parent.is_none() && snap.entities.iter().all(|e| e.parent != Some(pyc.id)));

        // Nothing is counted twice across classifier entities.
        let top: Vec<&Entity> =
            snap.entities.iter().filter(|e| e.parent.is_none() && e.provider == "classifier").collect();
        let sum: u64 = top.iter().map(|e| e.measured_alloc).sum();
        let mut nodes: Vec<NodeId> = snap
            .claims
            .iter()
            .filter(|c| snap.entities[c.entity as usize].provider == "classifier")
            .map(|c| c.node)
            .collect();
        nodes.sort_unstable();
        nodes.dedup();
        assert_eq!(sum, alloc_of(&snap.tree, &nodes));
    }

    #[test]
    fn pacman_cache_without_old_versions() {
        let mut snap = snap_from(&[
            ("/var/cache/pacman/pkg/linux-6.1-1-x86_64.pkg.tar.zst", 100 * MB),
            ("/var/cache/pacman/pkg/glibc-2.40-1-x86_64.pkg.tar.zst", 10 * MB),
        ]);
        run(&mut snap, &FakeRunner::default().with("paccache -rk0", ""));
        let r = find(&snap, "pacman package cache").reclaim.clone().unwrap();
        assert_eq!((r.risk, r.estimate), (Risk::Review, None));
        assert_eq!(
            r.action.unwrap().steps,
            vec![ActionStep::Command { argv: vec!["paccache".into(), "-rk0".into()], root: true }]
        );
    }

    #[test]
    fn paccache_missing_means_no_action() {
        let mut snap = sample();
        run(&mut snap, &FakeRunner::default());
        let r = find(&snap, "pacman package cache").reclaim.clone().unwrap();
        assert!(r.action.is_none());
        assert!(r.reason.contains("install paccache"));
    }

    #[test]
    fn user_rules_override_and_disable() {
        let mut snap = sample();
        let user = r#"
            disable = ["Trash"]
            [[path]]
            name = "pip cache"
            group = "Mine"
            kind = "devcache"
            risk = "danger"
            paths = ["~/.cache/pip"]
            [[dir]]
            name = "Plain targets"
            group = "Build artifacts"
            kind = "build-artifacts"
            names = ["target"]
            action = "delete"
        "#;
        let runner = FakeRunner::default().file("/home/u/.config/diskeye/rules.toml", user);
        run(&mut snap, &runner);
        let pip = find(&snap, "pip cache");
        assert_eq!(pip.group, "Mine");
        assert_eq!(pip.reclaim.as_ref().unwrap().risk, Risk::Danger);
        assert!(snap.entities.iter().all(|e| e.name != "Trash"));
        // The user's dir rule comes first and takes both target dirs.
        let t = find(&snap, "Plain targets");
        assert_eq!(snap.entities.iter().filter(|e| e.parent == Some(t.id)).count(), 2);
    }

    #[test]
    fn invalid_user_rules_degrade() {
        let mut snap = sample();
        let runner = FakeRunner::default().file("/home/u/.config/diskeye/rules.toml", "[[path]]\nnope = 1\n");
        let ctx = ctx(&runner, &[("u", "/home/u")]);
        let out = Classifier::builtin().collect(&ctx, &mut snap);
        assert_eq!(out.coverage, crate::model::Coverage::Partial);
        assert!(snap.entities.iter().any(|e| e.name == "Google Chrome cache"));
    }

    #[test]
    fn caps_children_and_folds_rest() {
        let owned: Vec<(String, u64)> =
            (0..(MAX_CHILDREN + 20)).map(|i| (format!("/home/u/p{i}/node_modules/x"), MB + i as u64)).collect();
        let entries: Vec<(&str, u64)> = owned.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        let mut snap = snap_from(&entries);
        run(&mut snap, &FakeRunner::default());
        let nm = find(&snap, "node_modules");
        let kids: Vec<&Entity> = snap.entities.iter().filter(|e| e.parent == Some(nm.id)).collect();
        assert_eq!(kids.len(), MAX_CHILDREN + 1);
        let folded = kids.iter().find(|e| e.name == "node_modules: 20 smaller matches").unwrap();
        assert_eq!(folded.paths.len(), 20);
        let total: u64 = entries.iter().map(|(_, s)| s).sum();
        assert_eq!(nm.measured_alloc, total);
    }

    #[test]
    fn multi_user_names() {
        let mut snap = snap_from(&[("/home/a/.cache/pip/x", 5 * MB), ("/home/b/.cache/pip/x", 6 * MB)]);
        let runner = FakeRunner::default();
        let ctx = ctx(&runner, &[("a", "/home/a"), ("b", "/home/b")]);
        Classifier::builtin().collect(&ctx, &mut snap);
        attribute(&mut snap);
        assert_eq!(find(&snap, "pip cache (a)").measured_alloc, 5 * MB);
        assert_eq!(find(&snap, "pip cache (b)").measured_alloc, 6 * MB);
    }
}
