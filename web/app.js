/* diskeye web UI: routing, API access and the six views. */
(function () {
  "use strict";
  const DK = window.DK;
  const $ = (sel, root) => (root || document).querySelector(sel);
  const esc = DK.esc, size = DK.size;

  // ------------------------------------------------------------ state & routing
  const S = {
    token: "",
    tab: "overview",
    summary: null,
    files: { id: null, metric: "alloc", color: "owner", chart: "treemap", data: null, limit: {}, chartApi: null, busy: 0 },
    cache: new Map(),
    wl: { data: null, open: new Set(), detail: new Map() },
    reclaim: { filter: "all", data: null, sel: new Set() },
    diff: { against: "", threshold: "100M" },
  };
  const TABS = ["overview", "physical", "files", "workloads", "reclaim", "diff"];

  function parseHash() {
    const p = new URLSearchParams(location.hash.replace(/^#/, ""));
    return Object.fromEntries(p.entries());
  }
  function writeHash(params, push) {
    const cur = parseHash();
    const next = { ...cur, ...params };
    Object.keys(next).forEach((k) => (next[k] == null || next[k] === "") && delete next[k]);
    const h = "#" + new URLSearchParams(next).toString();
    if (h === location.hash) return;
    if (push) history.pushState(null, "", h);
    else history.replaceState(null, "", h);
  }

  function initToken() {
    const h = parseHash();
    let t = h.token || "";
    try {
      if (t) sessionStorage.setItem("diskeye-token", t);
      else t = sessionStorage.getItem("diskeye-token") || "";
    } catch (e) { /* storage unavailable */ }
    S.token = t;
    if (t && !h.token) writeHash({ token: t });
  }

  async function api(path, opts) {
    const o = opts || {};
    const res = await fetch("/api/" + path, {
      method: o.method || "GET",
      headers: { "X-Diskeye-Token": S.token, ...(o.body ? { "Content-Type": "application/json" } : {}) },
      body: o.body ? JSON.stringify(o.body) : undefined,
      cache: "no-store",
    });
    let body = null;
    try { body = await res.json(); } catch (e) { body = null; }
    if (res.status === 401) {
      banner("error", "This page needs the access token. Open the full URL printed by <code>diskeye serve</code> (it ends in <code>#token=…</code>).");
      throw new Error("unauthorized");
    }
    if (!res.ok) {
      const err = new Error((body && body.error) || res.statusText);
      err.status = res.status;
      err.body = body;
      throw err;
    }
    return body;
  }
  async function cached(path) {
    if (S.cache.has(path)) return S.cache.get(path);
    const p = api(path);
    S.cache.set(path, p);
    p.catch(() => S.cache.delete(path));
    return p;
  }

  function banner(kind, html) {
    const b = document.getElementById("banners");
    if ([...b.children].some((c) => c.dataset.html === html)) return;
    const d = document.createElement("div");
    d.className = "banner " + kind;
    d.dataset.html = html;
    d.innerHTML = html;
    b.appendChild(d);
  }

  function setTab(tab, extra, push) {
    writeHash({ tab, ...(extra || {}) }, push !== false);
    route();
  }

  function route() {
    const h = parseHash();
    const tab = TABS.includes(h.tab) ? h.tab : "overview";
    S.tab = tab;
    document.querySelectorAll(".tabs a").forEach((a) => a.classList.toggle("on", a.dataset.tab === tab));
    document.querySelectorAll(".view").forEach((v) => v.classList.toggle("on", v.id === "view-" + tab));
    DK.hideTip();
    const run = { overview, physical, files, workloads, reclaim, diff }[tab];
    run(h).catch(showError(tab));
  }

  const showError = (tab) => (e) => {
    if (e && e.message === "unauthorized") return;
    console.error(e);
    const v = document.getElementById("view-" + tab);
    const box = document.createElement("div");
    box.className = "card hint";
    box.textContent = "Failed to load: " + (e && e.message ? e.message : e);
    v.prepend(box);
  };

  // ------------------------------------------------------------ small builders
  const chip = (risk) => `<span class="chip ${esc(risk)}"><span class="dot"></span>${esc(risk)}</span>`;
  const bar = (frac, cls, title) =>
    `<div class="bar"${title ? ` title="${esc(title)}"` : ""}><span class="${cls || ""}" style="width:${Math.max(0, Math.min(1, frac || 0)) * 100}%"></span></div>`;
  const dualBar = (total, unique, max) => {
    const t = max > 0 ? total / max : 0, u = max > 0 ? unique / max : 0;
    return `<div class="bar wide" title="${esc(size(unique))} unique of ${esc(size(total))}"><span class="light" style="width:${Math.min(1, t) * 100}%"></span><span style="width:${Math.min(1, u) * 100}%"></span></div>`;
  };
  const loading = (el, msg) => { el.innerHTML = `<div class="loading">${esc(msg || "Loading…")}</div>`; };
  const filesLink = (id, text) => (id == null ? esc(text) : `<a href="#" data-node="${id}" class="mono">${esc(text)}</a>`);
  const legend = (items) =>
    `<div class="legend">${items.map(([cls, label, style]) => `<span class="k"><span class="sw ${cls || ""}" style="${style || ""}"></span>${esc(label)}</span>`).join("")}</div>`;

  /** Delegate clicks on [data-node] links to the Files view. */
  document.addEventListener("click", (ev) => {
    const a = ev.target.closest("[data-node]");
    if (a) {
      ev.preventDefault();
      setTab("files", { node: a.dataset.node });
      return;
    }
    const t = ev.target.closest("[data-goto-tab]");
    if (t) {
      ev.preventDefault();
      setTab(t.dataset.gotoTab);
    }
  });

  // ------------------------------------------------------------ header
  async function loadSummary() {
    if (S.summary) return S.summary;
    S.summary = await cached("summary");
    const s = S.summary, m = s.meta;
    $("#meta").innerHTML =
      `<b>${esc(m.host)}</b> · scanned ${esc(s.started_human)} (${esc(DK.age(m.started))}) as ${s.scan_as_root ? "root" : "user"} · ` +
      `${DK.count(s.counts.nodes)} items · ${(m.duration_ms / 1000).toFixed(1)}s`;
    $("#meta").title = s.snapshot_path || "";
    if (s.server_root) {
      banner("root", "⚠ This server runs as <b>root</b>. Cleanup actions run with full privileges and need <code>delete</code> typed to confirm. Stop the server when you're done.");
    }
    return s;
  }

  // ------------------------------------------------------------ rescan
  const R = { btn: $("#rescan"), gen: null, watching: false };
  function rescanUI(st) {
    R.btn.hidden = !st.available;
    R.btn.disabled = st.running;
    R.btn.textContent = st.running ? "Scanning…" : "Re-scan";
    R.btn.title = st.running ? `Scan started ${DK.age(st.started)}; the page reloads when it's done` : "Scan the system again and reload";
  }
  async function pollRescan() {
    const st = await api("rescan");
    if (R.gen == null) R.gen = st.generation;
    rescanUI(st);
    if (st.running) {
      R.watching = true;
      setTimeout(() => pollRescan().catch(console.error), 1500);
    } else if (st.generation !== R.gen) {
      // Node and entity ids belong to the old snapshot.
      writeHash({ node: null, entity: null });
      location.reload();
    } else if (st.error && R.watching) {
      R.watching = false;
      banner("error", "Re-scan failed: " + esc(st.error));
    }
  }
  R.btn.addEventListener("click", async () => {
    R.btn.disabled = true;
    try {
      await api("rescan", { method: "POST" });
    } catch (e) {
      if (e.status !== 409) banner("error", "Re-scan failed: " + esc(e.message));
    }
    pollRescan().catch(console.error);
  });

  // ------------------------------------------------------------ Overview
  async function overview() {
    const v = $("#view-overview");
    if (!v.dataset.ready) loading(v);
    const [s, fs, wl, rc, hs, dl] = await Promise.all([
      loadSummary(), cached("filesystems"), cached("workloads"), api("reclaim"), cached("hotspots"), cached("deleted-open"),
    ]);
    const t = s.totals;
    const kpis = [
      ["Used on scanned filesystems", size(t.fs_used), `of ${size(t.fs_total)} · ${DK.pct(t.fs_used / t.fs_total)} full · ${size(t.fs_avail)} free`],
      ["Found by the scan", size(t.scanned), `${DK.pct(t.scanned / Math.max(1, t.fs_used))} of used · ${DK.count(s.counts.nodes)} items`],
      ["Owned by workloads", size(t.workloads), `${DK.count(s.counts.entities)} entities in ${s.groups.length} groups`],
      ["Reclaimable (safe)", size(t.reclaim_safe), `+ ${size(t.reclaim_review)} to review · ${size(t.reclaim_danger)} risky`],
      ["Deleted but still open", size(t.deleted_open), `${DK.count(s.counts.deleted_open)} files held by processes`],
      ["Unaccounted", DK.signed(t.unaccounted), `fs metadata, ${DK.count(s.counts.denied_dirs)} unreadable dirs, churn`],
    ];
    v.dataset.ready = "1";
    v.innerHTML = `
      <div class="grid kpis">${kpis.map(([l, val, n]) => `<div class="card kpi"><div class="label">${esc(l)}</div><div class="value">${esc(val)}</div><div class="note">${esc(n)}</div></div>`).join("")}</div>
      <div class="card" style="margin-top:14px"><h2>Filesystems <span class="sub">where every used byte went: scanned + deleted-open + hidden + unaccounted = used</span></h2><div id="ov-fs"></div></div>
      <div class="grid two" style="margin-top:14px">
        <div class="card"><h2>Workloads <span class="sub">by owner</span><a class="right" href="#" data-goto-tab="workloads">All workloads →</a></h2><div id="ov-wl"></div></div>
        <div class="card"><h2>Top reclaimable <a class="right" href="#" data-goto-tab="reclaim">Review & clean →</a></h2><div id="ov-rc"></div></div>
        <div class="card"><h2>Heaviest directories <span class="sub">most specific large dirs</span></h2><div id="ov-hs"></div></div>
        <div class="card"><h2>Coverage & blind spots</h2><div id="ov-cov"></div></div>
      </div>`;
    renderFsBars($("#ov-fs"), fs);
    renderWlSummary($("#ov-wl"), wl, s);
    renderTopReclaim($("#ov-rc"), rc);
    renderHotspots($("#ov-hs"), hs);
    renderCoverage($("#ov-cov"), s, fs, dl);
  }

  const SEGS = [
    ["scanned", "seg-scanned", "scanned files"],
    ["deleted_open", "seg-deleted", "deleted but open"],
    ["hidden", "seg-hidden", "hidden under mounts"],
    ["unaccounted", "seg-unaccounted", "unaccounted"],
    ["reserved", "seg-reserved", "reserved for root"],
    ["avail", "seg-free", "free"],
  ];

  function renderFsBars(el, fs) {
    const rows = fs.filesystems.filter((f) => f.reconcile);
    if (!rows.length) { el.innerHTML = `<div class="empty">No scanned filesystems with usage data.</div>`; return; }
    el.innerHTML = `<div class="tablewrap"><table class="t"><thead><tr><th>Mount</th><th style="width:45%">Breakdown of capacity</th><th class="num">Used</th><th class="num">Size</th><th class="num">Free</th><th class="num">Unaccounted</th></tr></thead><tbody>${rows.map((f) => {
      const r = f.reconcile;
      const tot = Math.max(1, r.total);
      const segs = SEGS.map(([k, cls, label]) => {
        const val = Math.max(0, r[k] || 0);
        return val > 0 ? `<span class="${cls}" style="flex:${val / tot}" data-tip="${esc(label)}: ${esc(size(val))} (${esc(DK.pct(val / tot))})"></span>` : "";
      }).join("");
      const usedPct = r.used / Math.max(1, r.used + r.avail);
      const warn = usedPct > 0.9 ? ' <span class="badge warn">' + DK.pct(usedPct) + " full</span>" : "";
      return `<tr><td class="nowrap">${filesLink(f.root_node, f.mount_point)} <span class="muted small">${esc(f.fstype)}</span>${warn}</td>
        <td><div class="stackbar">${segs}</div></td>
        <td class="num">${size(r.used)}</td><td class="num">${size(r.total)}</td><td class="num">${size(r.avail)}</td>
        <td class="num ${r.unaccounted < 0 ? "muted" : ""}" title="${r.denied_dirs ? r.denied_dirs + " unreadable directories" : ""}">${DK.signed(r.unaccounted)}</td></tr>`;
    }).join("")}</tbody></table></div>` + legend(SEGS.map(([, cls, label]) => [cls, label]));
    el.querySelectorAll("[data-tip]").forEach((s) => {
      s.addEventListener("mousemove", (ev) => DK.showTip(esc(s.dataset.tip), ev));
      s.addEventListener("mouseleave", DK.hideTip);
    });
  }

  function renderWlSummary(el, wl, s) {
    const groups = wl.groups.filter((g) => g.total > 0);
    if (!groups.length) {
      const absent = s.providers.filter((p) => p.coverage === "absent").map((p) => p.name);
      el.innerHTML = `<div class="empty">No workloads detected.${absent.length ? `<br><span class="small">Not present: ${esc(absent.join(", "))}</span>` : ""}</div>`;
      return;
    }
    const max = groups[0].total;
    el.innerHTML = `<table class="t"><tbody>${groups.slice(0, 10).map((g) => `<tr class="click" data-goto-tab="workloads">
      <td class="ellipsis" style="max-width:220px" title="${esc(g.name)}">${esc(g.name)}</td>
      <td style="width:45%">${bar(g.total / max)}</td><td class="num">${size(g.total)}</td>
      <td class="num small ink2">${g.reclaimable > 0 ? `${size(g.reclaimable)} reclaimable` : ""}</td></tr>`).join("")}</tbody></table>
      ${groups.length > 10 ? `<div class="muted small">… ${groups.length - 10} more groups</div>` : ""}`;
  }

  function renderTopReclaim(el, rc) {
    const items = rc.items.slice(0, 8);
    if (!items.length) { el.innerHTML = `<div class="empty">Nothing flagged as reclaimable.</div>`; return; }
    el.innerHTML = `<table class="t"><tbody>${items.map((r) => `<tr class="click" data-goto-tab="reclaim">
      <td>${chip(r.risk)}</td><td class="num">${size(r.bytes)}</td>
      <td class="ellipsis" style="max-width:260px" title="${esc(r.name)}">${esc(r.name)} <span class="muted small">${esc(r.kind_label)}</span></td>
      <td class="muted small ellipsis" style="max-width:200px" title="${esc(r.reason)}">${esc(r.reason)}</td></tr>`).join("")}</tbody></table>`;
  }

  function renderHotspots(el, hs) {
    const rows = hs.hotspots.slice(0, 14);
    if (!rows.length) { el.innerHTML = `<div class="empty">No large directories found.</div>`; return; }
    el.innerHTML = `<table class="t"><thead><tr><th>Directory</th><th>Share of its filesystem</th><th class="num">Size</th></tr></thead><tbody>${rows.map((h) => `<tr>
      <td class="ellipsis" style="max-width:360px" title="${esc(h.path)}">${filesLink(h.id, h.path)}${h.owner ? ` <span class="badge owner">${esc(h.owner.name)}</span>` : ""}</td>
      <td style="width:28%">${bar(h.fs_share, "", DK.pct(h.fs_share) + " of " + (h.fs || ""))}</td><td class="num">${size(h.alloc)}</td></tr>`).join("")}</tbody></table>`;
  }

  function renderCoverage(el, s, fs, dl) {
    const parts = [];
    if (s.coverage_gaps.length) {
      parts.push(`<table class="t"><tbody>${s.coverage_gaps.map((g) => `<tr><td class="nowrap"><span class="flagicon">!</span>${esc(g.name)}</td><td><span class="badge warn">${esc(g.coverage)}</span></td><td class="small">${esc(g.notes.join("; "))}</td></tr>`).join("")}</tbody></table>`);
    } else {
      parts.push(`<div class="small ink2">All providers reported complete coverage.</div>`);
    }
    if (!s.scan_as_root) {
      parts.push(`<div class="hint">This snapshot was taken without root, so some directories, processes and volumes were invisible. For the full picture run <code>sudo diskeye scan</code>, then <code>diskeye serve</code> again (it opens the newest snapshot).</div>`);
    }
    const skipped = fs.skipped.filter((f) => (f.used || 0) > 0);
    if (skipped.length) {
      parts.push(`<h2 style="margin-top:14px">Not walked</h2><table class="t"><tbody>${skipped.map((f) => `<tr><td class="mono">${esc(f.mount_point)}</td><td class="muted small">${esc(f.fstype)}</td><td class="num">${size(f.used)}</td><td class="small muted">${esc(f.reason || "")}</td></tr>`).join("")}</tbody></table>`);
    }
    if (dl.deleted_open.length) {
      parts.push(`<h2 style="margin-top:14px">Deleted but still open <span class="sub">${size(dl.total)} freed when these processes close them</span></h2><table class="t"><tbody>${dl.deleted_open.slice(0, 6).map((d) => `<tr><td class="num">${size(d.alloc)}</td><td class="nowrap">pid ${d.pid} <span class="muted">${esc(d.comm)}</span></td><td class="mono small ellipsis" style="max-width:260px" title="${esc(d.path)}">${esc(d.path)}</td></tr>`).join("")}</tbody></table>${dl.deleted_open.length > 6 ? `<div class="muted small">… ${dl.deleted_open.length - 6} more</div>` : ""}`);
    }
    if (dl.hidden.length) {
      parts.push(`<h2 style="margin-top:14px">Hidden under mountpoints</h2><table class="t"><tbody>${dl.hidden.map((h) => `<tr><td class="num">${size(h.alloc)}</td><td class="mono">${esc(h.path)}</td><td class="small muted">${DK.count(h.items)} items under the mount on ${esc(h.fs || "")}</td></tr>`).join("")}</tbody></table>`);
    }
    el.innerHTML = parts.join("");
  }

  // ------------------------------------------------------------ Physical
  const PHYS_KINDS = [
    ["disk", "disk", "--ink-2"],
    ["part", "partition", "--s3"],
    ["vg", "volume group", "--s5"],
    ["lvm", "logical volume", "--s7"],
    ["crypt", "encrypted", "--s4"],
    ["swap", "swap", "--s2"],
    ["fsused", "filesystem used", "--s1"],
  ];
  function physFill(n) {
    if (n.kind === "fsused") return DK.css("--s1");
    if (n.fstype === "swap" || n.kind === "swap") return DK.css("--s2");
    const k = PHYS_KINDS.find(([kind]) => kind === n.kind);
    return DK.css(k ? k[2] : "--other");
  }

  async function physical() {
    const v = $("#view-physical");
    if (!v.dataset.ready) loading(v);
    const [ph, fs] = await Promise.all([cached("physical"), cached("filesystems"), loadSummary()]);
    v.dataset.ready = "1";
    const rootByMount = new Map(fs.filesystems.filter((f) => f.root_node != null).map((f) => [f.mount_point, f.root_node]));
    v.innerHTML = `
      <div class="card"><h2>Storage layout <span class="sub">disk → partition → volume group → logical volume → filesystem; hatched = free / unallocated; amber outline = needs attention</span></h2>
        <div id="ph-icicle" class="icicle"></div>
        ${legend([...PHYS_KINDS.map(([, label, c]) => ["", label, `background:var(${c})`]), ["free-fill", "free / unallocated"], ["", "flagged", "background:transparent;border:2px solid var(--warning)"]])}
      </div>
      <div class="card" style="margin-top:14px"><h2>Devices</h2><div class="tablewrap" id="ph-table"></div></div>
      <div class="grid two" style="margin-top:14px">
        <div class="card"><h2>LVM <span class="sub">${esc(ph.lvm.source || "")}</span></h2><div id="ph-lvm"></div></div>
        <div class="card"><h2>Swap & warnings</h2><div id="ph-warn"></div></div>
      </div>`;
    const draw = () => DK.icicle($("#ph-icicle"), ph.rows, {
      fills: physFill,
      onHover(n, ev) {
        const title = n.kind === "fsused" || n.kind === "fsfree" ? `${n.parentLabel}: ${n.label}` : n.label;
        DK.showTip(`<div class="tt-title">${esc(title)}</div>` + DK.tipRows([
          ["type", n.kind === "fsused" || n.kind === "fsfree" ? "filesystem" : n.kind],
          ["size", size(n.kind === "fsused" ? n.used : n.kind === "fsfree" ? n.avail : n.size)],
          ["filesystem", n.fstype], ["mounted on", n.mount],
          ["used", n.kind === "fsused" || n.kind === "fsfree" ? null : n.used != null ? size(n.used) : null],
          ["free", n.kind === "fsused" || n.kind === "fsfree" ? null : n.avail != null ? size(n.avail) : null],
        ]) + (n.note ? `<div class="tt-badges"><span class="badge ${n.flag ? "warn" : ""}">${esc(n.note)}</span></div>` : ""), ev);
      },
      onClick(n) {
        const id = n.mount && rootByMount.get(n.mount);
        if (id != null) setTab("files", { node: id });
      },
    });
    draw();
    S.redrawPhysical = draw;
    $("#ph-table").innerHTML = `<table class="t"><thead><tr><th>Device</th><th>Type</th><th>Mount</th><th class="num">Size</th><th>Usage</th><th class="num">Free</th><th>Note</th></tr></thead><tbody>${ph.rows.map((r) => {
      const usage = r.used != null && r.avail != null && r.used + r.avail > 0 ? r.used / (r.used + r.avail) : null;
      const rid = r.mount && rootByMount.get(r.mount);
      return `<tr class="${r.flag ? "flag" : ""}${rid != null ? " click" : ""}"${rid != null ? ` data-node="${rid}"` : ""}>
        <td class="nowrap"><span class="indent" style="width:${r.depth * 16}px"></span>${r.flag ? '<span class="flagicon" title="needs attention">!</span>' : ""}${r.kind === "free" ? `<span class="sw free-fill" style="display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:6px"></span>` : ""}${esc(r.label)}</td>
        <td class="small">${esc(r.kind)}${r.fstype ? ` <span class="muted">${esc(r.fstype)}</span>` : ""}</td>
        <td class="mono small">${esc(r.mount || "")}</td>
        <td class="num">${size(r.size)}</td>
        <td style="min-width:140px">${usage != null ? `<div style="display:flex;gap:8px;align-items:center">${bar(usage, "", "")}<span class="num small">${DK.pct(usage)}</span></div>` : ""}</td>
        <td class="num">${r.avail != null ? size(r.avail) : ""}</td>
        <td class="small ${r.flag ? "" : "muted"}">${esc(r.note)}</td></tr>`;
    }).join("")}</tbody></table>`;
    const l = ph.lvm;
    $("#ph-lvm").innerHTML = !l.vgs.length && !l.lvs.length ? `<div class="empty">No LVM volume groups.</div>` :
      `<table class="t"><thead><tr><th>VG</th><th class="num">Size</th><th class="num">Free</th><th></th></tr></thead><tbody>${l.vgs.map((g) => `<tr><td>${esc(g.name)}</td><td class="num">${size(g.size)}</td><td class="num">${size(g.free)}</td><td class="small muted">${g.free_is_estimate ? "estimated" : ""}</td></tr>`).join("")}</tbody></table>
       <table class="t" style="margin-top:10px"><thead><tr><th>LV</th><th class="num">Size</th><th>Mount / use</th><th>Attr</th><th class="num">Data%</th></tr></thead><tbody>${l.lvs.map((x) => {
        const unused = ph.unused_lvs.some((u) => u.vg === x.vg && u.name === x.name);
        return `<tr class="${unused ? "flag" : ""}"><td class="nowrap">${unused ? '<span class="flagicon">!</span>' : ""}${esc(x.vg)}/${esc(x.name)}</td><td class="num">${size(x.size)}</td><td class="mono small">${esc(x.mountpoints.join(", ") || x.fstype || (unused ? "not mounted, not used by a known VM" : ""))}</td><td class="mono small">${esc(x.attr || x.segtype || "")}</td><td class="num">${x.data_percent != null ? x.data_percent.toFixed(1) + "%" : ""}</td></tr>`;
      }).join("")}</tbody></table>`;
    const warn = [];
    ph.unused_lvs.forEach((u) => warn.push(`<div class="hint"><span class="flagicon">!</span>LV <b>${esc(u.vg)}/${esc(u.name)}</b> (${size(u.size)}) is not mounted and not used by any known VM.</div>`));
    ph.rows.filter((r) => r.flag && r.kind !== "free" && r.kind !== "lvm").forEach((r) => warn.push(`<div class="hint"><span class="flagicon">!</span><b>${esc(r.label)}</b> (${size(r.size)}): ${esc(r.note)}</div>`));
    const sw = ph.swaps.length ? `<table class="t"><thead><tr><th>Swap</th><th>Kind</th><th class="num">Size</th><th class="num">Used</th></tr></thead><tbody>${ph.swaps.map((s) => `<tr><td class="mono small">${esc(s.path)}</td><td>${esc(s.kind)}</td><td class="num">${size(s.size)}</td><td class="num">${size(s.used)}</td></tr>`).join("")}</tbody></table>` : `<div class="muted small">No active swap.</div>`;
    $("#ph-warn").innerHTML = sw + (warn.length ? warn.join("") : `<div class="muted small" style="margin-top:8px">No physical-layer warnings.</div>`);
  }

  // ------------------------------------------------------------ Files
  const F = S.files;

  function ownerColor(row) {
    const o = row.owner;
    if (!o) return DK.css("--unowned");
    if (o.entity) return DK.slot(o.entity.group_idx);
    if (o.mixed) return DK.mix(DK.css("--unowned"), DK.slot(o.mixed.group_idx), o.mixed.share >= 0.5 ? 0.65 : 0.3);
    return DK.css("--unowned");
  }
  function kindColor(row) {
    if (row.mount_of != null) return DK.css("--s7");
    if (row.flags && row.flags.includes("denied")) return DK.css("--s4");
    if (row.kind === "dir") return DK.css("--s1");
    if (row.kind === "file") return DK.css("--s3");
    return DK.css("--other");
  }
  function colorFor(row, d) {
    if (row.other) return DK.css("--free");
    if (F.color === "owner") return ownerColor(row);
    if (F.color === "kind") return kindColor(row);
    // by top folder: the depth-1 ancestor's rank
    let a = d;
    while (a && a.depth > 1) a = a.parent;
    const i = a && a.parent ? a.parent.children.indexOf(a) : 0;
    return DK.slot(i);
  }

  function rowPath(row) {
    const base = F.data ? F.data.path : "";
    if (row.other) return base;
    if (row.mount_of != null && row.fs) return row.fs;
    return (base.endsWith("/") ? base : base + "/") + row.name;
  }

  function fileTip(row, d, ev) {
    const fmt = DK.metricFmt(F.metric);
    let path = rowPath(row);
    if (d && d.depth > 1 && d.parent && d.parent.data) {
      const p = d.parent.data;
      const pp = p.mount_of != null && p.fs ? p.fs : rowPath(p);
      path = row.other ? pp : (pp.endsWith("/") ? pp : pp + "/") + row.name;
    }
    const owner = row.owner && row.owner.entity ? `${row.owner.entity.kind_label}: ${row.owner.entity.name}${row.owner.direct ? "" : " (inherited)"}`
      : row.owner && row.owner.mixed ? `${DK.pct(row.owner.mixed.share)} ${row.owner.mixed.group}` : null;
    DK.showTip(`<div class="tt-title">${esc(row.other ? `${row.name} in ${path}` : path)}</div>` + DK.tipRows([
      [F.metric === "items" ? "items" : F.metric === "apparent" ? "apparent size" : "disk usage", fmt(row.value)],
      ["share of view", F.data && F.data.value ? DK.pct(row.value / Math.max(1, sumValue(F.data))) : null],
      F.metric !== "alloc" ? ["disk usage", size(row.alloc)] : null,
      F.metric !== "apparent" ? ["apparent", size(row.apparent)] : null,
      F.metric !== "items" ? ["items", DK.count(row.items)] : null,
      row.mtime ? ["newest change", DK.date(row.mtime)] : null,
      ["owner", owner],
    ]) + (row.badges && row.badges.length ? `<div class="tt-badges">${row.badges.map((b) => `<span class="badge">${esc(b)}</span>`).join("")}</div>` : "") +
      (row.other ? `<div class="muted small" style="margin-top:4px">folded; use “show more” in the table</div>` : ""), ev);
  }
  const sumValue = (data) => (data.children || []).reduce((s, c) => s + (c.value || 0), 0) || data.value || 1;

  async function loadNode(id) {
    const lim = F.limit[id] || (F.chart === "sunburst" ? 48 : 80);
    const depth = F.chart === "sunburst" ? 3 : 2;
    const sub = F.chart === "sunburst" ? 14 : 32;
    return cached(`node/${id}?metric=${F.metric}&depth=${depth}&limit=${lim}&sublimit=${sub}`);
  }

  async function files(h) {
    DK.hideTip();
    await loadSummary();
    F.metric = ["alloc", "apparent", "items"].includes(h.metric) ? h.metric : F.metric;
    if (!h.color && !F.colorSet && !S.summary.groups.length) F.color = "folder";
    F.colorSet = F.colorSet || !!h.color;
    F.color = ["owner", "folder", "kind"].includes(h.color) ? h.color : F.color;
    F.chart = h.chart === "sunburst" ? "sunburst" : h.chart === "treemap" ? "treemap" : F.chart;
    $("#metric").value = F.metric;
    $("#colorby").value = F.color;
    document.querySelectorAll("#chartkind button").forEach((b) => b.classList.toggle("on", b.dataset.kind === F.chart));
    let id = h.node != null && h.node !== "" ? Number(h.node) : null;
    if (id == null || isNaN(id)) {
      const roots = await cached("roots");
      id = roots.top.length ? roots.top[0].id : roots.all.length ? roots.all[0].id : null;
    }
    if (id == null) {
      $("#files-chart").innerHTML = `<div class="empty">This snapshot contains no scanned files.</div>`;
      return;
    }
    const token = ++F.busy;
    const zoom = F.chartApi && F.data && F.data.id !== id ? F.chartApi.zoomTo(id) : Promise.resolve();
    const [data] = await Promise.all([loadNode(id), zoom]);
    if (token !== F.busy) return; // a newer navigation won
    F.id = data.id;
    F.data = data;
    renderFiles();
  }

  function openRow(row, d) {
    if (row.other) {
      showMore();
      return;
    }
    let target = row;
    if (d && d.depth > 1 && !(row.has_children)) target = d.parent.data; // a file inside a group: open the group
    if (!target.has_children || target.id == null) {
      F.chartApi && F.chartApi.highlight(row.id);
      highlightRow(row.id);
      return;
    }
    writeHash({ tab: "files", node: target.id }, true);
    files(parseHash()).catch(showError("files"));
  }
  function goUp() {
    const c = F.data && F.data.crumbs;
    if (c && c.length > 1) {
      writeHash({ tab: "files", node: c[c.length - 2].id }, true);
      files(parseHash()).catch(showError("files"));
    }
  }
  function showMore() {
    const cur = F.limit[F.id] || (F.chart === "sunburst" ? 48 : 80);
    F.limit[F.id] = Math.min(5000, cur * 4);
    loadNode(F.id).then((data) => { F.data = data; renderFiles(); }).catch(showError("files"));
  }

  function renderFiles() {
    const data = F.data;
    const fmt = DK.metricFmt(F.metric);
    // breadcrumb
    $("#crumbs").innerHTML = data.crumbs.map((c, i) => {
      const last = i === data.crumbs.length - 1;
      return `${i > 0 && data.crumbs[i - 1].name !== "/" ? '<span class="sep">/</span>' : ""}<a href="#" class="${last ? "cur" : ""}" data-crumb="${c.id}">${esc(c.name)}</a>`;
    }).join("");
    $("#crumbs").querySelectorAll("[data-crumb]").forEach((a) => a.addEventListener("click", (ev) => {
      ev.preventDefault();
      writeHash({ tab: "files", node: a.dataset.crumb }, true);
      files(parseHash()).catch(showError("files"));
    }));
    // chart
    const el = $("#files-chart");
    const opts = {
      color: colorFor, fmt,
      onOpen: openRow,
      onHover: (row, d, ev) => { fileTip(row, d, ev); highlightRow(row.id, true); },
      onLeave: () => { DK.hideTip(); highlightRow(null, true); },
      onUp: goUp, canUp: data.crumbs.length > 1,
    };
    if (!data.children.length) {
      el.innerHTML = `<div class="empty">${data.kind === "dir" ? (data.flags.includes("denied") ? "Permission denied: contents unknown (scan with sudo)." : "Empty directory.") : "This is a file."}</div>`;
      F.chartApi = null;
    } else {
      F.chartApi = F.chart === "sunburst" ? DK.sunburst(el, data, opts) : DK.treemap(el, data, opts);
      // A scrollbar appearing after the first layout changes the width: redraw once.
      const w0 = el.clientWidth;
      requestAnimationFrame(() => {
        if (F.data === data && el.clientWidth !== w0) F.chartApi = F.chart === "sunburst" ? DK.sunburst(el, data, opts) : DK.treemap(el, data, opts);
      });
    }
    renderFilesLegend(data);
    renderFilesSide(data, fmt);
  }

  function renderFilesLegend(data) {
    const el = $("#files-legend");
    if (F.color === "owner") {
      const seen = new Map();
      const visit = (rows) => (rows || []).forEach((r) => {
        const o = r.owner;
        if (o && o.entity && o.entity.group_idx != null) seen.set(o.entity.group_idx, o.entity.group);
        if (o && o.mixed) seen.set(o.mixed.group_idx, o.mixed.group);
        visit(r.children);
      });
      visit(data.children);
      const items = [...seen.entries()].sort((a, b) => a[0] - b[0]).map(([i, g]) => ["", g, `background:${DK.slotVar(i)}`]);
      items.push(["", "no known owner", "background:var(--unowned)"]);
      el.outerHTML = `<div id="files-legend">${legend(items)}<div class="muted small">Faded tiles: directories that mostly contain an owner's data without being owned themselves.</div></div>`;
    } else if (F.color === "kind") {
      el.outerHTML = `<div id="files-legend">${legend([["", "directory", "background:var(--s1)"], ["", "file", "background:var(--s3)"], ["", "other filesystem", "background:var(--s7)"], ["", "unreadable", "background:var(--s4)"], ["free-fill", "folded small items"]])}</div>`;
    } else {
      const top = data.children.filter((c) => !c.other).slice(0, 8).map((c, i) => ["", c.name, `background:${DK.slotVar(i)}`]);
      if (data.children.filter((c) => !c.other).length > 8) top.push(["", "other folders", "background:var(--other)"]);
      el.outerHTML = `<div id="files-legend">${legend(top)}</div>`;
    }
  }

  function renderFilesSide(data, fmt) {
    const owners = data.owners.length ? data.owners.map((o) => `<a href="#" class="badge owner" data-entity="${o.id}">${esc(o.kind_label)}: ${esc(o.name)}</a>`).join("") : "";
    const badges = data.badges.filter((b) => !data.owners.length || !b.includes(data.owners[0].name)).map((b) => `<span class="badge">${esc(b)}</span>`).join("");
    $("#files-info").innerHTML = `<div class="info"><h3>${esc(data.path)}</h3>
      <div class="stats">
        <span class="k">disk usage</span><span>${size(data.alloc)}</span>
        <span class="k">apparent</span><span>${size(data.apparent)}</span>
        <span class="k">items</span><span>${DK.count(data.items)}</span>
        <span class="k">newest change</span><span>${DK.date(data.mtime)}</span>
        ${data.fs ? `<span class="k">filesystem</span><span>${esc(data.fs.mount_point)} <span class="muted">${esc(data.fs.fstype)}</span></span>` : ""}
      </div>${owners}${badges}</div>`;
    $("#files-info").querySelectorAll("[data-entity]").forEach((a) => a.addEventListener("click", (ev) => {
      ev.preventDefault();
      S.wl.open.add("e" + a.dataset.entity);
      setTab("workloads", { entity: a.dataset.entity });
    }));
    const rows = data.children;
    const tot = Math.max(1, rows.reduce((s, r) => s + (r.value || 0), 0));
    const max = Math.max(1, ...rows.map((r) => r.value || 0));
    const icon = (r) => (r.other ? "⋯" : r.mount_of != null ? "⛁" : r.kind === "dir" ? "▸" : r.kind === "symlink" ? "↪" : "·");
    $("#files-table").innerHTML = `<table class="t" style="margin-top:10px"><thead><tr><th>Name</th><th>${F.metric === "items" ? "Items" : "Size"}</th><th class="num">%</th></tr></thead><tbody>${rows.map((r) => `
      <tr class="click" data-row="${r.id == null ? "other" : r.id}">
        <td class="ellipsis name" title="${esc(r.name)}"><span class="muted">${icon(r)}</span> ${esc(r.name)}${r.owner && r.owner.entity && r.owner.direct ? ` <span class="sw" style="display:inline-block;width:8px;height:8px;border-radius:2px;background:${DK.slotVar(r.owner.entity.group_idx)}" title="${esc(r.owner.entity.name)}"></span>` : ""}${r.flags && r.flags.includes("denied") ? ' <span class="badge warn">denied</span>' : ""}</td>
        <td style="min-width:120px"><div style="display:flex;gap:6px;align-items:center">${bar((r.value || 0) / max, r.other ? "light" : "")}<span class="num small" style="min-width:64px">${esc(fmt(r.value))}</span></div></td>
        <td class="num small">${DK.pct((r.value || 0) / tot)}</td></tr>`).join("")}</tbody></table>
      ${rows.some((r) => r.other) ? `<button class="btn small" id="files-more" style="margin-top:8px">Show more items</button>` : ""}`;
    $("#files-table").querySelectorAll("tr[data-row]").forEach((tr) => {
      const r = rows.find((x) => String(x.id == null ? "other" : x.id) === tr.dataset.row);
      tr.addEventListener("click", () => openRow(r, null));
      tr.addEventListener("mouseenter", () => F.chartApi && F.chartApi.highlight(r.id));
      tr.addEventListener("mouseleave", () => F.chartApi && F.chartApi.highlight(null));
    });
    const more = $("#files-more");
    if (more) more.addEventListener("click", showMore);
  }

  function highlightRow(id, fromChart) {
    document.querySelectorAll("#files-table tr[data-row]").forEach((tr) => tr.classList.toggle("hl", id != null && tr.dataset.row === String(id)));
    if (!fromChart && F.chartApi) F.chartApi.highlight(id);
  }

  // controls
  $("#metric").addEventListener("change", (e) => { writeHash({ metric: e.target.value }); files(parseHash()).catch(showError("files")); });
  $("#colorby").addEventListener("change", (e) => { F.color = e.target.value; writeHash({ color: e.target.value }); if (F.data) renderFiles(); });
  document.querySelectorAll("#chartkind button").forEach((b) => b.addEventListener("click", () => {
    writeHash({ chart: b.dataset.kind });
    F.chartApi = null;
    files(parseHash()).catch(showError("files"));
  }));
  $("#goto").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const p = $("#goto-input").value.trim();
    if (!p) return;
    try {
      const r = await api("path?p=" + encodeURIComponent(p.startsWith("/") ? p : "/" + p));
      $("#goto-input").value = "";
      writeHash({ tab: "files", node: r.id }, true);
      files(parseHash()).catch(showError("files"));
    } catch (e) {
      $("#goto-input").setCustomValidity(e.message);
      $("#goto-input").reportValidity();
      setTimeout(() => $("#goto-input").setCustomValidity(""), 2500);
    }
  });
  document.addEventListener("keydown", (ev) => {
    const t = ev.target;
    if (S.tab !== "files" || !$("#modal").hidden || (t &&t.closest && t.closest("input, select, textarea, button"))) return;
    if (ev.key === "Backspace" || (ev.key === "ArrowUp" && ev.altKey)) { ev.preventDefault(); goUp(); }
  });

  // ------------------------------------------------------------ Workloads
  async function workloads(h) {
    const v = $("#view-workloads");
    if (!v.dataset.ready) loading(v);
    const [s, wl] = await Promise.all([loadSummary(), cached("workloads")]);
    S.wl.data = wl;
    v.dataset.ready = "1";
    if (h.entity != null) {
      await openEntityPath(Number(h.entity));
    }
    const groups = wl.groups;
    if (!groups.length) {
      v.innerHTML = `<div class="card"><h2>Workloads</h2><div class="empty">No workloads were detected in this snapshot.</div>
        <table class="t"><thead><tr><th>Provider</th><th>Coverage</th><th>Notes</th></tr></thead><tbody>${s.providers.map((p) => `<tr><td>${esc(p.name)}</td><td>${esc(p.coverage)}</td><td class="small muted">${esc(p.notes.join("; "))}</td></tr>`).join("")}</tbody></table></div>`;
      return;
    }
    v.innerHTML = `<div class="card"><h2>Workloads <span class="sub">who owns the space; click a row to expand, a path to open it in Files</span>
      <span class="right"><button class="btn small" id="wl-expand">Expand groups</button> <button class="btn small" id="wl-collapse">Collapse</button></span></h2>
      <div class="tablewrap" id="wl-table"></div>
      ${legend([["", "unique (freed if removed alone)", "background:var(--s1)"], ["", "shared with others", "background:var(--s1);opacity:.35"]])}</div>`;
    $("#wl-expand").onclick = () => { groups.forEach((g) => S.wl.open.add("g" + g.name)); drawWl(); };
    $("#wl-collapse").onclick = () => { S.wl.open.clear(); drawWl(); };
    if (S.wl.open.size === 0 && groups.length <= 3) groups.forEach((g) => S.wl.open.add("g" + g.name));
    drawWl();
  }

  async function openEntityPath(id) {
    // Expand the group and every ancestor of the entity.
    try {
      const e = await api("entity/" + id);
      S.wl.detail.set(id, e);
      S.wl.open.add("g" + (e.ancestors.length ? e.ancestors[0].group : e.group));
      for (const a of e.ancestors) {
        if (!S.wl.detail.has(a.id)) S.wl.detail.set(a.id, await api("entity/" + a.id));
        S.wl.open.add("e" + a.id);
      }
      S.wl.open.add("e" + id);
      S.wl.focus = id;
    } catch (err) { console.warn(err); }
  }

  function drawWl() {
    const groups = S.wl.data.groups;
    const max = Math.max(1, ...groups.map((g) => g.total));
    const out = [];
    const entityRows = (ids, depth) => {
      ids.forEach((e) => {
        const key = "e" + e.id;
        const open = S.wl.open.has(key);
        const det = S.wl.detail.get(e.id);
        const expandable = e.children > 0 || e.paths > 0 || e.block_devs.length > 0 || e.attrs.length > 0;
        out.push(`<tr class="click${S.wl.focus === e.id ? " hl" : ""}" data-key="${key}" data-eid="${e.id}">
          <td class="nowrap"><span class="indent" style="width:${depth * 18}px"></span><span class="twisty">${expandable ? (open ? "▾" : "▸") : ""}</span><span class="sw" style="display:inline-block;width:8px;height:8px;border-radius:2px;margin-right:6px;background:${DK.slotVar(e.group_idx)}"></span>${esc(e.name)}</td>
          <td class="small muted">${esc(e.kind_label)}</td>
          <td>${dualBar(e.total, e.unique, max)}</td>
          <td class="num">${size(e.total)}</td><td class="num">${size(e.unique)}</td>
          <td class="num muted">${e.reported != null ? size(e.reported) : ""}</td>
          <td class="num muted">${e.virtual_size != null ? size(e.virtual_size) : ""}</td>
          <td>${e.reclaim ? chip(e.reclaim.risk) : ""}</td></tr>`);
        if (open && det) {
          const extra = [];
          if (det.path_list.length) extra.push(`<div class="plist">${det.path_list.map((p) => `<div>${p.node != null ? filesLink(p.node, p.path) : `<span class="muted">${esc(p.path)} (not in scan)</span>`} ${p.alloc != null ? `<span class="muted">${size(p.alloc)}</span>` : ""}</div>`).join("")}</div>`);
          if (det.block_devs.length) extra.push(`<div class="plist">block devices: ${det.block_devs.map((b) => `<code>${esc(b)}</code>`).join(", ")}${det.external ? ` · ${size(det.external)} outside scanned filesystems` : ""}</div>`);
          if (det.attrs.length) extra.push(`<div class="plist small muted">${det.attrs.map(([k, v2]) => `${esc(k)}=<span class="ink2">${esc(v2)}</span>`).join(" · ")}</div>`);
          if (det.reclaim) extra.push(`<div class="plist small">${chip(det.reclaim.risk)} ${esc(det.reclaim.reason)} ${det.reclaim.has_action ? `<a href="#" data-goto-tab="reclaim">clean up →</a>` : ""}</div>`);
          if (extra.length) out.push(`<tr class="wl-paths"><td colspan="8">${extra.join("")}</td></tr>`);
          entityRows(det.child_list, depth + 1);
        }
      });
    };
    groups.forEach((g) => {
      const key = "g" + g.name;
      const open = S.wl.open.has(key);
      out.push(`<tr class="group click" data-key="${esc(key)}">
        <td class="nowrap"><span class="twisty">${open ? "▾" : "▸"}</span><span class="sw" style="display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:6px;background:${DK.slotVar(g.idx)}"></span>${esc(g.name)} <span class="muted small">${g.entities.length}</span></td>
        <td></td><td>${dualBar(g.total, g.unique, max)}</td>
        <td class="num">${size(g.total)}</td><td class="num">${size(g.unique)}</td><td></td><td></td>
        <td class="num small ink2">${g.reclaimable > 0 ? size(g.reclaimable) : ""}</td></tr>`);
      if (open) entityRows(g.entities, 1);
    });
    $("#wl-table").innerHTML = `<table class="t"><thead><tr><th>Name</th><th>Kind</th><th style="width:22%">Size</th><th class="num">Total</th><th class="num">Unique</th><th class="num">Reported</th><th class="num">Virtual</th><th>Reclaimable</th></tr></thead><tbody>${out.join("")}</tbody></table>`;
    $("#wl-table").querySelectorAll("tr[data-key]").forEach((tr) => tr.addEventListener("click", async (ev) => {
      if (ev.target.closest("a")) return;
      const key = tr.dataset.key;
      if (S.wl.open.has(key)) S.wl.open.delete(key);
      else {
        S.wl.open.add(key);
        const eid = tr.dataset.eid;
        if (eid != null && !S.wl.detail.has(Number(eid))) {
          try { S.wl.detail.set(Number(eid), await api("entity/" + eid)); } catch (e) { console.warn(e); }
        }
      }
      drawWl();
    }));
  }

  // ------------------------------------------------------------ Reclaim
  async function reclaim() {
    const v = $("#view-reclaim");
    if (!v.dataset.ready) loading(v);
    await loadSummary();
    const rc = await api("reclaim");
    S.reclaim.data = rc;
    v.dataset.ready = "1";
    drawReclaim();
  }

  function drawReclaim() {
    const v = $("#view-reclaim");
    const rc = S.reclaim.data;
    const R = S.reclaim;
    const risks = ["safe", "review", "danger"];
    const sum = (r) => rc.items.filter((i) => r === "all" || i.risk === r).reduce((s, i) => s + i.bytes, 0);
    const cnt = (r) => rc.items.filter((i) => r === "all" || i.risk === r).length;
    const f = R.filter;
    const items = rc.items.filter((i) => f === "all" || i.risk === f);
    const runnable = (i) => i.has_action && !(i.done && i.done.ok);
    // Forget selections that are no longer runnable (e.g. done after a rescan).
    const byId = new Map(rc.items.map((i) => [i.entity, i]));
    R.sel.forEach((id) => { if (!byId.has(id) || !runnable(byId.get(id))) R.sel.delete(id); });
    const sel = [...R.sel].map((id) => byId.get(id));
    const visible = items.filter(runnable);
    const allOn = visible.length > 0 && visible.every((i) => R.sel.has(i.entity));
    v.innerHTML = `<div class="card"><h2>Reclaimable space <span class="sub">nothing runs until you preview the items and confirm</span></h2>
      <div class="filters">${["all", ...risks].map((r) => `<button class="btn ${f === r ? "on" : ""}" data-filter="${r}">${r === "all" ? "All" : chip(r)} <span class="muted small">${cnt(r)} · ${size(sum(r))}</span></button>`).join("")}
        <span class="spacer" style="flex:1"></span>
        <button class="btn small" id="sel-safe">Select all safe</button>
        ${sel.length ? `<button class="btn small" id="sel-clear">Clear</button>` : ""}
        <button class="btn ${sel.some((i) => i.risk !== "safe") ? "danger" : "primary"}" id="run-sel" ${sel.length ? "" : "disabled"}>Run selected${sel.length ? ` (${sel.length} · ${size(sel.reduce((s, i) => s + i.bytes, 0))})` : ""}…</button></div>
      ${!items.length ? `<div class="empty">Nothing to show${f !== "all" ? " for this risk level" : ""}.</div>` : `<div class="tablewrap"><table class="t fixed"><colgroup><col style="width:34px"><col style="width:84px"><col style="width:84px"><col><col style="width:150px"><col style="width:190px"><col style="width:200px"><col style="width:96px"></colgroup>
        <thead><tr><th><input type="checkbox" id="sel-all" aria-label="Select all shown" ${allOn ? "checked" : ""} ${visible.length ? "" : "disabled"}></th><th>Risk</th><th class="num">Frees</th><th>Item</th><th>Group</th><th>Why</th><th>Action</th><th></th></tr></thead><tbody>${items.map((i) => `
        <tr class="${i.done && i.done.ok ? "done" : ""}">
          <td>${runnable(i) ? `<input type="checkbox" data-sel="${i.entity}" aria-label="Select ${esc(i.name)}" ${R.sel.has(i.entity) ? "checked" : ""}>` : ""}</td>
          <td>${chip(i.risk)}</td><td class="num">${size(i.bytes)}</td>
          <td><div class="ellipsis" title="${esc(i.name)}">${esc(i.name)} <span class="muted small">${esc(i.kind_label)}</span></div>${i.paths.map((p) => `<div class="small ellipsis" title="${esc(p.path)}">${filesLink(p.node, p.path)}</div>`).join("")}</td>
          <td class="small">${esc(i.group)}</td>
          <td class="small ink2">${esc(i.reason)}</td>
          <td class="small"><div class="ellipsis" title="${esc(i.action ? i.action.steps.join("\n") : "")}">${i.action ? esc(i.action.label) : '<span class="muted">manual</span>'}</div>${i.done ? `<div class="small ${i.done.ok ? "" : "up-ink"}">${i.done.ok ? "✓ done" : "✗ failed"}</div>` : ""}</td>
          <td>${i.has_action ? `<button class="btn small" data-preview="${i.entity}">Preview…</button>` : ""}</td></tr>`).join("")}</tbody></table></div>`}
      <div class="muted small" style="margin-top:8px">Sizes are from the snapshot; after cleaning, rescan to refresh the numbers. Every executed action is logged to <code>~/.local/state/diskeye/actions.log</code>.</div></div>`;
    v.querySelectorAll("[data-filter]").forEach((b) => b.addEventListener("click", () => { R.filter = b.dataset.filter; drawReclaim(); }));
    v.querySelectorAll("[data-preview]").forEach((b) => b.addEventListener("click", () => preview([Number(b.dataset.preview)])));
    v.querySelectorAll("[data-sel]").forEach((c) => c.addEventListener("change", () => {
      const id = Number(c.dataset.sel);
      if (c.checked) R.sel.add(id); else R.sel.delete(id);
      drawReclaim();
    }));
    const all = $("#sel-all");
    if (all) all.addEventListener("change", () => { visible.forEach((i) => (all.checked ? R.sel.add(i.entity) : R.sel.delete(i.entity))); drawReclaim(); });
    $("#sel-safe").addEventListener("click", () => { rc.items.filter((i) => i.risk === "safe" && runnable(i)).forEach((i) => R.sel.add(i.entity)); drawReclaim(); });
    if ($("#sel-clear")) $("#sel-clear").addEventListener("click", () => { R.sel.clear(); drawReclaim(); });
    $("#run-sel").addEventListener("click", () => preview(sel.map((i) => i.entity)));
  }

  function modal(html) {
    $("#modal-body").innerHTML = html;
    $("#modal").hidden = false;
  }
  function closeModal() { $("#modal").hidden = true; }
  $("#modal-close").addEventListener("click", closeModal);
  $("#modal").addEventListener("click", (ev) => { if (ev.target.id === "modal") closeModal(); });
  document.addEventListener("keydown", (ev) => { if (ev.key === "Escape" && !$("#modal").hidden) closeModal(); });

  // The server accepts `delete` for anything, and yes/y when no item is strict.
  const confirmed = (text, strict) => {
    const t = text.trim().toLowerCase();
    return t === "delete" || (!strict && (t === "yes" || t === "y"));
  };

  /** Preview one or more reclaim items, confirm once, then run them in order. */
  async function preview(ids) {
    modal(`<div class="loading">Checking…</div>`);
    let ps;
    try {
      ps = await Promise.all(ids.map((id) => api(`action/${id}/preview`, { method: "POST" }).then((p) => ({ id, ...p }))));
    } catch (e) { modal(`<div class="result bad">${esc(e.message)}</div>`); return; }
    ps.sort((a, b) => b.preflight.ok - a.preflight.ok); // runnable first
    const ok = ps.filter((p) => p.preflight.ok);
    const strict = ok.some((p) => p.confirm.phrase !== "yes");
    const word = strict ? "delete" : "yes";
    const total = ok.reduce((s, p) => s + p.bytes, 0);
    const why = !strict ? "Type <code>yes</code> to confirm."
      : ok.some((p) => p.confirm.reason === "danger") ? "Includes a <b>danger</b> item: type <code>delete</code> to confirm."
      : "The server runs as <b>root</b>: type <code>delete</code> to confirm.";
    const one = ps.length === 1;
    const item = (p) => `<div class="batch-item" data-item="${p.id}">
        <div>${chip(p.risk)} <span class="ink2">${esc(p.entity.kind_label)}: <b>${esc(p.entity.name)}</b> · ${esc(p.label)} · frees about <b>${size(p.bytes)}</b></span></div>
        ${one ? `<div style="margin-top:12px" class="small muted">Exactly what will run:</div><div class="steps">${p.steps.map(esc).join("\n")}</div>`
          : `<details><summary class="small muted">steps</summary><div class="steps">${p.steps.map(esc).join("\n")}</div></details>`}
        ${p.preflight.ok ? (one ? `<div class="result ok">✓ Preflight passed: the action can run now.</div>` : "") : `<div class="result bad">✗ ${one ? "Preflight failed" : "Will be skipped"}: ${esc(p.preflight.error)}</div>`}
        ${p.done ? `<div class="result ${p.done.ok ? "ok" : "bad"}">Already run in this session: ${esc(p.done.message)}</div>` : ""}
        <div class="item-result"></div></div>`;
    modal(`<h3 id="modal-title">${one ? esc(ps[0].label) : `Run ${ok.length} of ${ps.length} items · frees about ${size(total)}`}</h3>
      ${ps.some((p) => p.server_root) ? `<div class="result bad" style="margin-top:10px">Running as root: these steps run with full privileges.</div>` : ""}
      <div class="batch">${ps.map(item).join("")}</div>
      ${ok.length ? `<div style="margin-top:12px" class="small">${why}</div>
      <div class="confirm-row"><input type="text" id="confirm-input" placeholder="${word}" autocomplete="off" spellcheck="false" aria-label="Confirmation">
        <button class="btn ${ok.every((p) => p.risk === "safe") ? "primary" : "danger"}" id="exec-btn" disabled>${one ? "Run it" : `Run ${ok.length}`}</button></div>` : ""}
      <div id="exec-result"></div>`);
    if (!ok.length) return;
    const input = $("#confirm-input"), btn = $("#exec-btn");
    input.focus();
    input.addEventListener("input", () => { btn.disabled = !confirmed(input.value, strict); });
    input.addEventListener("keydown", (ev) => { if (ev.key === "Enter" && !btn.disabled) btn.click(); });
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      input.disabled = true;
      let good = 0, freed = 0;
      for (const p of ok) {
        const out = $(`[data-item="${p.id}"] .item-result`);
        out.innerHTML = `<div class="loading small">Running…</div>`;
        try {
          const r = await api(`action/${p.id}/execute`, { method: "POST", body: { confirm: input.value.trim() } });
          out.innerHTML = `<div class="result ok">✓ ${esc(r.message)}</div>`;
          good++; freed += p.bytes;
          S.reclaim.sel.delete(p.id);
        } catch (e) {
          out.innerHTML = `<div class="result bad">✗ ${esc(e.message)}</div>`;
        }
      }
      if (!one) $("#exec-result").innerHTML = `<div class="result ${good === ok.length ? "ok" : "bad"}">${good} of ${ok.length} done, about ${size(freed)} freed. Rescan to refresh the numbers.</div>`;
      if (S.tab === "reclaim") reclaim().catch(showError("reclaim"));
    });
  }

  // ------------------------------------------------------------ Diff
  async function diff() {
    const v = $("#view-diff");
    if (!v.dataset.ready) loading(v);
    await loadSummary();
    const sn = await api("snapshots");
    v.dataset.ready = "1";
    const others = sn.snapshots.filter((s) => !s.current);
    if (!others.length) {
      v.innerHTML = `<div class="card"><h2>Diff</h2><div class="empty">No other snapshots in <code>${esc(sn.dir)}</code> to compare with.<br>Run <code>diskeye scan</code> regularly (e.g. daily) to see what grew.</div></div>`;
      return;
    }
    if (!S.diff.against || !others.some((o) => o.name === S.diff.against)) S.diff.against = others[0].name;
    v.innerHTML = `<div class="card"><div class="toolbar" style="margin:0">
        <label class="ctl">Baseline <select id="diff-against">${others.map((o) => `<option value="${esc(o.name)}" ${o.name === S.diff.against ? "selected" : ""}>${esc(o.name)} · ${esc(o.mtime_human)} · ${size(o.size)}</option>`).join("")}</select></label>
        <label class="ctl">Ignore changes under <input type="text" id="diff-threshold" value="${esc(S.diff.threshold)}" style="width:70px"></label>
        <button class="btn primary" id="diff-go">Compare</button>
        <span class="muted small">current: ${esc(sn.current || "unsaved snapshot")} (${esc(DK.date(sn.current_time))})</span>
      </div></div><div id="diff-out" style="margin-top:14px"></div>`;
    $("#diff-go").addEventListener("click", runDiff);
    $("#diff-against").addEventListener("change", (e) => { S.diff.against = e.target.value; });
    runDiff();
  }

  const divBar = (d, max) => {
    const f = max > 0 ? Math.min(1, Math.abs(d) / max) / 2 : 0;
    return `<div class="divbar"><span class="${d >= 0 ? "up" : "down"}" style="width:${f * 100}%"></span><span class="mid"></span></div>`;
  };
  const deltaCell = (d) => `<td class="num ${d > 0 ? "up-ink" : d < 0 ? "down-ink" : "muted"}">${DK.signed(d)}</td>`;

  async function runDiff() {
    S.diff.threshold = $("#diff-threshold").value.trim() || "100M";
    S.diff.against = $("#diff-against").value;
    const out = $("#diff-out");
    loading(out, "Loading the baseline snapshot and comparing… (a few seconds for large trees)");
    let d;
    try {
      d = await cached(`diff?against=${encodeURIComponent(S.diff.against)}&threshold=${encodeURIComponent(S.diff.threshold)}`);
    } catch (e) { out.innerHTML = `<div class="card result bad">${esc(e.message)}</div>`; return; }
    const fsMax = Math.max(1, ...d.filesystems.map((f) => Math.abs(f.new_used - f.old_used)));
    const hsMax = Math.max(1, ...d.hotspots.map((h) => Math.abs(h.new - h.old)));
    const enMax = Math.max(1, ...d.entities.map((e) => Math.abs(e.new - e.old)));
    const net = d.filesystems.reduce((s, f) => s + (f.new_used - f.old_used), 0);
    const changed = d.filesystems.filter((f) => f.new_used !== f.old_used);
    const unchanged = d.filesystems.length - changed.length;
    out.innerHTML = `
      ${d.baseline_newer ? `<div class="hint">The baseline is newer than the snapshot being served, so growth shows as shrinkage.</div>` : ""}
      <div class="card"><h2>Filesystems <span class="sub">${esc(DK.date(d.old_time))} → ${esc(DK.date(d.new_time))} · net ${DK.signed(net)}</span></h2>
      ${legend([["", "grew", "background:var(--div-up)"], ["", "shrank", "background:var(--div-down)"]])}
      <table class="t"><thead><tr><th>Mount</th><th class="num">Before</th><th class="num">Now</th><th class="num">Change</th><th style="width:30%"></th></tr></thead><tbody>${changed.map((f) => {
        const dl = f.new_used - f.old_used;
        return `<tr><td class="mono">${esc(f.mount_point)}</td><td class="num">${size(f.old_used)}</td><td class="num">${size(f.new_used)}</td>${deltaCell(dl)}<td>${divBar(dl, fsMax)}</td></tr>`;
      }).join("")}</tbody></table>${unchanged ? `<div class="muted small" style="margin-top:6px">${unchanged} filesystem${unchanged > 1 ? "s" : ""} unchanged</div>` : ""}</div>
      <div class="grid two" style="margin-top:14px">
        <div class="card"><h2>Where it changed <span class="sub">most specific paths, ≥ ${size(d.threshold)}</span></h2>
          ${d.hotspots.length ? `<table class="t"><tbody>${d.hotspots.slice(0, 60).map((h) => {
            const dl = h.new - h.old;
            return `<tr>${deltaCell(dl)}<td style="width:24%">${divBar(dl, hsMax)}</td><td class="ellipsis" style="max-width:340px" title="${esc(h.path)}">${filesLink(h.node, h.path)}${h.owner ? ` <span class="badge owner">${esc(h.owner)}</span>` : ""}</td></tr>`;
          }).join("")}</tbody></table>` : `<div class="empty">No path changed by more than ${size(d.threshold)}.</div>`}</div>
        <div class="card"><h2>Workloads <span class="sub">entity size changes</span></h2>
          ${d.entities.length ? `<table class="t"><tbody>${d.entities.slice(0, 60).map((e) => {
            const dl = e.new - e.old;
            return `<tr>${deltaCell(dl)}<td style="width:24%">${divBar(dl, enMax)}</td><td>${esc(e.name)} <span class="muted small">${esc(e.group)} · ${esc(e.kind)}</span>${e.old === 0 ? ' <span class="badge">new</span>' : e.new === 0 ? ' <span class="badge">gone</span>' : ""}</td></tr>`;
          }).join("")}</tbody></table>` : `<div class="empty">No workload changed by more than ${size(d.threshold)}.</div>`}</div>
      </div>`;
  }

  // ------------------------------------------------------------ boot
  document.querySelectorAll(".tabs a").forEach((a) => a.addEventListener("click", (ev) => {
    ev.preventDefault();
    setTab(a.dataset.tab, a.dataset.tab === "files" ? { node: F.id, entity: null } : { node: null, entity: null });
  }));
  window.addEventListener("popstate", route);
  let rt;
  window.addEventListener("resize", () => {
    clearTimeout(rt);
    rt = setTimeout(() => {
      if (S.tab === "files" && F.data) renderFiles();
      if (S.tab === "physical" && S.redrawPhysical) S.redrawPhysical();
    }, 150);
  });
  const mq = window.matchMedia("(prefers-color-scheme: dark)");
  const onTheme = () => {
    if (S.tab === "files" && F.data) renderFiles();
    if (S.tab === "physical" && S.redrawPhysical) S.redrawPhysical();
  };
  if (mq.addEventListener) mq.addEventListener("change", onTheme);

  initToken();
  if (!S.token) {
    banner("error", "No access token. Open the full URL printed by <code>diskeye serve</code> (it ends in <code>#token=…</code>).");
  }
  route();
  if (S.token) pollRescan().catch(console.error);
})();
