/* diskeye charts: formatting, colors, tooltip, treemap, sunburst, icicle.
   Everything hangs off window.DK; app.js wires it to the API. */
(function () {
  "use strict";
  const DK = (window.DK = window.DK || {});

  // ------------------------------------------------------------ formatting
  const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  DK.size = function (b) {
    if (b == null || isNaN(b)) return "–";
    const neg = b < 0;
    let v = Math.abs(b), i = 0;
    while (v >= 1024 && i < UNITS.length - 1) { v /= 1024; i++; }
    const s = i === 0 ? String(Math.round(v)) : v.toFixed(v >= 100 ? 0 : 1);
    return (neg ? "-" : "") + s + " " + UNITS[i];
  };
  DK.signed = (b) => (b > 0 ? "+" : b < 0 ? "−" : "±") + DK.size(Math.abs(b));
  DK.count = (n) => (n == null ? "–" : Number(n).toLocaleString("en-US"));
  DK.pct = (f, digits) => (f == null || !isFinite(f) ? "–" : (f * 100).toFixed(digits == null ? (f < 0.1 ? 1 : 0) : digits) + "%");
  DK.metricFmt = (metric) => (metric === "items" ? (v) => DK.count(v) + " items" : DK.size);
  DK.date = function (secs) {
    if (!secs) return "–";
    const d = new Date(secs * 1000);
    return d.toISOString().slice(0, 16).replace("T", " ");
  };
  DK.age = function (secs) {
    if (!secs) return "–";
    const d = Math.max(0, Date.now() / 1000 - secs);
    if (d < 120) return Math.round(d) + "s ago";
    if (d < 7200) return Math.round(d / 60) + "m ago";
    if (d < 172800) return Math.round(d / 3600) + "h ago";
    if (d < 5184000) return Math.round(d / 86400) + "d ago";
    if (d < 63072000) return Math.round(d / 2592000) + "mo ago";
    return Math.round(d / 31536000) + "y ago";
  };
  const ESC = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
  DK.esc = (s) => String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ESC[c]);

  // ------------------------------------------------------------ colors
  DK.css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  /** Categorical slot for a stable index: 0..7 → --s1..--s8, beyond → "other". */
  DK.slot = (i) => (i == null || i < 0 ? DK.css("--unowned") : i < 8 ? DK.css("--s" + (i + 1)) : DK.css("--other"));
  DK.slotVar = (i) => (i == null || i < 0 ? "var(--unowned)" : i < 8 ? `var(--s${i + 1})` : "var(--other)");
  /** Black or white text for a fill. */
  DK.inkOn = function (fill) {
    const c = d3.color(fill);
    if (!c) return "light";
    const { r, g, b } = c.rgb();
    const lum = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
    return lum > 0.6 ? "dark" : "light";
  };
  DK.mix = function (a, b, t) {
    return d3.interpolateRgb(a, b)(t);
  };
  /** Adds a diagonal hatch pattern (free / folded space) to an svg; returns its url(). */
  DK.hatch = function (svg, id) {
    const p = svg.append("defs").append("pattern").attr("id", id).attr("patternUnits", "userSpaceOnUse")
      .attr("width", 6).attr("height", 6).attr("patternTransform", "rotate(45)");
    p.append("rect").attr("width", 6).attr("height", 6).style("fill", "var(--free)");
    p.append("line").attr("x1", 0).attr("y1", 0).attr("x2", 0).attr("y2", 6).style("stroke", "var(--free-line)").attr("stroke-width", 1.5);
    return `url(#${id})`;
  };

  /** Set an svg text/tspan to `str`, shortened with an ellipsis until it fits `maxW` px. */
  DK.fit = function (node, str, maxW) {
    node.textContent = str;
    if (maxW <= 8) { node.textContent = ""; return false; }
    if (node.getComputedTextLength() <= maxW) return true;
    let lo = 0, hi = str.length;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      node.textContent = str.slice(0, mid) + "…";
      if (node.getComputedTextLength() <= maxW) lo = mid; else hi = mid - 1;
    }
    node.textContent = lo > 0 ? str.slice(0, lo) + "…" : "";
    return lo > 0;
  };

  // ------------------------------------------------------------ tooltip
  const tip = () => document.getElementById("tooltip");
  DK.showTip = function (html, ev) {
    const t = tip();
    t.innerHTML = html;
    t.hidden = false;
    DK.moveTip(ev);
  };
  DK.moveTip = function (ev) {
    const t = tip();
    if (t.hidden || !ev) return;
    const pad = 14, w = t.offsetWidth, h = t.offsetHeight;
    let x = ev.clientX + pad, y = ev.clientY + pad;
    if (x + w > window.innerWidth - 8) x = ev.clientX - w - pad;
    if (y + h > window.innerHeight - 8) y = ev.clientY - h - pad;
    t.style.left = Math.max(4, x) + "px";
    t.style.top = Math.max(4, y) + "px";
  };
  DK.hideTip = () => { tip().hidden = true; };
  DK.tipRows = (rows) =>
    rows.filter((r) => r && r[1] != null && r[1] !== "").map((r) => `<div class="tt-row"><span>${DK.esc(r[0])}</span><span class="num">${DK.esc(r[1])}</span></div>`).join("");

  // ------------------------------------------------------------ hierarchy helpers
  /** d3 hierarchy from an /api/node response; internal nodes keep their remainder as own weight. */
  function hier(data) {
    const root = d3.hierarchy({ ...data, children: data.children }, (d) => (Array.isArray(d.children) && d.children.length ? d.children : null));
    root.sum((d) => {
      const v = Math.max(0, d.value || 0);
      if (Array.isArray(d.children) && d.children.length) {
        const kids = d.children.reduce((s, c) => s + Math.max(0, c.value || 0), 0);
        return Math.max(0, v - kids);
      }
      return v;
    });
    root.sort((a, b) => b.value - a.value);
    return root;
  }

  // ------------------------------------------------------------ treemap
  /**
   * opts: { color(row, d) -> css color, onOpen(row, d), onHover(row, d, ev), onLeave(), fmt(v), dim() }
   * Returns { zoomTo(id) -> Promise } so the caller can animate before re-rendering.
   */
  DK.treemap = function (el, data, opts) {
    el.innerHTML = "";
    const w = el.clientWidth, h = el.clientHeight;
    const root = hier(data);
    d3.treemap().size([w, h]).tile(d3.treemapSquarify.ratio(1.2)).round(true)
      .paddingInner(1).paddingOuter(1)
      .paddingTop((d) => (d.depth === 1 && d.children && d.y1 - d.y0 > 34 && d.x1 - d.x0 > 40 ? 16 : d.depth === 0 ? 0 : 1))(root);
    const svg = d3.select(el).append("svg").attr("width", w).attr("height", h).attr("role", "img")
      .attr("aria-label", "Treemap of " + (data.path || data.name));
    const hatch = DK.hatch(svg, "tm-hatch");
    const fillOf = (d) => (d.data.other ? hatch : opts.color(d.data, d));
    const g = svg.append("g");
    const nodes = root.descendants().filter((d) => d.depth > 0 && d.x1 - d.x0 >= 1 && d.y1 - d.y0 >= 1);
    const node = g.selectAll("g").data(nodes).join("g")
      .attr("class", (d) => "tm-node" + (d.children ? " group" : "") + (d.data.other ? " other" : ""))
      .attr("transform", (d) => `translate(${d.x0},${d.y0})`);
    node.append("rect")
      .attr("width", (d) => d.x1 - d.x0).attr("height", (d) => d.y1 - d.y0)
      .attr("fill", fillOf)
      .attr("fill-opacity", (d) => (d.depth > 1 && !d.data.other ? 0.92 : 1))
      .attr("data-id", (d) => (d.data.id == null ? null : d.data.id))
      .on("mousemove", (ev, d) => opts.onHover(d.data, d, ev))
      .on("mouseleave", () => opts.onLeave())
      .on("click", (ev, d) => {
        ev.stopPropagation();
        opts.onOpen(d.data, d);
      });
    // Labels: group headers, and leaves that are big enough.
    const fmt = opts.fmt;
    node.each(function (d) {
      const tw = d.x1 - d.x0, th = d.y1 - d.y0;
      const header = d.depth === 1 && d.children && th > 34 && tw > 40;
      const leafOk = !d.children && tw > 46 && th > (d.depth === 1 ? 26 : 16);
      if (!header && !leafOk) return;
      const fill = d.data.other ? DK.css("--free") : opts.color(d.data, d);
      const t = d3.select(this).append("text").attr("class", "tile-label " + DK.inkOn(fill))
        .attr("x", 4).attr("y", 12);
      const name = d.data.name || "";
      const sz = fmt(d.data.value);
      const room = tw - 8;
      if (header || th < 28) {
        const n = t.node();
        n.textContent = name + "  " + sz;
        if (n.getComputedTextLength() > room) DK.fit(n, name, room);
      } else {
        DK.fit(t.append("tspan").attr("x", 4).node(), name, room);
        const szn = t.append("tspan").attr("x", 4).attr("dy", 13).attr("class", "sz").text(sz).node();
        if (szn.getComputedTextLength() > room) szn.remove();
      }
    });
    return {
      highlight(id) {
        g.selectAll("rect").classed("hl", (d) => id != null && d.data.id === id);
      },
      zoomTo(id, ms) {
        const d = nodes.find((n) => n.data.id === id && n.depth === 1) || nodes.find((n) => n.data.id === id);
        if (!d) return Promise.resolve();
        const kx = w / Math.max(1, d.x1 - d.x0), ky = h / Math.max(1, d.y1 - d.y0);
        g.selectAll("text").transition().duration(80).style("opacity", 0);
        return g.transition().duration(ms || 320).ease(d3.easeCubicInOut)
          .attr("transform", `translate(${-d.x0 * kx},${-d.y0 * ky}) scale(${kx},${ky})`)
          .end().catch(() => {});
      },
    };
  };

  // ------------------------------------------------------------ sunburst
  DK.sunburst = function (el, data, opts) {
    el.innerHTML = "";
    const w = el.clientWidth, h = el.clientHeight;
    const size = Math.min(w, h);
    const radius = size / 2 - 4;
    const root = hier(data);
    const depth = root.height || 1;
    d3.partition().size([2 * Math.PI, depth + 1])(root);
    const r0 = radius * 0.22; // center hole
    const ring = (radius - r0) / depth;
    const rad = (y) => (y <= 1 ? r0 * y : r0 + (y - 1) * ring);
    const arc = d3.arc().startAngle((d) => d.x0).endAngle((d) => d.x1)
      .padAngle((d) => Math.min((d.x1 - d.x0) / 2, 0.004)).padRadius(radius)
      .innerRadius((d) => rad(d.y0) + 1).outerRadius((d) => rad(d.y1) - 1);
    const svg = d3.select(el).append("svg").attr("width", w).attr("height", h).attr("role", "img")
      .attr("aria-label", "Sunburst of " + (data.path || data.name));
    const g = svg.append("g").attr("transform", `translate(${w / 2},${h / 2})`);
    const nodes = root.descendants().filter((d) => d.depth > 0 && d.x1 - d.x0 > 0.002);
    g.selectAll("path").data(nodes).join("path").attr("class", "sb-arc").attr("d", arc)
      .attr("fill", (d) => opts.color(d.data, d))
      .attr("fill-opacity", (d) => (d.depth === 1 ? 1 : d.depth === 2 ? 0.85 : 0.7))
      .on("mousemove", (ev, d) => opts.onHover(d.data, d, ev))
      .on("mouseleave", () => opts.onLeave())
      .on("click", (ev, d) => opts.onOpen(d.data, d));
    // Labels on wide-enough first/second ring arcs.
    g.selectAll("text.lab").data(nodes.filter((d) => d.depth <= 2 && (d.x1 - d.x0) * rad((d.y0 + d.y1) / 2) > 34 && ring > 30))
      .join("text").attr("class", (d) => "tile-label " + DK.inkOn(opts.color(d.data, d)))
      .attr("transform", (d) => {
        const a = ((d.x0 + d.x1) / 2) * 180 / Math.PI;
        const r = rad((d.y0 + d.y1) / 2);
        return `rotate(${a - 90}) translate(${r},0) rotate(${a < 180 ? 0 : 180})`;
      })
      .attr("dy", "0.35em").attr("text-anchor", "middle")
      .each(function (d) { DK.fit(this, d.data.name || "", ring - 8); });
    const center = g.append("g").attr("class", "sb-center").on("click", () => opts.onUp && opts.onUp());
    center.append("circle").attr("r", r0 - 2).style("fill", "var(--surface-2)");
    center.append("text").attr("text-anchor", "middle").attr("dy", "-0.2em").style("fill", "var(--ink)")
      .style("font-weight", 650).style("font-size", "13px").text(opts.fmt(data.value));
    center.append("text").attr("text-anchor", "middle").attr("dy", "1.2em").style("fill", "var(--muted)")
      .style("font-size", "11px").text(opts.canUp ? "↑ up" : (data.name || ""));
    return {
      highlight(id) {
        g.selectAll("path").classed("hl", (d) => id != null && d.data.id === id);
      },
      zoomTo() {
        return g.transition().duration(200).style("opacity", 0.3).end().catch(() => {});
      },
    };
  };

  // ------------------------------------------------------------ icicle (physical layout)
  /**
   * rows: views::physical rows (pre-order with depth). Builds a hierarchy, adds
   * used/free segments under leaf filesystems, and draws a top-down icicle.
   */
  DK.icicle = function (el, rows, opts) {
    el.innerHTML = "";
    const root = { label: "all", kind: "root", size: 0, children: [] };
    const stack = [root];
    rows.forEach((r, i) => {
      const n = { ...r, idx: i, children: [] };
      while (stack.length > r.depth + 1) stack.pop();
      (stack[stack.length - 1] || root).children.push(n);
      stack.push(n);
    });
    // Synthetic used/free split for leaves that carry filesystem usage.
    (function addUsage(n) {
      n.children.forEach(addUsage);
      if (!n.children.length && n.used != null && n.avail != null && n.kind !== "free") {
        const tot = n.used + n.avail;
        if (tot > 0) {
          n.children.push({ kind: "fsused", label: "used", size: n.size * (n.used / tot), used: n.used, parentLabel: n.label, children: [], mount: n.mount });
          n.children.push({ kind: "fsfree", label: "free", size: n.size * (n.avail / tot), avail: n.avail, parentLabel: n.label, children: [], mount: n.mount });
        }
      }
    })(root);
    root.size = root.children.reduce((s, c) => s + c.size, 0);
    let maxDepth = 0;
    const flat = [];
    (function layout(n, x0, x1, depth) {
      n.x0 = x0; n.x1 = x1; n.d = depth;
      if (depth > 0) flat.push(n);
      maxDepth = Math.max(maxDepth, depth);
      const tot = Math.max(n.size, n.children.reduce((s, c) => s + c.size, 0)) || 1;
      let x = x0;
      n.children.forEach((c) => {
        const cw = ((x1 - x0) * c.size) / tot;
        layout(c, x, x + cw, depth + 1);
        x += cw;
      });
    })(root, 0, 1, 0);
    const w = el.clientWidth, band = 34, gap = 3;
    const h = maxDepth * (band + gap);
    const svg = d3.select(el).append("svg").attr("width", w).attr("height", h).attr("role", "img")
      .attr("aria-label", "Physical storage layout: disks, partitions, volume groups, logical volumes and filesystems");
    const hatch = DK.hatch(svg, "ic-hatch");
    const fills = (n) => (n.kind === "free" || n.kind === "fsfree" ? hatch : opts.fills(n));
    const node = svg.selectAll("g").data(flat.filter((n) => (n.x1 - n.x0) * w >= 0.5)).join("g")
      .attr("transform", (n) => `translate(${n.x0 * w},${(n.d - 1) * (band + gap)})`);
    node.append("rect").attr("class", (n) => "ic-rect" + (n.flag ? " flag" : ""))
      .attr("width", (n) => Math.max(0.5, (n.x1 - n.x0) * w - 1)).attr("height", band).attr("rx", 3)
      .attr("fill", (n) => fills(n))
      .on("mousemove", (ev, n) => opts.onHover(n, ev))
      .on("mouseleave", () => DK.hideTip())
      .on("click", (ev, n) => opts.onClick && opts.onClick(n));
    node.each(function (n) {
      const pw = (n.x1 - n.x0) * w;
      if (pw < 44) return;
      const free = n.kind === "free" || n.kind === "fsfree";
      const fill = free ? DK.css("--free") : fills(n);
      const t = d3.select(this).append("text").attr("class", "ic-label " + (free ? "free" : DK.inkOn(fill))).attr("x", 6).attr("y", 14);
      const name = n.kind === "fsused" ? "used" : n.kind === "fsfree" ? "free" : n.label;
      DK.fit(t.append("tspan").node(), name, pw - 10);
      if (pw > 70) t.append("tspan").attr("x", 6).attr("dy", 13).style("opacity", 0.85).text(DK.size(n.kind === "fsused" ? n.used : n.kind === "fsfree" ? n.avail : n.size));
    });
    return flat;
  };
})();
