//! LVM volume groups / logical volumes. Exact via lvm2 tools as root,
//! otherwise inferred from device-mapper sysfs + lsblk (free space estimated).

use super::{Ctx, Outcome, Provider};
use crate::model::{BlockDev, Lv, LvmInfo, Pv, Snapshot, Vg};
use serde_json::Value;

pub struct Lvm;

fn num(v: &Value, k: &str) -> Option<u64> {
    let x = v.get(k)?;
    x.as_u64().or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok().map(|f| f as u64)))
}

fn st(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn report_rows<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    v.get("report")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| r.get(key).and_then(|x| x.as_array()))
        .flatten()
        .collect()
}

pub fn parse_lvm2(vgs: &str, lvs: &str, pvs: &str) -> Option<LvmInfo> {
    let vgs: Value = serde_json::from_str(vgs).ok()?;
    let lvs: Value = serde_json::from_str(lvs).ok()?;
    let pvs: Value = serde_json::from_str(pvs).ok()?;
    Some(LvmInfo {
        source: "lvm2".into(),
        vgs: report_rows(&vgs, "vg")
            .into_iter()
            .map(|r| Vg {
                name: st(r, "vg_name").unwrap_or_default(),
                size: num(r, "vg_size").unwrap_or(0),
                free: num(r, "vg_free").unwrap_or(0),
                free_is_estimate: false,
                extent_size: num(r, "vg_extent_size"),
            })
            .collect(),
        lvs: report_rows(&lvs, "lv")
            .into_iter()
            .map(|r| Lv {
                vg: st(r, "vg_name").unwrap_or_default(),
                name: st(r, "lv_name").unwrap_or_default(),
                size: num(r, "lv_size").unwrap_or(0),
                attr: st(r, "lv_attr"),
                segtype: st(r, "segtype"),
                pool: st(r, "pool_lv"),
                origin: st(r, "origin"),
                data_percent: st(r, "data_percent").and_then(|s| s.parse().ok()),
                kname: st(r, "lv_dm_path"),
                ..Default::default()
            })
            .fold(Vec::<Lv>::new(), |mut lvs, lv| {
                // `segtype` is a segment field, so lvs emits one row per segment; keep the first.
                if !lvs.iter().any(|l| l.vg == lv.vg && l.name == lv.name) {
                    lvs.push(lv);
                }
                lvs
            }),
        pvs: report_rows(&pvs, "pv")
            .into_iter()
            .map(|r| Pv {
                name: st(r, "pv_name").unwrap_or_default(),
                vg: st(r, "vg_name").unwrap_or_default(),
                size: num(r, "pv_size").unwrap_or(0),
                free: num(r, "pv_free"),
            })
            .collect(),
    })
}

/// Split a device-mapper name `vg-lv` (dashes inside names are doubled).
pub fn split_dm_name(name: &str) -> Option<(String, String)> {
    let b = name.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' {
            if b.get(i + 1) == Some(&b'-') {
                i += 2;
                continue;
            }
            return Some((name[..i].replace("--", "-"), name[i + 1..].replace("--", "-")));
        }
        i += 1;
    }
    None
}

fn is_internal(lv: &str) -> bool {
    lv.ends_with("-real")
        || lv.ends_with("-cow")
        || lv.ends_with("_tdata")
        || lv.ends_with("_tmeta")
        || lv.ends_with("-tpool")
        || lv.contains("_rimage_")
        || lv.contains("_rmeta_")
        || lv.contains("_mlog")
        || lv.contains("_corig")
        || lv.contains("_cdata")
        || lv.contains("_cmeta")
}

/// Infer LVM layout from block devices: dm devices whose uuid starts with LVM-.
pub fn infer_from_block(block: &[BlockDev], dm_uuid: impl Fn(&str) -> Option<String>) -> LvmInfo {
    let mut info = LvmInfo { source: "sysfs".into(), ..Default::default() };
    let is_lvm = |d: &BlockDev| d.dtype == "lvm" || dm_uuid(&d.kname).is_some_and(|u| u.starts_with("LVM-"));
    let lv_devs: Vec<&BlockDev> = block.iter().filter(|d| is_lvm(d)).collect();
    for pv in block.iter().filter(|d| d.fstype.as_deref() == Some("LVM2_member")) {
        // LVs allocated directly on this PV.
        let on_pv: Vec<&&BlockDev> = lv_devs.iter().filter(|l| l.parents.iter().any(|p| p == &pv.kname)).collect();
        let vg = on_pv.iter().find_map(|l| split_dm_name(&l.name).map(|(vg, _)| vg)).unwrap_or_default();
        let used: u64 = on_pv.iter().map(|l| l.size).sum();
        // lvm2 reserves 1 MiB of metadata at the start of each PV.
        let usable = pv.size.saturating_sub(1 << 20);
        info.pvs.push(Pv {
            name: pv.path.clone(),
            vg: vg.clone(),
            size: pv.size,
            free: Some(usable.saturating_sub(used)),
        });
    }
    for pv in &info.pvs {
        if pv.vg.is_empty() {
            continue;
        }
        match info.vgs.iter_mut().find(|v| v.name == pv.vg) {
            Some(v) => {
                v.size += pv.size;
                v.free += pv.free.unwrap_or(0);
            }
            None => info.vgs.push(Vg {
                name: pv.vg.clone(),
                size: pv.size,
                free: pv.free.unwrap_or(0),
                free_is_estimate: true,
                extent_size: None,
            }),
        }
    }
    for d in lv_devs {
        let Some((vg, lv)) = split_dm_name(&d.name) else { continue };
        if is_internal(&lv) {
            continue;
        }
        info.lvs.push(Lv {
            vg,
            name: lv,
            size: d.size,
            kname: Some(d.kname.clone()),
            mountpoints: d.mountpoints.clone(),
            fstype: d.fstype.clone(),
            ..Default::default()
        });
    }
    info
}

impl Provider for Lvm {
    fn name(&self) -> &'static str {
        "lvm"
    }

    fn collect(&self, ctx: &Ctx, snap: &mut Snapshot) -> Outcome {
        let has_lvm = snap.block.iter().any(|d| d.fstype.as_deref() == Some("LVM2_member") || d.dtype == "lvm");
        if !has_lvm {
            return Outcome::absent();
        }
        let common = ["--reportformat", "json", "--units", "b", "--nosuffix"];
        let run = |cmd: &str, fields: &str| -> Option<String> {
            let mut argv = vec![cmd];
            argv.extend(common);
            argv.extend(["-o", fields]);
            ctx.runner.run(&argv).filter(|o| o.ok()).map(|o| o.stdout)
        };
        let exact = if ctx.is_root {
            match (
                run("vgs", "vg_name,vg_size,vg_free,vg_extent_size"),
                run("lvs", "lv_name,vg_name,lv_size,lv_attr,segtype,pool_lv,origin,data_percent,lv_dm_path"),
                run("pvs", "pv_name,vg_name,pv_size,pv_free"),
            ) {
                (Some(v), Some(l), Some(p)) => parse_lvm2(&v, &l, &p),
                _ => None,
            }
        } else {
            None
        };
        let mut outcome = Outcome::complete();
        let mut info = match exact {
            Some(mut info) => {
                // Map /dev/mapper paths to kernel names and mount info.
                for lv in info.lvs.iter_mut() {
                    let kname = lv
                        .kname
                        .as_deref()
                        .and_then(|p| std::fs::canonicalize(p).ok())
                        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()));
                    if let Some(d) = kname.as_deref().and_then(|k| snap.block.iter().find(|d| d.kname == k)) {
                        lv.mountpoints = d.mountpoints.clone();
                        lv.fstype = d.fstype.clone();
                    }
                    lv.kname = kname;
                }
                info
            }
            None => {
                outcome.degrade(
                    "not root: VG free space estimated from device-mapper tables (run with sudo for exact lvm2 data)",
                );
                infer_from_block(&snap.block, |k| {
                    ctx.runner.read(&format!("/sys/block/{k}/dm/uuid")).map(|s| s.trim().to_string())
                })
            }
        };
        info.lvs.sort_by(|a, b| (&a.vg, &a.name).cmp(&(&b.vg, &b.name)));

        // Annotate block devices: unused LVs are interesting.
        for lv in &info.lvs {
            let Some(k) = &lv.kname else { continue };
            if let Some(d) = snap.block.iter_mut().find(|d| &d.kname == k)
                && lv.segtype.as_deref() == Some("thin-pool")
            {
                d.used_by.push(format!("thin pool ({:.0}% data used)", lv.data_percent.unwrap_or(0.0)));
            }
        }
        snap.lvm = info;
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_names() {
        assert_eq!(split_dm_name("vg_system-lv_root"), Some(("vg_system".into(), "lv_root".into())));
        assert_eq!(split_dm_name("my--vg-my--lv"), Some(("my-vg".into(), "my-lv".into())));
        assert_eq!(split_dm_name("nodash"), None);
    }

    #[test]
    fn infers_vg_free() {
        let block = crate::providers::block::parse_lsblk(include_str!("../../tests/fixtures/lsblk.json"));
        let info = infer_from_block(&block, |_| None);
        assert_eq!(info.vgs.len(), 1);
        let vg = &info.vgs[0];
        assert_eq!(vg.name, "vg_system");
        let lvs: u64 = info.lvs.iter().map(|l| l.size).sum();
        assert_eq!(vg.free, vg.size - (1 << 20) - lvs);
        assert!(info.lvs.iter().any(|l| l.name == "lv_vm_test" && l.mountpoints.is_empty()));
    }

    #[test]
    fn parses_lvm2_json() {
        let vgs =
            r#"{"report":[{"vg":[{"vg_name":"vg0","vg_size":"1000","vg_free":"200","vg_extent_size":"4194304"}]}]}"#;
        let lvs = r#"{"report":[{"lv":[{"lv_name":"pool","vg_name":"vg0","lv_size":"500","lv_attr":"twi-aotz--","segtype":"thin-pool","pool_lv":"","origin":"","data_percent":"42.50","lv_dm_path":"/dev/mapper/vg0-pool"}]}]}"#;
        let pvs = r#"{"report":[{"pv":[{"pv_name":"/dev/sda2","vg_name":"vg0","pv_size":"1000","pv_free":"200"}]}]}"#;
        let info = parse_lvm2(vgs, lvs, pvs).unwrap();
        assert_eq!(info.vgs[0].free, 200);
        assert_eq!(info.lvs[0].data_percent, Some(42.5));
        assert_eq!(info.pvs[0].free, Some(200));
    }

    #[test]
    fn merges_multi_segment_lvs() {
        let vgs = r#"{"report":[{"vg":[{"vg_name":"vg0","vg_size":"1000","vg_free":"200"}]}]}"#;
        let seg = |lv: &str| {
            format!(r#"{{"lv_name":"{lv}","vg_name":"vg0","lv_size":"300","lv_attr":"-wi-ao----","segtype":"linear"}}"#)
        };
        let lvs =
            format!(r#"{{"report":[{{"lv":[{},{},{},{}]}}]}}"#, seg("home"), seg("home"), seg("home"), seg("root"));
        let pvs = r#"{"report":[{"pv":[]}]}"#;
        let info = parse_lvm2(vgs, &lvs, pvs).unwrap();
        let names: Vec<&str> = info.lvs.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["home", "root"]);
        assert_eq!(info.lvs[0].size, 300);
    }
}
