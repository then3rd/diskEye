//! Kubernetes node data: pod directories under the kubelet root (emptyDirs,
//! projected volumes...), pod logs, and k3s local-path persistent volumes.
//!
//! Directory names alone identify most of it — `/var/log/pods/<ns>_<pod>_<uid>`
//! and `/var/lib/rancher/k3s/storage/pvc-<uid>_<ns>_<pvc>` — so the provider
//! works without API access; `kubectl get pods,pvc,pv -A -o json` adds phases,
//! volume types and released/orphaned PVs for reclaim.

use super::{Ctx, Outcome, Provider};
use crate::model::{ActionSpec, ActionStep, Entity, Reclaim, Risk, Snapshot};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub struct Kube;

pub const KUBELET_PODS: &str = "/var/lib/kubelet/pods";
pub const POD_LOGS: &str = "/var/log/pods";
pub const K3S_STORAGE: &str = "/var/lib/rancher/k3s/storage";
const K3S_YAML: &str = "/etc/rancher/k3s/k3s.yaml";

/// Directory listings of the node-local paths we map.
#[derive(Debug, Default)]
pub struct NodeView {
    /// Children of `/var/lib/kubelet/pods` (pod UIDs).
    pub pod_dirs: Vec<String>,
    /// Children of `/var/log/pods`.
    pub log_dirs: Vec<String>,
    /// Children of the k3s local-path storage dir.
    pub pv_dirs: Vec<String>,
}

/// `<namespace>_<pod>_<uid>` → (ns, pod, uid). Neither names nor UIDs contain `_`.
pub fn parse_log_dir(name: &str) -> Option<(&str, &str, &str)> {
    let mut it = name.splitn(3, '_');
    Some((it.next()?, it.next()?, it.next().filter(|u| !u.is_empty())?))
}

/// `pvc-<uid>_<ns>_<claim>` → (pv name, ns, claim).
pub fn parse_pv_dir(name: &str) -> Option<(&str, &str, &str)> {
    if !name.starts_with("pvc-") {
        return None;
    }
    let mut it = name.splitn(3, '_');
    Some((it.next()?, it.next()?, it.next().filter(|c| !c.is_empty())?))
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(|x| x.as_str()).unwrap_or("")
}

fn items<'a>(api: Option<&'a Value>, kind: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    api.and_then(|v| v.get("items"))
        .and_then(|i| i.as_array())
        .into_iter()
        .flatten()
        .filter(move |i| s(i, "/kind") == kind)
}

/// Short description of a pod volume's type, e.g. `cache (emptyDir)`.
fn volume_desc(v: &Value) -> String {
    let name = s(v, "/name");
    let ty = v
        .as_object()
        .and_then(|o| o.keys().find(|k| *k != "name"))
        .map(|k| match k.as_str() {
            "persistentVolumeClaim" => format!("pvc {}", s(v, "/persistentVolumeClaim/claimName")),
            "hostPath" => format!("hostPath {}", s(v, "/hostPath/path")),
            other => other.to_string(),
        })
        .unwrap_or_default();
    format!("{name} ({ty})")
}

/// Turn the node view (and optional API listing) into entities.
pub fn build(
    snap: &mut Snapshot,
    group: &str,
    node: &NodeView,
    api: Option<&Value>,
    kubectl: &[String],
) -> Vec<String> {
    let mut notes = Vec::new();
    let add = |snap: &mut Snapshot, kind: &str, name: String, parent: Option<u32>, f: &mut dyn FnMut(&mut Entity)| {
        let mut e = Entity {
            kind: kind.into(),
            name,
            provider: "kube".into(),
            group: group.into(),
            parent,
            ..Default::default()
        };
        f(&mut e);
        snap.add_entity(e)
    };
    let as_root = kubectl.iter().any(|a| a == K3S_YAML);
    let kubectl_action = |args: &[&str]| -> Vec<ActionStep> {
        if kubectl.is_empty() {
            return vec![];
        }
        let mut argv = kubectl.to_vec();
        argv.extend(args.iter().map(|a| a.to_string()));
        vec![ActionStep::Command { argv, root: as_root }]
    };

    // ------------------------------------------------ pods
    let api_pods: HashMap<&str, &Value> = items(api, "Pod").map(|p| (s(p, "/metadata/uid"), p)).collect();
    let logs: HashMap<&str, (&str, &str, &str)> = node
        .log_dirs
        .iter()
        .filter_map(|d| parse_log_dir(d).map(|(ns, pod, uid)| (uid, (ns, pod, d.as_str()))))
        .collect();
    let mut uids: Vec<&str> = node.pod_dirs.iter().map(String::as_str).chain(logs.keys().copied()).collect();
    uids.sort_unstable();
    uids.dedup();
    // Only trust an API that knows at least one pod on this node (kubeconfigs often point elsewhere).
    let api_matches = api.is_some() && (uids.is_empty() || uids.iter().any(|u| api_pods.contains_key(u)));
    if api.is_some() && !api_matches {
        notes.push("the kubeconfig's cluster has none of this node's pods; using directory names only".into());
    }
    let pods_id = (!uids.is_empty()).then(|| add(snap, "kube.pods", "Pods".into(), None, &mut |_| {}));
    for uid in uids {
        let pod = api_pods.get(uid).filter(|_| api_matches);
        let log = logs.get(uid);
        let (ns, name) = match (pod, log) {
            (Some(p), _) => (s(p, "/metadata/namespace").to_string(), s(p, "/metadata/name").to_string()),
            (None, Some((ns, n, _))) => (ns.to_string(), n.to_string()),
            (None, None) => (String::new(), format!("pod {}", &uid[..uid.len().min(8)])),
        };
        let mut paths = Vec::new();
        if node.pod_dirs.iter().any(|d| d == uid) {
            paths.push(format!("{KUBELET_PODS}/{uid}"));
        }
        if let Some((_, _, d)) = log {
            paths.push(format!("{POD_LOGS}/{d}"));
        }
        let label = if ns.is_empty() { name.clone() } else { format!("{ns}/{name}") };
        add(snap, "kube.pod", label.clone(), pods_id, &mut |e| {
            e.paths = paths.clone();
            e.attrs.push(("uid".into(), uid.into()));
            if !ns.is_empty() {
                e.attrs.push(("namespace".into(), ns.clone()));
            }
            let Some(p) = pod else {
                if api_matches {
                    e.attrs.push(("status".into(), "not known to the API (orphaned)".into()));
                    e.reclaim = Some(Reclaim {
                        risk: Risk::Review,
                        reason: "pod directory the API no longer knows; kubelet normally removes these — check for \
                                 leftover volume mounts before deleting"
                            .into(),
                        estimate: None,
                        action: None,
                    });
                }
                return;
            };
            let phase = s(p, "/status/phase");
            e.attrs.push(("phase".into(), phase.into()));
            let vols: Vec<String> =
                p.pointer("/spec/volumes").and_then(|v| v.as_array()).into_iter().flatten().map(volume_desc).collect();
            if !vols.is_empty() {
                e.attrs.push(("volumes".into(), vols.join(", ")));
            }
            if matches!(phase, "Succeeded" | "Failed") {
                let steps = kubectl_action(&["delete", "pod", "-n", &ns, &name]);
                e.reclaim = Some(Reclaim {
                    risk: Risk::Review,
                    reason: format!("{phase} pod; its emptyDir volumes and logs are freed when it is deleted"),
                    estimate: None,
                    action: (!steps.is_empty()).then(|| ActionSpec { label: format!("delete pod {label}"), steps }),
                });
            }
        });
    }

    // ------------------------------------------------ persistent volumes
    let pvs: Vec<&Value> = if api_matches { items(api, "PersistentVolume").collect() } else { vec![] };
    let pv_path = |pv: &Value| -> String {
        let h = s(pv, "/spec/hostPath/path");
        if h.is_empty() { s(pv, "/spec/local/path").to_string() } else { h.to_string() }
    };
    let api_paths: HashSet<String> = pvs.iter().map(|pv| pv_path(pv)).filter(|p| !p.is_empty()).collect();
    let local_pvs: Vec<&&Value> = pvs
        .iter()
        .filter(|pv| {
            let p = pv_path(pv);
            !p.is_empty() && (p.starts_with(&format!("{K3S_STORAGE}/")) || snap.lookup_static(&p).is_some())
        })
        .collect();
    let orphan_dirs: Vec<&String> =
        node.pv_dirs.iter().filter(|d| !api_paths.contains(&format!("{K3S_STORAGE}/{d}"))).collect();
    if local_pvs.is_empty() && orphan_dirs.is_empty() {
        return notes;
    }
    let pvs_id = Some(add(snap, "kube.pvs", "Persistent volumes".into(), None, &mut |_| {}));
    for pv in local_pvs {
        let path = pv_path(pv);
        let pv_name = s(pv, "/metadata/name");
        let claim = format!("{}/{}", s(pv, "/spec/claimRef/namespace"), s(pv, "/spec/claimRef/name"));
        let phase = s(pv, "/status/phase");
        let policy = s(pv, "/spec/persistentVolumeReclaimPolicy");
        let label = if claim == "/" { pv_name.to_string() } else { claim.clone() };
        add(snap, "kube.pv", label, pvs_id, &mut |e| {
            e.paths = vec![path.clone()];
            e.attrs.push(("pv".into(), pv_name.into()));
            e.attrs.push(("phase".into(), phase.into()));
            e.attrs.push(("reclaim policy".into(), policy.into()));
            e.attrs.push(("storage class".into(), s(pv, "/spec/storageClassName").into()));
            e.attrs.push(("capacity".into(), s(pv, "/spec/capacity/storage").into()));
            if !matches!(phase, "Released" | "Available") {
                return;
            }
            let mut steps = kubectl_action(&["delete", "pv", pv_name]);
            if path.starts_with(&format!("{K3S_STORAGE}/")) {
                steps.push(ActionStep::DeletePath { path: path.clone(), trash: false });
            }
            e.reclaim = Some(Reclaim {
                risk: Risk::Review,
                reason: format!(
                    "{phase} volume (policy {policy}) — no claim uses it, but it holds DATA; deleting the PV object \
                     alone does not free a Retain volume's directory"
                ),
                estimate: None,
                action: (!steps.is_empty()).then(|| ActionSpec { label: format!("delete PV {pv_name}"), steps }),
            });
        });
    }
    for d in orphan_dirs {
        let path = format!("{K3S_STORAGE}/{d}");
        let (pv, ns, claim) = parse_pv_dir(d).unwrap_or((d.as_str(), "", ""));
        let label = if claim.is_empty() { d.clone() } else { format!("{ns}/{claim}") };
        add(snap, "kube.pv", label, pvs_id, &mut |e| {
            e.paths = vec![path.clone()];
            e.attrs.push(("pv".into(), pv.into()));
            if !api_matches {
                return;
            }
            e.attrs.push(("phase".into(), "no PV object (orphaned directory)".into()));
            e.reclaim = Some(Reclaim {
                risk: Risk::Review,
                reason: "local-path directory whose PersistentVolume no longer exists — leftover DATA".into(),
                estimate: None,
                action: Some(ActionSpec {
                    label: format!("delete {path}"),
                    steps: vec![ActionStep::DeletePath { path: path.clone(), trash: false }],
                }),
            });
        });
    }
    notes
}

/// Kubeconfig to use: `$KUBECONFIG`, the k3s admin config when this is a k3s
/// node, then `~/.kube/config`.
pub fn kubeconfig(env: Option<&str>, home: &str, k3s_node: bool, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let mut cands: Vec<String> = Vec::new();
    if let Some(e) = env.and_then(|e| e.split(':').find(|p| !p.is_empty())) {
        cands.push(e.to_string());
    }
    if k3s_node {
        cands.push(K3S_YAML.into());
    }
    cands.push(format!("{home}/.kube/config"));
    cands.into_iter().find(|c| exists(c))
}

/// Names of a directory's children: from the scanned tree, else the filesystem.
fn children(snap: &Snapshot, dir: &str) -> Vec<String> {
    if let Some(n) = snap.lookup_static(dir) {
        return snap.tree.children(n).map(|c| snap.tree.name(c).into_owned()).collect();
    }
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

impl Provider for Kube {
    fn name(&self) -> &'static str {
        "kube"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let present: Vec<&str> = [KUBELET_PODS, POD_LOGS, K3S_STORAGE].into_iter().filter(|d| ctx.exists(d)).collect();
        if present.is_empty() {
            return Outcome::absent();
        }
        let mut outcome = Outcome::complete();
        let node = NodeView {
            pod_dirs: children(snap, KUBELET_PODS),
            log_dirs: children(snap, POD_LOGS),
            pv_dirs: children(snap, K3S_STORAGE),
        };
        if node.pod_dirs.is_empty() && ctx.exists(KUBELET_PODS) && !ctx.is_root {
            outcome.degrade(format!("{KUBELET_PODS} needs root to read"));
        }
        let k3s = ctx.exists("/var/lib/rancher/k3s");
        let env = std::env::var("KUBECONFIG").ok();
        let cfg = kubeconfig(env.as_deref(), &ctx.home.display().to_string(), k3s, &|p| ctx.exists(p));
        let prefix: Vec<String> = match (&cfg, ctx.runner.has("kubectl"), ctx.runner.has("k3s")) {
            (Some(c), true, _) => vec!["kubectl".into(), "--kubeconfig".into(), c.clone()],
            (Some(c), false, true) => vec!["k3s".into(), "kubectl".into(), "--kubeconfig".into(), c.clone()],
            _ => vec![],
        };
        let api = if prefix.is_empty() {
            outcome.degrade("no kubectl/kubeconfig: pods and volumes named from directory names only");
            None
        } else {
            let mut argv: Vec<&str> = prefix.iter().map(String::as_str).collect();
            argv.extend(["get", "pods,pvc,pv", "-A", "-o", "json"]);
            match ctx.runner.run(&argv).filter(|o| o.ok()).and_then(|o| serde_json::from_str::<Value>(&o.stdout).ok()) {
                Some(v) => Some(v),
                None => {
                    outcome.degrade(format!("`{}` failed; using directory names only", argv.join(" ")));
                    None
                }
            }
        };
        let group = if k3s { "Kubernetes (k3s)" } else { "Kubernetes" };
        for n in build(snap, group, &node, api.as_ref(), if api.is_some() { &prefix } else { &[] }) {
            outcome.degrade(n);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::containerd::testtree;

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/kube/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    const WEB: &str = "5b0e6a1c-3f7d-4b8e-9c2a-7d4e1f0a9b31";
    const COREDNS: &str = "0d4a8d5e-5b8f-4c47-9a5b-1c2d3e4f5a61";
    const JOB: &str = "9a7c2e44-1b3d-4f6a-8e5c-2d1f0b9a8c77";
    const ORPHAN: &str = "e3f1c9a2-7b6d-4c5e-8f1a-0b2c3d4e5f60";

    fn node_tree() -> Snapshot {
        let mb = 1u64 << 20;
        let files = vec![
            (format!("{KUBELET_PODS}/{WEB}/volumes/kubernetes.io~empty-dir/cache/blob"), 120 * mb),
            (format!("{KUBELET_PODS}/{WEB}/etc-hosts"), 4096),
            (format!("{KUBELET_PODS}/{COREDNS}/volumes/kubernetes.io~configmap/config-volume/Corefile"), 4096),
            (format!("{KUBELET_PODS}/{JOB}/volumes/kubernetes.io~empty-dir/scratch/out.csv"), 300 * mb),
            (format!("{KUBELET_PODS}/{ORPHAN}/volumes/kubernetes.io~empty-dir/tmp/x"), 2 * mb),
            (format!("{POD_LOGS}/default_web-0_{WEB}/web/0.log"), 10 * mb),
            (format!("{POD_LOGS}/kube-system_coredns-6799fbcd5-xk2lp_{COREDNS}/coredns/0.log"), mb),
            (format!("{POD_LOGS}/default_report-job-28456120-abcde_{JOB}/report/0.log"), mb),
            (
                format!("{K3S_STORAGE}/pvc-7f3e2b1a-aaaa-4c3d-9e8f-111111111111_default_data-web-0/pgdata/base"),
                800 * mb,
            ),
            (format!("{K3S_STORAGE}/pvc-2c4d6e8f-bbbb-4a1b-8c9d-222222222222_default_old-data/dump.sql"), 50 * mb),
            (format!("{K3S_STORAGE}/pvc-9e8d7c6b-cccc-4f5e-a1b2-333333333333_test_leftover/file"), 5 * mb),
        ];
        let refs: Vec<(&str, u64)> = files.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        testtree::snapshot(&refs)
    }

    fn view(snap: &Snapshot) -> NodeView {
        NodeView {
            pod_dirs: children(snap, KUBELET_PODS),
            log_dirs: children(snap, POD_LOGS),
            pv_dirs: children(snap, K3S_STORAGE),
        }
    }

    #[test]
    fn dir_names() {
        assert_eq!(parse_log_dir(&format!("default_web-0_{WEB}")), Some(("default", "web-0", WEB)));
        assert_eq!(parse_log_dir("nounderscore"), None);
        assert_eq!(
            parse_pv_dir("pvc-7f3e2b1a-aaaa_default_data-web-0"),
            Some(("pvc-7f3e2b1a-aaaa", "default", "data-web-0"))
        );
        assert_eq!(parse_pv_dir("other_dir"), None);
    }

    #[test]
    fn with_api() {
        let mut snap = node_tree();
        let node = view(&snap);
        let api: Value = serde_json::from_str(&fx("get-pods-pvc-pv.json")).unwrap();
        let kubectl: Vec<String> = ["kubectl", "--kubeconfig", K3S_YAML].iter().map(|s| s.to_string()).collect();
        let notes = build(&mut snap, "Kubernetes (k3s)", &node, Some(&api), &kubectl);
        assert!(notes.is_empty(), "{notes:?}");
        crate::model::attribution::attribute(&mut snap);
        let find = |n: &str| snap.entities.iter().find(|e| e.name == n).unwrap_or_else(|| panic!("no {n}"));
        let mb = 1u64 << 20;

        let web = find("default/web-0");
        assert_eq!(web.measured_alloc, 120 * mb + 4096 + 10 * mb);
        assert!(web.attr("volumes").unwrap().contains("cache (emptyDir)"));
        assert!(web.reclaim.is_none());
        let job = find("default/report-job-28456120-abcde");
        let r = job.reclaim.as_ref().unwrap();
        let ActionStep::Command { argv, root } = &r.action.as_ref().unwrap().steps[0] else { panic!() };
        assert!(*root);
        assert_eq!(argv[3..], ["delete", "pod", "-n", "default", "report-job-28456120-abcde"]);
        let orphan = snap.entities.iter().find(|e| e.attr("uid") == Some(ORPHAN)).unwrap();
        assert!(orphan.reclaim.as_ref().unwrap().action.is_none());

        let bound = find("default/data-web-0");
        assert_eq!(bound.measured_alloc, 800 * mb);
        assert!(bound.reclaim.is_none());
        let released = find("default/old-data");
        let steps = &released.reclaim.as_ref().unwrap().action.as_ref().unwrap().steps;
        assert_eq!(steps.len(), 2);
        assert!(
            matches!(&steps[1], ActionStep::DeletePath { path, trash: false } if path.ends_with("_default_old-data"))
        );
        let leftover = find("test/leftover");
        assert!(leftover.reclaim.is_some());
        let groups = crate::views::workloads(&snap);
        assert_eq!(groups[0].total, snap.tree.node(0).alloc);
    }

    #[test]
    fn without_api() {
        let mut snap = node_tree();
        let node = view(&snap);
        build(&mut snap, "Kubernetes (k3s)", &node, None, &[]);
        crate::model::attribution::attribute(&mut snap);
        assert!(snap.entities.iter().any(|e| e.name == "kube-system/coredns-6799fbcd5-xk2lp"));
        assert!(snap.entities.iter().any(|e| e.name == "default/old-data" && e.reclaim.is_none()));
        assert!(snap.entities.iter().any(|e| e.name == "pod e3f1c9a2"));
        assert!(snap.entities.iter().all(|e| e.reclaim.is_none()));
    }

    #[test]
    fn foreign_cluster_is_ignored() {
        let mut snap = node_tree();
        let node = view(&snap);
        let api: Value = serde_json::json!({"kind": "List", "items": [
            {"kind": "Pod", "metadata": {"name": "x", "namespace": "y", "uid": "11111111-0000-0000-0000-000000000000"},
             "status": {"phase": "Succeeded"}}
        ]});
        let notes = build(&mut snap, "Kubernetes", &node, Some(&api), &["kubectl".into()]);
        assert_eq!(notes.len(), 1);
        assert!(snap.entities.iter().all(|e| e.reclaim.is_none()));
    }

    #[test]
    fn picks_kubeconfig() {
        let all = |_: &str| true;
        assert_eq!(kubeconfig(Some("/a:/b"), "/home/u", true, &all).as_deref(), Some("/a"));
        assert_eq!(kubeconfig(None, "/home/u", true, &all).as_deref(), Some(K3S_YAML));
        assert_eq!(kubeconfig(None, "/home/u", false, &all).as_deref(), Some("/home/u/.kube/config"));
        assert_eq!(kubeconfig(None, "/home/u", false, &|_| false), None);
    }
}
