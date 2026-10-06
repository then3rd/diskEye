//! Export a subtree in ncdu's JSON dump format (`ncdu -f file.json`).
//! Mountpoints are stitched to the scanned filesystem mounted there.

use super::tree::{Kind, flags};
use super::{NodeId, Snapshot};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;

pub fn export(snap: &Snapshot, root: NodeId, stitch: bool, out: &mut impl Write) -> anyhow::Result<()> {
    let mounts: HashMap<String, NodeId> =
        snap.filesystems.iter().filter_map(|f| Some((f.scan_root.clone(), f.root_node?))).collect();
    let path = snap.tree.path(root);
    let body = dir_value(snap, root, &path, &path, &mounts, stitch);
    let doc = json!([1, 2, {
        "progname": "diskeye",
        "progver": env!("CARGO_PKG_VERSION"),
        "timestamp": snap.meta.started,
    }, body]);
    serde_json::to_writer(&mut *out, &doc)?;
    out.write_all(b"\n")?;
    Ok(())
}

fn dir_value(
    snap: &Snapshot,
    id: NodeId,
    name: &str,
    path: &str,
    mounts: &HashMap<String, NodeId>,
    stitch: bool,
) -> Value {
    let t = &snap.tree;
    let n = t.node(id);
    let (mut ca, mut cd) = (0u64, 0u64);
    for c in t.children(id) {
        let cn = t.node(c);
        if !cn.has(flags::HARDLINK_DUP) {
            ca += cn.apparent;
            cd += cn.alloc;
        }
    }
    let mut info = json!({
        "name": name,
        "asize": n.apparent.saturating_sub(ca),
        "dsize": n.alloc.saturating_sub(cd),
    });
    if n.has(flags::DENIED) {
        info["read_error"] = json!(true);
    }
    let mut arr = vec![info];
    for c in t.children(id) {
        let cn = t.node(c);
        let cname = t.name(c).into_owned();
        let cpath = if path.ends_with('/') { format!("{path}{cname}") } else { format!("{path}/{cname}") };
        if cn.has(flags::MOUNTPOINT) {
            match mounts.get(&cpath).filter(|_| stitch) {
                Some(&root) => arr.push(dir_value(snap, root, &cname, &cpath, mounts, stitch)),
                None => arr.push(json!({"name": cname, "excluded": "othfs"})),
            }
        } else if cn.kind == Kind::Dir {
            arr.push(dir_value(snap, c, &cname, &cpath, mounts, stitch));
        } else {
            let dup = cn.has(flags::HARDLINK_DUP);
            let mut v = json!({
                "name": cname,
                "asize": if dup { 0 } else { cn.apparent },
                "dsize": if dup { 0 } else { cn.alloc },
            });
            if cn.kind != Kind::File {
                v["notreg"] = json!(true);
            }
            arr.push(v);
        }
    }
    Value::Array(arr)
}
