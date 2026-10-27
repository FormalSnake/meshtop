"use strict";

// The TUI's 3D mesh (ui/map.rs) on a canvas: hosts stand on a floor circle at
// a height set by their CPU, rings fill with memory, links bow apart per
// direction and carry one packet per kind.

const FLOOR = -0.55, HEIGHT = 1.1, DIST = 4.0, PITCH_MAX = 1.4;
const KIND_COLORS = { "nix build": "--magenta", ssh: "--blue", http: "--cyan", syncthing: "--teal", sunshine: "--purple" };

const floorAngle = (i, n) => (2 * Math.PI * i) / Math.max(1, n);
const floorCoords = (i, n) => (n === 1 ? [0, 0] : [Math.sin(floorAngle(i, n)), Math.cos(floorAngle(i, n))]);

live.map = () => {
  const canvas = h("canvas", { class: "map-canvas", role: "img", "aria-label": "Mesh map" });
  const summary = h("p", { class: "muted mono" });
  const inspector = h("aside", { class: "map-inspector", "aria-live": "polite" });
  const links = h("div", { class: "scroll" });
  const el = h("div", { class: "map" },
    h("div", { class: "bar" }, summary, h("p", { class: "muted map-hint" }, "drag to turn · wheel or pinch to zoom · tap a host")),
    h("div", { class: "map-stage" }, canvas, inspector),
    h("h2", { class: "map-links-h" }, "Links"), links);

  const cam = { yaw: 0, pitch: 0.6, zoom: 1, target: null, touched: -Infinity };
  let data = null, selected = 0, palette = {}, raf = 0, last = performance.now(), clock = 0;
  // When each host's latest cpu sample arrived, for the half-second glide.
  const arrived = new Map();
  let hits = [];

  const readPalette = () => {
    const cs = getComputedStyle(document.documentElement);
    palette = new Proxy({}, { get: (_, k) => cs.getPropertyValue(k).trim() });
  };
  readPalette();
  const scheme = matchMedia("(prefers-color-scheme: light)");
  scheme.addEventListener("change", readPalette);

  const hostCol = (i) => palette[HOST_COLORS[i % HOST_COLORS.length]];
  const kindCol = (k) => palette[KIND_COLORS[k] || "--tx"];

  function load(i, now) {
    const host = data.hosts[i];
    if (!host.live) return 0;
    const cpu = host.history.cpu;
    if (!cpu.length) return 0;
    const cur = cpu.at(-1), prev = cpu.length > 1 ? cpu.at(-2) : cur;
    const t = Math.min(1, (now - (arrived.get(host.name) || 0)) / 500);
    return prev + (cur - prev) * (1 - (1 - t) ** 3);
  }

  function scene(w, hgt) {
    const r = Math.max(40, Math.min(hgt * 0.36, (w - 40) / 2.6)) * cam.zoom;
    const sy = Math.sin(cam.yaw), cy = Math.cos(cam.yaw), sp = Math.sin(cam.pitch), cp = Math.cos(cam.pitch);
    const rotate = ([x, y, z]) => {
      const rx = x * cy - z * sy, rz = x * sy + z * cy;
      return [rx, y * cp - rz * sp, y * sp + rz * cp];
    };
    const project = ([x, y, z]) => {
      const s = DIST / (DIST - z);
      return [w / 2 + x * s * r, hgt / 2 - y * s * r, s];
    };
    return { r, rotate, project, pt: (p) => project(rotate(p)) };
  }

  function frame(now) {
    raf = requestAnimationFrame(frame);
    const dt = Math.min(0.1, (now - last) / 1000);
    last = now;
    clock += dt;
    if (cam.target) {
      const k = 1 - Math.exp(-5 * dt);
      const dy = ((cam.target.yaw - cam.yaw + Math.PI) % (2 * Math.PI) + 2 * Math.PI) % (2 * Math.PI) - Math.PI;
      cam.yaw += dy * k;
      cam.pitch += (cam.target.pitch - cam.pitch) * k;
      if (Math.abs(dy) < 0.002 && Math.abs(cam.target.pitch - cam.pitch) < 0.002) cam.target = null;
    } else if (now - cam.touched > 6000 && !matchMedia("(prefers-reduced-motion: reduce)").matches) {
      cam.yaw += dt * 0.18;
    }
    paint(now);
  }

  function paint(now) {
    const dpr = devicePixelRatio || 1;
    const w = canvas.clientWidth, hgt = canvas.clientHeight;
    if (!w || !hgt) return;
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(hgt * dpr)) {
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(hgt * dpr);
    }
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, hgt);
    if (!data) return;
    const sc = scene(w, hgt);
    const n = data.hosts.length;

    // Floor grid, dotted, so height and tilt read against something.
    ctx.fillStyle = palette["--tx-3"];
    const E = 1.35;
    for (let a = 0; a < 7; a++) {
      const u = -E + (2 * E * a) / 6;
      for (let b = 0; b <= 40; b++) {
        const v = -E + (2 * E * b) / 40;
        for (const [x, z] of [[u, v], [v, u]]) {
          const [px, py, s] = sc.pt([x, FLOOR, z]);
          ctx.fillRect(px - 0.6 * s, py - 0.6 * s, 1.2 * s, 1.2 * s);
        }
      }
    }

    const world = data.hosts.map((_, i) => {
      const [x, z] = floorCoords(i, n);
      return { top: sc.rotate([x, FLOOR + (load(i, now) / 100) * HEIGHT, z]), base: sc.rotate([x, FLOOR, z]) };
    });
    const proj = world.map((p) => ({ top: sc.project(p.top), base: sc.project(p.base), z: p.top[2] }));

    // Stems from the floor shadow up to each node.
    data.hosts.forEach((host, i) => {
      const { top, base } = proj[i];
      ctx.strokeStyle = palette["--tx-3"];
      ctx.lineWidth = 1;
      ctx.setLineDash(host.live ? [] : [2, 3]);
      ctx.beginPath();
      ctx.moveTo(base[0], base[1]);
      ctx.lineTo(top[0], top[1]);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = palette["--tx-3"];
      ctx.beginPath();
      ctx.ellipse(base[0], base[1], 6 * base[2], 2.5 * base[2], 0, 0, 2 * Math.PI);
      ctx.fill();
    });

    // Links, bowed sideways so the two directions of a pair separate.
    const has = new Set(data.edges.map((e) => e.client + ":" + e.server));
    const curves = data.edges.map((e) => {
      const a = world[e.client].top, b = world[e.server].top;
      const bow = e.client > e.server && has.has(e.server + ":" + e.client) ? -0.12 : 0.12;
      const d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
      const len = Math.hypot(...d);
      // Sideways in the view plane, perpendicular to the edge.
      const side = [-d[1], d[0], 0], sl = Math.hypot(...side) || 1;
      const at = (t) => {
        const off = bow * len * Math.sin(Math.PI * t);
        return sc.project([a[0] + d[0] * t + (side[0] / sl) * off, a[1] + d[1] * t + (side[1] / sl) * off, a[2] + d[2] * t]);
      };
      const kinds = Object.entries(e.kinds).sort((x, y) => y[1] - x[1]);
      return { e, len, at, kinds, selected: e.client === selected || e.server === selected };
    });
    for (const c of curves) {
      ctx.strokeStyle = kindCol(c.kinds[0]?.[0]);
      ctx.globalAlpha = c.selected ? 0.9 : 0.45;
      ctx.lineWidth = c.selected ? 2 : 1.25;
      ctx.beginPath();
      for (let t = 0; t <= 1.0001; t += 0.04) {
        const [x, y] = c.at(t);
        t ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
      }
      ctx.stroke();
    }
    ctx.globalAlpha = 1;

    // Nodes back to front.
    const order = [...data.hosts.keys()].sort((a, b) => proj[a].z - proj[b].z);
    hits = [];
    for (const i of order) {
      const host = data.hosts[i], snap = host.snap;
      const [x, y, s] = proj[i].top;
      const rr = Math.max(9, 15 * s * Math.sqrt(cam.zoom));
      const col = hostCol(i);
      const cpu = host.live && snap ? snap.cpu : 0;
      const memP = host.live && snap ? pct(snap.memUsed, snap.memTotal) : 0;
      ctx.fillStyle = palette["--bg"];
      ctx.beginPath();
      ctx.arc(x, y, rr, 0, 2 * Math.PI);
      ctx.fill();
      ctx.lineWidth = 2;
      ctx.strokeStyle = palette["--ui-2"];
      ctx.stroke();
      if (memP) {
        ctx.strokeStyle = col;
        ctx.beginPath();
        ctx.arc(x, y, rr, -Math.PI / 2, -Math.PI / 2 + (2 * Math.PI * memP) / 100);
        ctx.stroke();
      }
      ctx.fillStyle = !host.live ? palette["--tx-3"] : cpu >= 90 ? palette["--red"] : cpu >= 75 ? palette["--yellow"] : col;
      ctx.beginPath();
      ctx.arc(x, y, rr * (i === selected ? 0.5 : 0.4), 0, 2 * Math.PI);
      host.live ? ctx.fill() : (ctx.lineWidth = 1.5, ctx.strokeStyle = ctx.fillStyle, ctx.stroke());
      if (i === selected) {
        const ph = (clock / 2.4) % 1;
        ctx.globalAlpha = 1 - ph;
        ctx.strokeStyle = col;
        ctx.lineWidth = 1.5;
        ctx.beginPath();
        ctx.arc(x, y, rr * (1 + ph * 1.2), 0, 2 * Math.PI);
        ctx.stroke();
        ctx.globalAlpha = 1;
      }
      hits.push({ i, x, y, r: rr + 10 });
    }

    // Packets, one per kind, the same speed on long and short edges.
    for (const c of curves) {
      const speed = 0.35 / Math.max(0.3, c.len);
      c.kinds.forEach(([kind], k) => {
        const t = 0.1 + 0.8 * ((clock * speed + k / c.kinds.length) % 1);
        ctx.fillStyle = kindCol(kind);
        for (let j = 0; j <= 4; j++) {
          const [px, py] = c.at(t - j * 0.015);
          ctx.globalAlpha = 1 - j * 0.2;
          ctx.beginPath();
          ctx.arc(px, py, j ? 1.6 : 2.6, 0, 2 * Math.PI);
          ctx.fill();
        }
      });
      ctx.globalAlpha = 1;
    }

    // Labels only on links that touch the selected host.
    ctx.font = `11px ${palette["--mono"]}`;
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    for (const c of curves.filter((c) => c.selected)) {
      const [mx, my] = c.at(0.5), [ax, ay] = c.at(0.45), [bx, by] = c.at(0.55);
      const arrow = Math.abs(bx - ax) > Math.abs(by - ay) ? (bx > ax ? "→" : "←") : by > ay ? "↓" : "↑";
      const text = c.kinds.map(([k]) => k).join(" ") + " " + arrow;
      pill(ctx, text, mx, my, palette["--bg-2"], kindCol(c.kinds[0]?.[0]));
    }

    // Name above each ring, readings below.
    for (const i of order) {
      const host = data.hosts[i], snap = host.snap;
      const [x, y, s] = proj[i].top;
      const rr = Math.max(9, 15 * s * Math.sqrt(cam.zoom));
      ctx.font = `600 12px ${palette["--mono"]}`;
      pill(ctx, host.name + (host.role ? "  " + host.role : ""), x, y - rr - 13, i === selected ? palette["--ui-2"] : palette["--bg-2"], hostCol(i));
      ctx.font = `11px ${palette["--mono"]}`;
      ctx.fillStyle = palette["--tx-2"];
      if (!host.live) {
        ctx.fillText(host.status === "down" ? "down" : host.status, x, y + rr + 12);
        continue;
      }
      if (!snap) continue;
      const gpu = snap.gpus?.[0];
      ctx.fillText(`cpu ${Math.round(snap.cpu)}%  mem ${pct(snap.memUsed, snap.memTotal)}%` + (gpu ? `  gpu ${Math.round(gpu.util)}%` : ""), x, y + rr + 12);
      const agents = snap.agents || [];
      if (agents.length) {
        const working = agents.filter((a) => a.status === "working").length;
        const blocked = agents.filter((a) => a.status === "blocked").length;
        ctx.fillText(`agents ${working}/${agents.length} working`, x, y + rr + 26);
        if (blocked) {
          ctx.fillStyle = palette["--yellow"];
          ctx.fillText(`▲ ${blocked} blocked`, x, y + rr + 40);
        }
      }
    }
  }

  function pill(ctx, text, x, y, bg, fg) {
    const w = ctx.measureText(text).width + 12;
    ctx.fillStyle = bg;
    ctx.beginPath();
    ctx.roundRect(x - w / 2, y - 9, w, 18, 5);
    ctx.fill();
    ctx.fillStyle = fg;
    ctx.fillText(text, x, y + 0.5);
  }

  function select(i) {
    if (!data || i < 0 || i >= data.hosts.length) return;
    selected = i;
    cam.target = { yaw: floorAngle(i, data.hosts.length), pitch: cam.pitch };
    drawSide();
  }

  // Pointer input: one finger or mouse turns, two fingers pinch, a tap selects.
  const pointers = new Map();
  let pinch = 0, moved = 0;
  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    pointers.set(e.pointerId, [e.clientX, e.clientY]);
    moved = 0;
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      pinch = Math.hypot(a[0] - b[0], a[1] - b[1]);
    }
  });
  canvas.addEventListener("pointermove", (e) => {
    const prev = pointers.get(e.pointerId);
    if (!prev) return;
    const dx = e.clientX - prev[0], dy = e.clientY - prev[1];
    pointers.set(e.pointerId, [e.clientX, e.clientY]);
    moved += Math.abs(dx) + Math.abs(dy);
    cam.touched = performance.now();
    cam.target = null;
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      const d = Math.hypot(a[0] - b[0], a[1] - b[1]);
      if (pinch) cam.zoom = Math.min(2.5, Math.max(0.5, cam.zoom * (d / pinch)));
      pinch = d;
    } else {
      cam.yaw += dx * 0.01;
      cam.pitch = Math.min(PITCH_MAX, Math.max(-0.2, cam.pitch + dy * 0.01));
    }
  });
  const up = (e) => {
    if (pointers.size === 1 && moved < 6) {
      const r = canvas.getBoundingClientRect();
      const x = e.clientX - r.left, y = e.clientY - r.top;
      const hit = hits.filter((t) => Math.hypot(t.x - x, t.y - y) < t.r).at(-1);
      if (hit) select(hit.i);
    }
    pointers.delete(e.pointerId);
    if (pointers.size < 2) pinch = 0;
  };
  canvas.addEventListener("pointerup", up);
  canvas.addEventListener("pointercancel", (e) => { pointers.delete(e.pointerId); pinch = 0; });
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    cam.touched = performance.now();
    cam.zoom = Math.min(2.5, Math.max(0.5, cam.zoom * Math.exp(-e.deltaY * 0.0015)));
  }, { passive: false });
  canvas.addEventListener("dblclick", () => {
    const sel = data?.hosts[selected];
    if (sel) location.hash = "#host/" + encodeURIComponent(sel.name);
  });

  function drawSide() {
    if (!data) return;
    const host = data.hosts[selected], snap = host?.snap;
    if (!host) return inspector.replaceChildren();
    const row = (label, value, values, max, cls) =>
      h("div", { class: "insp-row" }, h("span", { class: "muted" }, label), h("span", { class: "mono " + (cls || "") }, value), values ? spark(values, max, cls) : null);
    const out = data.edges.filter((e) => e.client === selected), inn = data.edges.filter((e) => e.server === selected);
    const peer = (dir, e, i) => {
      const name = h("span", {}, data.hosts[i].name);
      name.style.color = hostColor(i);
      return h("li", { class: "mono" }, dir + " ", name, " ", h("span", { class: "muted" }, Object.keys(e.kinds).join(" ")));
    };
    const name = h("h2", { class: "mono" }, host.name);
    name.style.color = hostColor(selected);
    const ext = data.external.filter((i) => i === selected).length;
    const agents = (snap?.agents || []).slice().sort((a, b) => agentMeta(a.status).rank - agentMeta(b.status).rank);
    inspector.replaceChildren(...[
      h("div", { class: "insp-head" }, name, host.role ? h("span", { class: "muted" }, host.role) : null,
        h("a", { href: "#host/" + encodeURIComponent(host.name), class: "insp-open" }, "Details")),
      snap
        ? h("p", { class: "muted mono insp-os" }, `${snap.os} · ${snap.arch} · up ${span(snap.uptime * 1000)}`, h("br"), snap.cpuBrand)
        : h("p", { class: "muted" }, host.reason || host.status),
      snap && host.live ? [
        row("cpu", Math.round(snap.cpu) + "%", host.history.cpu, 100, loadClass(snap.cpu)),
        row("mem", `${bytes(snap.memUsed)} / ${bytes(snap.memTotal)}`, host.history.mem, 100),
        snap.gpus?.length ? row("gpu", Math.round(snap.gpus[0].util) + "%", host.history.gpu, 100) : null,
        row("net", `↓${rate(snap.rx)} ↑${rate(snap.tx)}`, host.history.rx.map((v, j) => v + (host.history.tx[j] || 0))),
      ] : null,
      out.length || inn.length || ext
        ? h("div", { class: "insp-links" }, h("h3", {}, "Links"),
          h("ul", {}, out.map((e) => peer("→", e, e.server)), inn.map((e) => peer("←", e, e.client))),
          ext ? h("p", { class: "muted" }, `${ext} external`) : null)
        : null,
      agents.length
        ? h("div", { class: "insp-agents" }, h("h3", {}, "Agents"),
          h("ul", {}, agents.map((a) => h("li", { class: "agent " + a.status }, h("span", { class: "sym" }, agentMeta(a.status).sym), " ", h("span", { class: "mono" }, project(a.cwd) || a.kind), " ", h("span", { class: "muted" }, a.title || a.name || "")))))
        : null,
    ].flat().filter(Boolean));
  }

  function drawLinks() {
    const groups = new Map();
    for (const l of data.links) {
      const k = [l.client, l.server, l.kind, l.port, l.process].join("\u0000");
      const g = groups.get(k);
      g ? g.n++ : groups.set(k, { ...l, n: 1 });
    }
    const rows = [...groups.values()].sort((a, b) => a.client - b.client || a.server - b.server || b.n - a.n).map((l) => {
      const from = h("td", { class: "mono" }, data.hosts[l.client].name), to = h("td", { class: "mono" }, data.hosts[l.server].name);
      from.style.color = hostColor(l.client);
      to.style.color = hostColor(l.server);
      const kind = h("td", { class: "mono" }, l.kind);
      kind.style.color = `var(${KIND_COLORS[l.kind] || "--tx"})`;
      return h("tr", {}, from, to, kind, h("td", { class: "num" }, l.port), h("td", { class: "mono opt" }, l.process || ""), h("td", { class: "num" }, l.n));
    });
    links.replaceChildren(table([["From"], ["To"], ["Kind"], ["Port"], ["Process", "opt"], ["Conns"]], rows, "No links between the hosts right now."));
  }

  raf = requestAnimationFrame(frame);
  return {
    el,
    update(d) {
      const now = performance.now();
      for (const host of d.hosts) {
        const old = data?.hosts.find((o) => o.name === host.name);
        if (!old || old.history.cpu.length !== host.history.cpu.length || old.history.cpu.at(-1) !== host.history.cpu.at(-1)) arrived.set(host.name, now);
      }
      const first = !data;
      data = d;
      if (selected >= d.hosts.length) selected = 0;
      if (first) {
        const local = d.hosts.findIndex((x) => x.local);
        selected = local < 0 ? 0 : local;
        cam.yaw = floorAngle(selected, d.hosts.length);
      }
      const up = d.hosts.filter((x) => x.live).length;
      const working = d.hosts.reduce((t, x) => t + (x.snap?.agents || []).filter((a) => a.status === "working").length, 0);
      summary.textContent = `${up}/${d.hosts.length} up · ${d.links.length} links · ${working} agents working`;
      drawSide();
      drawLinks();
    },
    stop() {
      cancelAnimationFrame(raf);
      scheme.removeEventListener("change", readPalette);
    },
  };
};
