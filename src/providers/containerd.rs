//! Standalone containerd and k3s/rke2's embedded containerd, plus helpers the
//! Docker provider shares for the containerd image store: a read-only bbolt
//! reader for snapshotter metadata, chain IDs, and content-store walks.
//!
//! Snapshot directories are named by a numeric id (`snapshots/<n>`) that only
//! the snapshotter's `metadata.db` maps to snapshot keys. Committed image layers
//! are keyed by their chain ID, which we derive from the image config's
//! `rootfs.diff_ids`, so every layer directory maps back to the images using it.

use super::{Ctx, Outcome, Provider};
use crate::model::{ActionSpec, ActionStep, Entity, NodeId, Reclaim, Risk, Snapshot};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub struct Containerd;

// ---------------------------------------------------------------- bbolt

/// Minimal read-only reader for bbolt databases (containerd's metadata stores).
pub mod bolt {
    const MAGIC: u32 = 0xED0C_DAED;
    const BRANCH: u16 = 0x01;
    const LEAF: u16 = 0x02;
    const BUCKET_LEAF: u32 = 0x01;
    const PAGE_HEADER: usize = 16;
    const ELEM: usize = 16;

    pub struct Db {
        data: Vec<u8>,
        page_size: usize,
        root: u64,
    }

    #[derive(Clone, Copy)]
    pub struct Bucket<'a> {
        db: &'a Db,
        /// Root page, or 0 for an inline bucket stored in `inline`.
        root: u64,
        inline: &'a [u8],
    }

    pub enum Val<'a> {
        Bytes(&'a [u8]),
        Bucket(Bucket<'a>),
    }

    fn u16_at(b: &[u8], o: usize) -> Option<u16> {
        Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
    }
    fn u32_at(b: &[u8], o: usize) -> Option<u32> {
        Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
    }
    fn u64_at(b: &[u8], o: usize) -> Option<u64> {
        Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
    }

    fn fnv64a(b: &[u8]) -> u64 {
        b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &c| (h ^ c as u64).wrapping_mul(0x0100_0000_01b3))
    }

    /// (page_size, root pgid, txid) of a valid meta page at `off`.
    fn meta(data: &[u8], off: usize) -> Option<(usize, u64, u64)> {
        let m = off + PAGE_HEADER;
        if u32_at(data, m)? != MAGIC || u32_at(data, m + 4)? != 2 {
            return None;
        }
        let checksum = u64_at(data, m + 56)?;
        if fnv64a(data.get(m..m + 56)?) != checksum {
            return None;
        }
        Some((u32_at(data, m + 8)? as usize, u64_at(data, m + 16)?, u64_at(data, m + 48)?))
    }

    impl Db {
        pub fn parse(data: Vec<u8>) -> Option<Db> {
            let m0 = meta(&data, 0);
            let ps = m0.map(|m| m.0).unwrap_or(4096);
            let m1 = meta(&data, ps);
            let (page_size, root, _) = match (m0, m1) {
                (Some(a), Some(b)) => {
                    if b.2 > a.2 {
                        b
                    } else {
                        a
                    }
                }
                (a, b) => a.or(b)?,
            };
            if !(512..=1 << 20).contains(&page_size) {
                return None;
            }
            Some(Db { data, page_size, root })
        }

        pub fn root(&self) -> Bucket<'_> {
            Bucket { db: self, root: self.root, inline: &[] }
        }
    }

    impl<'a> Bucket<'a> {
        /// All key/value pairs, in page order.
        pub fn entries(&self) -> Vec<(&'a [u8], Val<'a>)> {
            let mut out = Vec::new();
            if self.root == 0 {
                self.page_entries(self.inline, 0, 0, &mut out);
            } else {
                let data: &'a [u8] = &self.db.data;
                self.page_entries(data, self.root as usize * self.db.page_size, 0, &mut out);
            }
            out
        }

        fn page_entries(&self, buf: &'a [u8], off: usize, depth: u32, out: &mut Vec<(&'a [u8], Val<'a>)>) {
            if depth > 32 {
                return;
            }
            let (Some(flags), Some(count)) = (u16_at(buf, off + 8), u16_at(buf, off + 10)) else { return };
            for i in 0..count as usize {
                let e = off + PAGE_HEADER + i * ELEM;
                if flags & BRANCH != 0 {
                    let Some(pgid) = u64_at(buf, e + 8) else { return };
                    let data: &'a [u8] = &self.db.data;
                    self.page_entries(data, pgid as usize * self.db.page_size, depth + 1, out);
                } else if flags & LEAF != 0 {
                    let (Some(f), Some(pos), Some(ks), Some(vs)) =
                        (u32_at(buf, e), u32_at(buf, e + 4), u32_at(buf, e + 8), u32_at(buf, e + 12))
                    else {
                        return;
                    };
                    let k0 = e + pos as usize;
                    let (Some(key), Some(val)) =
                        (buf.get(k0..k0 + ks as usize), buf.get(k0 + ks as usize..k0 + (ks + vs) as usize))
                    else {
                        return;
                    };
                    if f & BUCKET_LEAF != 0 {
                        let Some(root) = u64_at(val, 0) else { continue };
                        let inline = if root == 0 { &val[PAGE_HEADER.min(val.len())..] } else { &[][..] };
                        out.push((key, Val::Bucket(Bucket { db: self.db, root, inline })));
                    } else {
                        out.push((key, Val::Bytes(val)));
                    }
                }
            }
        }

        pub fn get(&self, key: &[u8]) -> Option<Val<'a>> {
            self.entries().into_iter().find(|(k, _)| *k == key).map(|(_, v)| v)
        }

        pub fn bucket(&self, key: &[u8]) -> Option<Bucket<'a>> {
            match self.get(key)? {
                Val::Bucket(b) => Some(b),
                Val::Bytes(_) => None,
            }
        }

        pub fn bytes(&self, key: &[u8]) -> Option<&'a [u8]> {
            match self.get(key)? {
                Val::Bytes(b) => Some(b),
                Val::Bucket(_) => None,
            }
        }

        pub fn string(&self, key: &[u8]) -> Option<String> {
            self.bytes(key).map(|b| String::from_utf8_lossy(b).into_owned())
        }
    }

    pub fn uvarint(b: &[u8]) -> Option<u64> {
        let mut x = 0u64;
        for (i, &c) in b.iter().enumerate().take(10) {
            x |= ((c & 0x7f) as u64) << (7 * i);
            if c < 0x80 {
                return Some(x);
            }
        }
        None
    }

    /// Go's zig-zag `binary.Varint`.
    pub fn varint(b: &[u8]) -> Option<i64> {
        let u = uvarint(b)?;
        Some(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
}

// ---------------------------------------------------------------- sha256 / chain IDs

/// SHA-256 (FIPS 180-4); only used for layer chain IDs, so speed is irrelevant.
pub fn sha256_hex(msg: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut data = msg.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&((msg.len() as u64) * 8).to_be_bytes());
    for block in data.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, c) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*c);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// OCI chain IDs for a list of layer diff IDs (bottom first).
pub fn chain_ids(diff_ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(diff_ids.len());
    for d in diff_ids {
        let next = match out.last() {
            None => d.clone(),
            Some(prev) => format!("sha256:{}", sha256_hex(format!("{prev} {d}").as_bytes())),
        };
        out.push(next);
    }
    out
}

// ---------------------------------------------------------------- snapshotter metadata

pub const KIND_VIEW: u8 = 1;
pub const KIND_ACTIVE: u8 = 2;
pub const KIND_COMMITTED: u8 = 3;

/// One snapshot from a snapshotter's `metadata.db`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapRec {
    pub ns: String,
    /// Chain ID for image layers, container ID for rw layers, buildkit ref...
    pub name: String,
    /// Directory id: `snapshots/<id>`.
    pub id: u64,
    pub kind: u8,
    pub parent: Option<String>,
    pub size: Option<i64>,
}

/// Snapshots by (namespace, name).
#[derive(Debug, Default)]
pub struct SnapIndex {
    pub recs: HashMap<(String, String), SnapRec>,
}

/// Split a snapshotter key `ns/<seq>/<name>`.
fn split_key(key: &str) -> Option<(&str, &str)> {
    let mut it = key.splitn(3, '/');
    let ns = it.next()?;
    it.next()?;
    Some((ns, it.next()?))
}

impl SnapIndex {
    /// Parse the snapshotter's bbolt `metadata.db` (overlayfs, native, fuse-overlayfs...).
    pub fn from_bolt(bytes: Vec<u8>) -> Option<SnapIndex> {
        let db = bolt::Db::parse(bytes)?;
        let snaps = db.root().bucket(b"v1")?.bucket(b"snapshots")?;
        let mut recs = HashMap::new();
        for (k, v) in snaps.entries() {
            let bolt::Val::Bucket(b) = v else { continue };
            let key = String::from_utf8_lossy(k);
            let Some((ns, name)) = split_key(&key) else { continue };
            let Some(id) = b.bytes(b"id").and_then(bolt::uvarint) else { continue };
            let rec = SnapRec {
                ns: ns.to_string(),
                name: name.to_string(),
                id,
                kind: b.bytes(b"kind").and_then(|k| k.first().copied()).unwrap_or(0),
                parent: b.string(b"parent").and_then(|p| split_key(&p).map(|(_, n)| n.to_string())),
                size: b.bytes(b"size").and_then(bolt::varint),
            };
            recs.insert((rec.ns.clone(), rec.name.clone()), rec);
        }
        Some(SnapIndex { recs })
    }

    pub fn load(path: &str) -> Option<SnapIndex> {
        Self::from_bolt(std::fs::read(path).ok()?)
    }

    pub fn get(&self, ns: &str, name: &str) -> Option<&SnapRec> {
        self.recs.get(&(ns.to_string(), name.to_string()))
    }

    pub fn in_ns<'a>(&'a self, ns: &'a str) -> impl Iterator<Item = &'a SnapRec> + 'a {
        self.recs.values().filter(move |r| r.ns == ns)
    }

    /// Committed snapshots that an active snapshot or view sits directly on:
    /// the top layers of images some container uses.
    pub fn in_use(&self, ns: &str) -> HashSet<String> {
        self.in_ns(ns).filter(|r| r.kind != KIND_COMMITTED).filter_map(|r| r.parent.clone()).collect()
    }
}

// ---------------------------------------------------------------- content store

/// Blob path for `sha256:<hex>` in a content store root.
pub fn blob_path(content_root: &str, digest: &str) -> Option<String> {
    let (algo, hex) = digest.split_once(':')?;
    if hex.is_empty() || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{content_root}/blobs/{algo}/{hex}"))
}

#[derive(Debug, Default)]
pub struct ContentRefs {
    /// Every digest reachable from the root (indexes, manifests, configs, layers).
    pub blobs: Vec<String>,
    /// `rootfs.diff_ids` of each config that could be read.
    pub diff_ids: Vec<Vec<String>>,
}

fn is_index_or_manifest(media_type: &str) -> bool {
    media_type.contains("manifest") || media_type.contains("index")
}

/// Follow an image's target descriptor through the content store. Only
/// index/manifest/config JSON is read; layer blobs are just listed.
pub fn walk_content(
    content_root: &str,
    digest: &str,
    media_type: &str,
    read: &dyn Fn(&str) -> Option<String>,
) -> ContentRefs {
    let mut refs = ContentRefs::default();
    let mut seen = HashSet::new();
    let mut stack = vec![(digest.to_string(), media_type.to_string(), 0u32)];
    while let Some((dg, mt, depth)) = stack.pop() {
        if !seen.insert(dg.clone()) {
            continue;
        }
        refs.blobs.push(dg.clone());
        let is_config = mt.contains("config") || mt.contains("container.image");
        if depth > 4 || !(is_index_or_manifest(&mt) || is_config) {
            continue;
        }
        let Some(v) =
            blob_path(content_root, &dg).and_then(|p| read(&p)).and_then(|s| serde_json::from_str::<Value>(&s).ok())
        else {
            continue;
        };
        if let Some(diffs) = v.pointer("/rootfs/diff_ids").and_then(|d| d.as_array()) {
            refs.diff_ids.push(diffs.iter().filter_map(|d| d.as_str().map(String::from)).collect());
            continue;
        }
        let desc = |d: &Value| -> Option<(String, String)> {
            Some((
                d.get("digest")?.as_str()?.to_string(),
                d.get("mediaType").and_then(|m| m.as_str()).unwrap_or("").into(),
            ))
        };
        for key in ["manifests", "layers"] {
            for d in v.get(key).and_then(|m| m.as_array()).into_iter().flatten() {
                if let Some((dg, mt)) = desc(d) {
                    // Layers listed in a manifest are never JSON we need to read.
                    let mt = if key == "layers" { format!("layer:{mt}") } else { mt };
                    stack.push((dg, mt, depth + 1));
                }
            }
        }
        if let Some((dg, mt)) = v.get("config").and_then(desc) {
            let mt = if mt.is_empty() { "config".into() } else { mt };
            stack.push((dg, mt, depth + 1));
        }
    }
    refs
}

// ---------------------------------------------------------------- shared helpers

/// Maximal subtrees under `root` that no entity claims, as paths. Empty if
/// `root` isn't in the scanned tree.
pub fn unclaimed_under(snap: &Snapshot, root: &str) -> Vec<String> {
    let Some(root_node) = snap.lookup_static(root) else { return vec![] };
    let mut claimed: HashSet<NodeId> = HashSet::new();
    for e in &snap.entities {
        for p in &e.paths {
            if p.starts_with(root)
                && let Some(n) = snap.lookup_static(p)
            {
                claimed.insert(n);
            }
        }
    }
    let mut ancestors: HashSet<NodeId> = HashSet::new();
    for &n in &claimed {
        for a in snap.tree.ancestors(n).skip(1) {
            if !ancestors.insert(a) || a == root_node {
                break;
            }
        }
    }
    if claimed.contains(&root_node) || snap.tree.ancestors(root_node).skip(1).any(|a| claimed.contains(&a)) {
        return vec![];
    }
    let mut out = Vec::new();
    let mut stack = vec![root_node];
    while let Some(n) = stack.pop() {
        for c in snap.tree.children(n) {
            if claimed.contains(&c) {
                continue;
            }
            if ancestors.contains(&c) {
                stack.push(c);
            } else {
                out.push(snap.tree.path(c));
            }
        }
    }
    out.sort();
    out
}

/// Mark entities from index `from` on whose paths include directories this
/// user couldn't read; their measured size is then a lower bound. Returns how many.
pub fn flag_incomplete(snap: &mut Snapshot, from: usize) -> usize {
    use crate::model::tree::flags;
    let mut n = 0;
    for i in from..snap.entities.len() {
        let partial = snap.entities[i]
            .paths
            .iter()
            .any(|p| snap.lookup_static(p).is_some_and(|id| snap.tree.node(id).has(flags::INCOMPLETE | flags::DENIED)));
        if partial {
            snap.entities[i].attrs.push(("incomplete".into(), "some directories unreadable; run with sudo".into()));
            n += 1;
        }
    }
    n
}

/// Every path already claimed by an entity (exact strings).
pub fn claimed_paths(snap: &Snapshot) -> HashSet<String> {
    snap.entities.iter().flat_map(|e| e.paths.iter().cloned()).collect()
}

/// Read `root = "..."` from a containerd TOML config.
pub fn config_root(text: &str) -> Option<String> {
    let v: toml::Value = toml::from_str(text).ok()?;
    v.get("root")?.as_str().map(String::from).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------- provider

struct Endpoint {
    group: String,
    socket: String,
    root: String,
}

fn endpoints(ctx: &Ctx) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let std_root = ctx
        .runner
        .read("/etc/containerd/config.toml")
        .and_then(|t| config_root(&t))
        .unwrap_or_else(|| "/var/lib/containerd".into());
    let std_sock = "/run/containerd/containerd.sock";
    if ctx.exists(std_sock) || ctx.exists(&std_root) {
        out.push(Endpoint { group: "containerd (system)".into(), socket: std_sock.into(), root: std_root });
    }
    let k3s_sock = "/run/k3s/containerd/containerd.sock";
    let k3s_roots =
        [("k3s", "/var/lib/rancher/k3s/agent/containerd"), ("rke2", "/var/lib/rancher/rke2/agent/containerd")];
    let found = k3s_roots.iter().find(|(_, r)| ctx.exists(r));
    if let Some((name, root)) = found {
        out.push(Endpoint { group: format!("{name} containerd"), socket: k3s_sock.into(), root: root.to_string() });
    } else if ctx.exists(k3s_sock) {
        out.push(Endpoint { group: "k3s containerd".into(), socket: k3s_sock.into(), root: k3s_roots[0].1.into() });
    }
    out
}

/// An image as `ctr images ls` (or meta.db) describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtrImage {
    pub refs: Vec<String>,
    pub digest: String,
    pub media_type: String,
    pub size: Option<u64>,
    pub platforms: String,
}

/// Parse `ctr images ls` (REF TYPE DIGEST SIZE PLATFORMS LABELS), grouping refs by digest.
pub fn parse_ctr_images(out: &str) -> Vec<CtrImage> {
    let mut v: Vec<CtrImage> = Vec::new();
    for line in out.lines().skip_while(|l| !l.starts_with("REF")).skip(1) {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() < 5 || !t[2].starts_with("sha256:") {
            continue;
        }
        let size = super::parse_size(&format!("{} {}", t[3], t[4]));
        let platforms = t.get(5).copied().unwrap_or("").to_string();
        match v.iter_mut().find(|i| i.digest == t[2]) {
            Some(i) => i.refs.push(t[0].to_string()),
            None => v.push(CtrImage {
                refs: vec![t[0].to_string()],
                digest: t[2].to_string(),
                media_type: t[1].to_string(),
                size,
                platforms,
            }),
        }
    }
    v
}

/// Images in every namespace via `ctr` (`ctr` runs `ctr -a <socket> <args>`).
pub fn ctr_images(ctr: &dyn Fn(&[&str]) -> Option<String>) -> Option<Vec<(String, CtrImage)>> {
    let mut out = Vec::new();
    for ns in ctr(&["namespaces", "ls", "-q"])?.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(list) = ctr(&["-n", ns, "images", "ls"]) {
            out.extend(parse_ctr_images(&list).into_iter().map(|i| (ns.to_string(), i)));
        }
    }
    Some(out)
}

/// Images per namespace straight from containerd's metadata store (root fallback without ctr).
pub fn meta_db_images(bytes: Vec<u8>) -> Vec<(String, CtrImage)> {
    let Some(db) = bolt::Db::parse(bytes) else { return vec![] };
    let Some(v1) = db.root().bucket(b"v1") else { return vec![] };
    let mut out: Vec<(String, CtrImage)> = Vec::new();
    for (ns, v) in v1.entries() {
        let bolt::Val::Bucket(nsb) = v else { continue };
        let ns = String::from_utf8_lossy(ns).into_owned();
        for (name, iv) in nsb.bucket(b"images").map(|b| b.entries()).unwrap_or_default() {
            let bolt::Val::Bucket(ib) = iv else { continue };
            let Some(t) = ib.bucket(b"target") else { continue };
            let Some(digest) = t.string(b"digest") else { continue };
            let name = String::from_utf8_lossy(name).into_owned();
            match out.iter_mut().find(|(n, i)| *n == ns && i.digest == digest) {
                Some((_, i)) => i.refs.push(name),
                None => out.push((
                    ns.clone(),
                    CtrImage {
                        refs: vec![name],
                        digest,
                        media_type: t.string(b"mediatype").unwrap_or_default(),
                        ..Default::default()
                    },
                )),
            }
        }
    }
    out
}

/// Best display name: a tag rather than a digest or bare config ID.
pub fn display_ref(refs: &[String]) -> String {
    refs.iter()
        .find(|r| !r.starts_with("sha256:") && !r.contains('@'))
        .or_else(|| refs.iter().find(|r| !r.starts_with("sha256:")))
        .or(refs.first())
        .cloned()
        .unwrap_or_default()
}

/// Everything collected for one containerd instance; `build` is pure over it.
pub struct CtrData {
    pub images: Vec<(String, CtrImage)>,
    pub snaps: Option<SnapIndex>,
    pub snapshotter: String,
}

fn base_entity(group: &str, kind: &str, name: impl Into<String>, parent: Option<u32>) -> Entity {
    Entity {
        kind: kind.into(),
        name: name.into(),
        provider: "containerd".into(),
        group: group.into(),
        parent,
        ..Default::default()
    }
}

/// Turn one containerd instance into entities under `root`.
#[allow(clippy::too_many_arguments)]
pub fn build(
    snap: &mut Snapshot,
    group: &str,
    socket: &str,
    root: &str,
    data: &CtrData,
    read: &dyn Fn(&str) -> Option<String>,
    has: &dyn Fn(&str) -> bool,
    outcome: &mut Outcome,
) {
    let taken = claimed_paths(snap);
    let content_root = format!("{root}/io.containerd.content.v1.content");
    let snap_root = format!("{root}/io.containerd.snapshotter.v1.{}", data.snapshotter);
    let share = Some(format!("containerd:{root}"));
    let mut namespaces: Vec<&str> = data.images.iter().map(|(n, _)| n.as_str()).collect();
    if let Some(ix) = &data.snaps {
        namespaces.extend(ix.recs.values().map(|r| r.ns.as_str()));
    }
    namespaces.sort_unstable();
    namespaces.dedup();

    for ns in namespaces {
        let in_use = data.snaps.as_ref().map(|ix| ix.in_use(ns)).unwrap_or_default();
        let mut image_snaps: HashSet<u64> = HashSet::new();
        let imgs: Vec<&CtrImage> = data.images.iter().filter(|(n, _)| n == ns).map(|(_, i)| i).collect();

        let mut parent = None;
        for img in &imgs {
            let refs = walk_content(&content_root, &img.digest, &img.media_type, read);
            let scanned = snap.lookup_static(&content_root).is_some();
            let mut paths: Vec<String> = refs
                .blobs
                .iter()
                .filter_map(|d| blob_path(&content_root, d))
                .filter(|p| !scanned || snap.lookup_static(p).is_some())
                .collect();
            let mut used = false;
            let mut config_id = None;
            for diffs in &refs.diff_ids {
                let chain = chain_ids(diffs);
                let Some(ix) = &data.snaps else { break };
                for c in &chain {
                    if let Some(r) = ix.get(ns, c) {
                        image_snaps.insert(r.id);
                        paths.push(format!("{snap_root}/snapshots/{}", r.id));
                    }
                }
                used |= chain.last().is_some_and(|top| in_use.contains(top));
            }
            if let Some(cfg) = refs.blobs.iter().find(|b| img.refs.iter().any(|r| r == *b)) {
                config_id = Some(cfg.clone());
            }
            let all = paths.len();
            paths.retain(|p| !taken.contains(p));
            if all > 0 && paths.is_empty() {
                // Already owned by another provider (rootful Docker's "moby" namespace).
                continue;
            }
            let parent = *parent.get_or_insert_with(|| {
                snap.add_entity(base_entity(group, "containerd.images", format!("Images ({ns})"), None))
            });
            let name = display_ref(&img.refs);
            let mut e = base_entity(group, "containerd.image", name.clone(), Some(parent));
            e.paths = paths;
            e.share_key = share.clone();
            e.reported = img.size;
            e.attrs = vec![
                ("namespace".into(), ns.into()),
                ("digest".into(), img.digest.clone()),
                ("refs".into(), img.refs.join(", ")),
                ("in use".into(), if used { "yes" } else { "no" }.into()),
            ];
            if !img.platforms.is_empty() {
                e.attrs.push(("platforms".into(), img.platforms.clone()));
            }
            if !used && data.snaps.is_some() {
                let id = config_id.unwrap_or_else(|| name.clone());
                let (argv, how) = if ns == "k8s.io" && has("crictl") {
                    (
                        vec![
                            "crictl".into(),
                            "--runtime-endpoint".into(),
                            format!("unix://{socket}"),
                            "rmi".into(),
                            id,
                        ],
                        "crictl rmi",
                    )
                } else if ns == "k8s.io" && has("k3s") {
                    (vec!["k3s".into(), "crictl".into(), "rmi".into(), id], "k3s crictl rmi")
                } else {
                    let mut a: Vec<String> = vec![
                        "ctr".into(),
                        "-a".into(),
                        socket.into(),
                        "-n".into(),
                        ns.into(),
                        "images".into(),
                        "rm".into(),
                    ];
                    a.extend(img.refs.iter().cloned());
                    (a, "ctr images rm")
                };
                e.reclaim = Some(Reclaim {
                    risk: Risk::Review,
                    reason: format!(
                        "no container uses this image; it is re-pulled on demand (`{how}`; `crictl rmi --prune` removes all unused images)"
                    ),
                    estimate: None,
                    action: Some(ActionSpec {
                        label: format!("remove image {name}"),
                        steps: vec![ActionStep::Command { argv, root: true }],
                    }),
                });
            }
            snap.add_entity(e);
        }

        // Container rw layers and views: everything active in this namespace.
        if let Some(ix) = &data.snaps {
            let mut paths: Vec<String> = ix
                .in_ns(ns)
                .filter(|r| matches!(r.kind, KIND_ACTIVE | KIND_VIEW) && !image_snaps.contains(&r.id))
                .map(|r| format!("{snap_root}/snapshots/{}", r.id))
                .filter(|p| !taken.contains(p))
                .collect();
            paths.sort();
            if !paths.is_empty() {
                let mut e = base_entity(group, "containerd.containers", format!("Container layers ({ns})"), None);
                e.attrs.push(("snapshots".into(), paths.len().to_string()));
                e.paths = paths;
                snap.add_entity(e);
            }
        }
    }

    if data.snaps.is_none() {
        let p = format!("{snap_root}/snapshots");
        if !taken.contains(&p) {
            let mut e = base_entity(group, "containerd.layers", "Image layers (all images)", None);
            e.paths = vec![p];
            snap.add_entity(e);
        }
    }

    // Whatever is left under the root: metadata, unreferenced blobs and snapshots.
    let rest = unclaimed_under(snap, root);
    if !rest.is_empty() {
        let mut e = base_entity(group, "containerd.other", "Other (metadata, unreferenced content)", None);
        e.paths = rest;
        snap.add_entity(e);
    } else if snap.lookup_static(root).is_none() {
        outcome.degrade(format!("{root} is not in the scanned tree; sizes come from containerd only"));
    }
}

impl Provider for Containerd {
    fn name(&self) -> &'static str {
        "containerd"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let eps = endpoints(ctx);
        if eps.is_empty() {
            return Outcome::absent();
        }
        let mut outcome = Outcome::complete();
        for ep in eps {
            if !ctx.is_root {
                // /var/lib/containerd and the socket are root-only: claim the tree as one block.
                let taken = claimed_paths(snap);
                if !taken.contains(&ep.root) {
                    let mut e =
                        base_entity(&ep.group, "containerd.data", "containerd data (run as root for detail)", None);
                    e.paths = vec![ep.root.clone()];
                    snap.add_entity(e);
                }
                outcome.degrade(format!("{}: needs root to read {} and {}", ep.group, ep.root, ep.socket));
                continue;
            }
            let data = gather(ctx, &ep, &mut outcome);
            let read = |p: &str| ctx.runner.read(p);
            let has = |p: &str| ctx.runner.has(p);
            build(snap, &ep.group, &ep.socket, &ep.root, &data, &read, &has, &mut outcome);
        }
        outcome
    }
}

fn gather(ctx: &Ctx, ep: &Endpoint, outcome: &mut Outcome) -> CtrData {
    let snapshotter = ["overlayfs", "native", "fuse-overlayfs", "stargz"]
        .into_iter()
        .find(|s| ctx.exists(&format!("{}/io.containerd.snapshotter.v1.{s}", ep.root)))
        .unwrap_or("overlayfs")
        .to_string();
    let snaps = SnapIndex::load(&format!("{}/io.containerd.snapshotter.v1.{snapshotter}/metadata.db", ep.root));
    if snaps.is_none() {
        outcome.degrade(format!("{}: could not read snapshot metadata; layers shown as one block", ep.group));
    }
    let ctr = |args: &[&str]| -> Option<String> {
        let mut argv = vec!["ctr", "-a", ep.socket.as_str()];
        argv.extend_from_slice(args);
        ctx.runner.run(&argv).filter(|o| o.ok()).map(|o| o.stdout)
    };
    let images = ctr_images(&ctr).unwrap_or_else(|| {
        let meta = format!("{}/io.containerd.metadata.v1.bolt/meta.db", ep.root);
        std::fs::read(&meta).map(meta_db_images).unwrap_or_else(|e| {
            outcome.degrade(format!("{}: ctr unavailable and {meta}: {e}", ep.group));
            vec![]
        })
    });
    CtrData { images, snaps, snapshotter }
}

#[cfg(test)]
pub(crate) mod testtree {
    //! Build a synthetic scanned tree from (path, size) pairs.
    use crate::model::tree::{FNode, Kind};
    use crate::model::{FileTree, FsInfo, NONE, Snapshot};
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct T {
        kids: BTreeMap<String, T>,
        size: Option<u64>,
    }

    /// Paths ending in `/` are empty directories; everything else is a file of `size` bytes.
    pub fn snapshot(files: &[(&str, u64)]) -> Snapshot {
        let mut root = T::default();
        for (p, size) in files {
            let mut cur = &mut root;
            let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
            for c in &comps {
                cur = cur.kids.entry(c.to_string()).or_default();
            }
            if !p.ends_with('/') {
                cur.size = Some(*size);
            }
        }
        let mut tree = FileTree::default();
        let mk = |tree: &mut FileTree, name: &str, parent: u32, t: &T| {
            let (off, len) = tree.push_name(name.as_bytes());
            let s = t.size.unwrap_or(0);
            tree.nodes.push(FNode {
                name_off: off,
                name_len: len,
                kind: if t.size.is_some() { Kind::File } else { Kind::Dir },
                flags: 0,
                parent,
                child_start: 0,
                child_count: 0,
                apparent: s,
                alloc: s,
                items: 1,
                mtime: 0,
            });
        };
        mk(&mut tree, "/", NONE, &root);
        let mut queue: std::collections::VecDeque<(u32, &T)> = [(0u32, &root)].into();
        while let Some((id, t)) = queue.pop_front() {
            let start = tree.nodes.len() as u32;
            for (name, k) in &t.kids {
                let cid = tree.nodes.len() as u32;
                mk(&mut tree, name, id, k);
                queue.push_back((cid, k));
            }
            let n = &mut tree.nodes[id as usize];
            n.child_start = start;
            n.child_count = t.kids.len() as u32;
        }
        for i in (1..tree.nodes.len()).rev() {
            let (a, b, c, p) = {
                let n = &tree.nodes[i];
                (n.alloc, n.apparent, n.items, n.parent)
            };
            let pn = &mut tree.nodes[p as usize];
            pn.alloc += a;
            pn.apparent += b;
            pn.items += c;
        }
        tree.roots.push(0);
        Snapshot {
            tree,
            filesystems: vec![FsInfo {
                mount_point: "/".into(),
                scan_root: "/".into(),
                root_node: Some(0),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::runner::FakeRunner;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/containerd/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let long = vec![b'a'; 1000];
        assert_eq!(sha256_hex(&long), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
    }

    #[test]
    fn chain_id_matches_oci_spec() {
        // Example from the OCI image spec (config.md, "Layer ChainID").
        let diffs = vec!["sha256:a".to_string(), "sha256:b".to_string()];
        let c = chain_ids(&diffs);
        assert_eq!(c[0], "sha256:a");
        assert_eq!(c[1], format!("sha256:{}", sha256_hex(b"sha256:a sha256:b")));
    }

    #[test]
    fn varints() {
        assert_eq!(bolt::uvarint(&[0x9f, 0x01]), Some(159));
        assert_eq!(bolt::varint(&[0x80, 0x80, 0xc6, 0x53]), Some(87605248));
        assert_eq!(bolt::uvarint(&[0x80]), None);
    }

    #[test]
    fn reads_snapshotter_bolt() {
        let bytes = std::fs::read(format!("{}/tests/fixtures/containerd/k3s-snapshots.db", env!("CARGO_MANIFEST_DIR")))
            .unwrap();
        let ix = SnapIndex::from_bolt(bytes).unwrap();
        let r = ix.get("k8s.io", "sha256:1111111111111111111111111111111111111111111111111111111111111111").unwrap();
        assert_eq!(r.id, 1);
        assert_eq!(r.kind, KIND_COMMITTED);
        assert_eq!(r.size, Some(7_340_032));
        let rw = ix.in_ns("k8s.io").find(|r| r.kind == KIND_ACTIVE).unwrap();
        assert!(rw.parent.is_some());
        assert!(ix.recs.len() >= 5);
        assert!(SnapIndex::from_bolt(b"not a bolt file".to_vec()).is_none());
    }

    #[test]
    fn reads_meta_db_images() {
        let bytes =
            std::fs::read(format!("{}/tests/fixtures/containerd/k3s-meta.db", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let imgs = meta_db_images(bytes);
        assert_eq!(imgs.len(), 2);
        let (ns, pause) = imgs.iter().find(|(_, i)| i.refs.len() == 2).unwrap();
        assert_eq!(ns, "k8s.io");
        assert_eq!(display_ref(&pause.refs), "registry.k8s.io/pause:3.6");
        assert!(pause.media_type.contains("index"));
    }

    #[test]
    fn parses_ctr_images() {
        let imgs = parse_ctr_images(&fixture("ctr-images-k8s.txt"));
        assert_eq!(imgs.len(), 3);
        let pause = imgs.iter().find(|i| i.refs.iter().any(|r| r.contains("pause"))).unwrap();
        assert_eq!(pause.refs.len(), 3);
        assert_eq!(display_ref(&pause.refs), "registry.k8s.io/pause:3.6");
        assert_eq!(pause.size, Some((301.2 * 1024.0) as u64));
    }

    const K3S_ROOT: &str = "/var/lib/rancher/k3s/agent/containerd";

    /// A k3s node's containerd root as a scanned tree, plus what `gather` would collect.
    fn k3s_setup() -> (Snapshot, CtrData, FakeRunner) {
        let root = K3S_ROOT;
        let snap_bytes =
            std::fs::read(format!("{}/tests/fixtures/containerd/k3s-snapshots.db", env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        let ix = SnapIndex::from_bolt(snap_bytes).unwrap();
        let content: serde_json::Map<String, Value> = serde_json::from_str(&fixture("k3s-content.json")).unwrap();
        let cr = format!("{root}/io.containerd.content.v1.content");
        let mut runner = FakeRunner::default()
            .with("ctr -a /run/k3s/containerd/containerd.sock namespaces ls -q", "k8s.io\n")
            .with("ctr -a /run/k3s/containerd/containerd.sock -n k8s.io images ls", &fixture("ctr-images-k8s.txt"));
        for (d, v) in &content {
            runner = runner.file(&blob_path(&cr, d).unwrap(), v.as_str().unwrap());
        }
        runner.programs.push("crictl".into());

        let mut files: Vec<(String, u64)> = ix
            .recs
            .values()
            .map(|r| (format!("{root}/io.containerd.snapshotter.v1.overlayfs/snapshots/{}/fs/f", r.id), 1 << 20))
            .collect();
        files.push((format!("{root}/io.containerd.snapshotter.v1.overlayfs/snapshots/99/fs/orphan"), 5 << 20));
        files.push((format!("{root}/io.containerd.snapshotter.v1.overlayfs/metadata.db"), 32768));
        for d in content.keys() {
            files.push((blob_path(&cr, d).unwrap(), 2048));
        }
        let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        let snap = testtree::snapshot(&refs);
        let ctr = |args: &[&str]| {
            let mut argv = vec!["ctr", "-a", "/run/k3s/containerd/containerd.sock"];
            argv.extend_from_slice(args);
            crate::providers::runner::CommandRunner::run(&runner, &argv).filter(|o| o.ok()).map(|o| o.stdout)
        };
        let images = ctr_images(&ctr).unwrap();
        (snap, CtrData { images, snaps: Some(ix), snapshotter: "overlayfs".into() }, runner)
    }

    fn run_build(snap: &mut Snapshot, data: &CtrData, runner: &FakeRunner) {
        let read = |p: &str| crate::providers::runner::CommandRunner::read(runner, p);
        let has = |p: &str| crate::providers::runner::CommandRunner::has(runner, p);
        let mut out = Outcome::complete();
        build(snap, "k3s containerd", "/run/k3s/containerd/containerd.sock", K3S_ROOT, data, &read, &has, &mut out);
        crate::model::attribution::attribute(snap);
    }

    /// k3s as root: images mapped to snapshot dirs and blobs, rw layers, leftovers.
    #[test]
    fn builds_k3s_entities() {
        let root = K3S_ROOT;
        let (mut snap, data, runner) = k3s_setup();
        run_build(&mut snap, &data, &runner);
        let by_name = |n: &str| snap.entities.iter().find(|e| e.name == n).unwrap();
        let pause = by_name("registry.k8s.io/pause:3.6");
        let coredns = by_name("docker.io/rancher/mirrored-coredns-coredns:1.10.1");
        let unused = by_name("docker.io/library/busybox:latest");
        // Shared base layer (snapshot 1) splits between pause and busybox.
        assert!(pause.paths.iter().any(|p| p.ends_with("/snapshots/1")));
        assert!(unused.paths.iter().any(|p| p.ends_with("/snapshots/1")));
        assert!(pause.measured_alloc > pause.measured_unique);
        assert_eq!(coredns.attr("in use"), Some("yes"));
        assert!(coredns.reclaim.is_none());
        let r = unused.reclaim.as_ref().unwrap();
        assert_eq!(r.risk, Risk::Review);
        let ActionStep::Command { argv, root: as_root } = &r.action.as_ref().unwrap().steps[0] else { panic!() };
        assert!(*as_root && argv[0] == "crictl" && argv.last().unwrap().starts_with("sha256:"));
        let rw = by_name("Container layers (k8s.io)");
        assert!(rw.measured_alloc >= 1 << 20);
        let other = by_name("Other (metadata, unreferenced content)");
        assert!(other.paths.iter().any(|p| p.ends_with("/snapshots/99")));
        assert!(other.paths.iter().any(|p| p.ends_with("metadata.db")));
        // Every byte under the root is owned exactly once at the top level.
        let groups = crate::views::workloads(&snap);
        let total = snap.tree.node(snap.lookup(root).unwrap()).alloc;
        assert_eq!(groups[0].total, total);
    }

    /// Paths another provider (rootful Docker's "moby" namespace) already owns are skipped.
    #[test]
    fn skips_images_owned_elsewhere() {
        let (mut snap, data, runner) = k3s_setup();
        let snaps = format!("{K3S_ROOT}/io.containerd.snapshotter.v1.overlayfs/snapshots");
        let blobs = format!("{K3S_ROOT}/io.containerd.content.v1.content/blobs/sha256");
        let mut owned: Vec<String> = vec![format!("{snaps}/1"), format!("{snaps}/4")];
        owned.extend(["d1", "d2", "a1", "a4"].iter().map(|h| format!("{blobs}/{}", h.repeat(32))));
        snap.add_entity(Entity { name: "docker".into(), paths: owned, ..Default::default() });
        run_build(&mut snap, &data, &runner);
        assert!(!snap.entities.iter().any(|e| e.name == "docker.io/library/busybox:latest"));
        let pause = snap.entities.iter().find(|e| e.name == "registry.k8s.io/pause:3.6").unwrap();
        assert!(!pause.paths.iter().any(|p| p.ends_with("/snapshots/1")));
    }

    #[test]
    fn unclaimed_paths_are_maximal() {
        let mut snap = testtree::snapshot(&[("/r/a/x", 10), ("/r/a/y", 20), ("/r/b/z", 5), ("/r/c", 1)]);
        snap.add_entity(Entity { paths: vec!["/r/a/x".into()], ..Default::default() });
        assert_eq!(unclaimed_under(&snap, "/r"), vec!["/r/a/y", "/r/b", "/r/c"]);
        assert!(unclaimed_under(&snap, "/nope").is_empty());
    }

    #[test]
    fn reads_config_root() {
        assert_eq!(config_root("version = 2\nroot = \"/data/containerd\"\n").as_deref(), Some("/data/containerd"));
        assert_eq!(config_root("version = 2\n"), None);
    }
}
