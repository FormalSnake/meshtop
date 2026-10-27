"use strict";

// Every write carries X-Meshtop: the server refuses writes without it, and a
// cross-site form cannot set it.
async function api(path, body) {
  const opts = body === undefined
    ? {}
    : { method: "POST", headers: { "Content-Type": "application/json", "X-Meshtop": "1" }, body: JSON.stringify(body) };
  const r = await fetch("/api/" + path, opts);
  const data = await r.json().catch(() => ({ error: r.statusText }));
  if (!r.ok) throw new Error(data.error || r.statusText);
  return data;
}

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const k of kids.flat()) if (k != null && k !== false) el.append(k instanceof Node ? k : String(k));
  return el;
}

function span(ms) {
  const s = Math.floor(ms / 1000);
  if (s < 60) return s + "s";
  if (s < 3600) return Math.floor(s / 60) + "m";
  if (s < 86400) return Math.floor(s / 3600) + "h";
  return Math.floor(s / 86400) + "d";
}
const ago = (at) => (at ? span(Date.now() - at) + " ago" : "never");
const word = (s) => String(s).replaceAll("_", " ");

let toastTimer;
function toast(msg, err) {
  const t = document.getElementById("toast");
  t.textContent = msg;
  t.className = "toast show" + (err ? " err" : "");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.className = "toast"), err ? 6000 : 2500);
}

// Runs a write, says what happened, and redraws.
async function act(path, body, done) {
  try {
    await api(path, body ?? {});
    toast(done);
    await draw();
    await drawHosts(true);
  } catch (e) {
    toast(e.message, true);
  }
}

function showLog(title, text) {
  document.getElementById("log-title").textContent = title;
  document.getElementById("log-body").textContent = text || "(empty)";
  document.getElementById("log").showModal();
}

async function logOf(path, title) {
  try {
    showLog(title, (await api(path)).log);
  } catch (e) {
    toast(e.message, true);
  }
}

function table(head, rows, empty) {
  if (!rows.length) return h("p", { class: "empty" }, empty);
  return h("div", { class: "scroll" },
    h("table", {},
      h("thead", {}, h("tr", {}, head.map(([label, cls]) => h("th", { class: cls }, label)))),
      h("tbody", {}, rows)));
}

const state = (s, label = word(s)) => h("span", { class: "state " + s }, label);

// ---- live ----------------------------------------------------------------

// Live tabs mount once and then redraw from /api/live about once a second,
// only while they are showing. live.js and map.js register into `live`: each
// entry takes the hash argument and returns { el, update(data), host?, stop? }.
const live = {};
let liveView = null, liveTimer = 0;

async function pollLive() {
  clearTimeout(liveTimer);
  const v = liveView;
  if (!v) return;
  if (document.visibilityState === "visible") {
    try {
      const d = await api("live" + (v.host ? "?host=" + encodeURIComponent(v.host) : ""));
      if (v === liveView) v.update(d);
    } catch (e) {
      if (v === liveView) toast("Live readings did not load: " + e.message, true);
    }
  }
  if (v === liveView) liveTimer = setTimeout(pollLive, 1000);
}

const pct = (used, total) => (total ? Math.min(100, Math.round((used / total) * 100)) : 0);

function bytes(n) {
  const u = ["B", "K", "M", "G", "T"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i && n < 10 ? n.toFixed(1) : Math.round(n)) + u[i];
}
const rate = (n) => bytes(n) + "/s";

// Same order as the TUI: no green, yellow or red, which mean load.
const HOST_COLORS = ["--cyan", "--magenta", "--blue", "--purple", "--orange", "--teal"];
const hostColor = (i) => `var(${HOST_COLORS[i % HOST_COLORS.length]})`;
const loadClass = (p) => (p >= 90 ? "hot" : p >= 75 ? "warm" : "");

// A sparkline of the last samples, scaled to `max` (or the largest sample).
function spark(values, max, cls = "") {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("class", "spark " + cls);
  svg.setAttribute("viewBox", "0 0 100 24");
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("aria-hidden", "true");
  const vs = values.slice(-60);
  const top = max || Math.max(1, ...vs);
  if (vs.length > 1) {
    const pts = vs.map((v, i) => `${((i / (vs.length - 1)) * 100).toFixed(1)},${(23 - (Math.min(v, top) / top) * 22).toFixed(1)}`);
    const area = document.createElementNS(ns, "polygon");
    area.setAttribute("points", `0,24 ${pts.join(" ")} 100,24`);
    const line = document.createElementNS(ns, "polyline");
    line.setAttribute("points", pts.join(" "));
    svg.append(area, line);
  }
  return svg;
}

// Agent states, most urgent first, as the TUI ranks them.
const AGENT = {
  blocked: { sym: "▲", label: "needs input", rank: 0 },
  working: { sym: "●", label: "working", rank: 1 },
  done: { sym: "✓", label: "done", rank: 2 },
  idle: { sym: "○", label: "idle", rank: 3 },
};
const agentMeta = (s) => AGENT[s] || { sym: "?", label: "unknown", rank: 4 };

function project(cwd) {
  const p = (cwd || "").split("/").filter(Boolean);
  const last = p.at(-1) || "";
  return ["apps", "packages", "crates", "nix"].includes(p.at(-2)) ? p.at(-3) + "/" + last : last;
}

// ---- hosts ---------------------------------------------------------------

const FRESH_MS = 30000;

function hostCard(s) {
  const fresh = s.fresh && s.reach === "ready";
  const now = fresh
    ? [h("strong", {}, s.os), h("span", { class: "muted" }, "via " + s.via)]
    : [h("strong", {}, word(s.reach))];
  const age = h("div", { class: "age", "aria-hidden": "true" }, h("span"));
  if (s.observed) age.firstChild.style.transform = `scaleX(${Math.max(0, 1 - (Date.now() - s.observed) / FRESH_MS)})`;
  const last = s.last_known && !fresh ? `last ready on ${s.last_known.os}, ${ago(s.last_known.at)}` : null;
  return h("article", { class: "host" + (fresh ? "" : " stale"), "aria-label": s.name },
    h("div", { class: "host-head" }, h("span", { class: "reach " + s.reach, title: word(s.reach) }), h("h3", {}, s.name),
      s.local ? h("span", { class: "tag" }, "this host") : null),
    s.description ? h("p", { class: "host-desc" }, s.description) : null,
    h("div", { class: "now" }, now, h("span", { class: "muted" }, s.observed ? "seen " + ago(s.observed) : "not observed")),
    age,
    s.boot_changed ? h("p", { class: "detail" }, `rebooted ${ago(s.boot_changed)}`) : null,
    !fresh && s.detail ? h("p", { class: "detail" }, s.detail) : null,
    last ? h("p", { class: "detail" }, last) : null,
    h("div", { class: "tags", "aria-label": "Roles" },
      s.roles.map((r) => h("span", { class: "tag" + (s.eligible.includes(r) ? " on" : ""), title: s.eligible.includes(r) ? "can take this work now" : "not now" }, r))),
    s.hold ? h("p", { class: "hold" }, `Held: ${s.hold.reason}`) : null,
    h("div", { class: "host-actions" },
      s.hold
        ? h("button", { onclick: () => act(`hosts/${s.name}/release`, {}, `Released ${s.name}`) }, "Release")
        : h("button", { onclick: () => hold(s.name) }, "Hold")));
}

function hold(name) {
  const reason = prompt(`Why hold ${name}? Agents will not pick it while held.`);
  if (!reason) return;
  act(`hosts/${name}/hold`, { reason, hours: 4 }, `Held ${name} for 4 hours`);
}

async function drawHosts(force) {
  try {
    const d = await api(force ? "hosts?max_age=0" : "hosts?max_age=30");
    document.getElementById("hosts").replaceChildren(...d.hosts.map(hostCard));
  } catch (e) {
    document.getElementById("hosts").replaceChildren(h("p", { class: "empty" }, "Hosts did not load: " + e.message));
  }
}

// ---- sections ------------------------------------------------------------

const views = {
  async deliveries() {
    const { deliveries } = await api("deliveries");
    return [
      h("div", { class: "bar" }, h("p", {}, "Builds that wait for their target. Start one with ", h("code", {}, "meshtop deliver start"), ".")),
      table([["Delivery"], ["Phase"], ["Target"], ["Where it stands", "opt"], ["Updated", "opt"], [""]],
        deliveries.map(({ intent: i, progress: p, driver }) => h("tr", {},
          h("td", { class: "mono" }, i.id),
          h("td", {}, state(p.phase)),
          h("td", { class: "mono" }, i.target + (i.os ? ` (${i.os})` : "")),
          h("td", { class: "why opt" }, p.detail, p.artifact ? h("div", { class: "mono" }, p.artifact) : null),
          h("td", { class: "num opt" }, ago(p.updated)),
          h("td", { class: "act" },
            driver ? h("button", { onclick: () => act(`deliveries/${i.id}/cancel`, {}, `Stopped the driver for ${i.id}`) }, "Stop") : null,
            !driver && !["done", "failed", "unknown"].includes(p.phase)
              ? h("button", { onclick: () => act(`deliveries/${i.id}/resume`, {}, `Resumed ${i.id}`) }, "Resume") : null,
            p.phase === "unknown"
              ? h("button", { class: "danger", onclick: () => confirm(`Run the install for ${i.id} again? Check ${i.target} first: the last attempt's outcome is unknown.`) && act(`deliveries/${i.id}/resume`, { retry_install: true }, `Retrying the install for ${i.id}`) }, "Retry install") : null))),
        "No deliveries."),
    ];
  },

  async jobs() {
    const { jobs } = await api("jobs");
    return table([["Job"], ["State"], ["Owner", "opt"], ["Command", "opt"], ["Started", "opt"], [""]],
      jobs.slice(0, 80).map((j) => h("tr", {},
        h("td", { class: "mono" }, j.id),
        h("td", {}, state(j.state), j.outcome?.code != null && j.state !== "succeeded" ? h("span", { class: "muted mono" }, ` exit ${j.outcome.code}`) : null,
          j.quiet_ms > 60000 ? h("div", { class: "muted" }, `quiet ${span(j.quiet_ms)}, still holds its lock`) : null),
        h("td", { class: "opt" }, j.record.owner || "unknown"),
        h("td", { class: "mono why opt" }, j.record.cmd.join(" ")),
        h("td", { class: "num opt" }, ago(j.record.created)),
        h("td", { class: "act" },
          h("button", { onclick: () => logOf(`jobs/${j.id}/logs?lines=400`, j.id) }, "Log"),
          ["running", "starting"].includes(j.state)
            ? h("button", { class: "danger", onclick: () => confirm(`Cancel ${j.id}?`) && act(`jobs/${j.id}/cancel`, {}, `Cancelled ${j.id}`) }, "Cancel") : null))),
      "No jobs on this host.");
  },

  async worktrees() {
    const { worktrees } = await api("worktrees");
    const removable = worktrees.filter((w) => w.verdict !== "keep").length;
    return [
      h("div", { class: "bar" }, h("p", {}, `${worktrees.length} linked worktrees on this host, ${removable} safe to remove. Removal never forces and keeps the branch.`)),
      table([["Worktree"], ["Branch"], ["Owner", "opt"], ["Verdict"], ["Why", "opt"], ["Active", "opt"], [""]],
        worktrees.map((w) => h("tr", {},
          h("td", { class: "mono" }, w.path.replace(/^\/Users\/[^/]+/, "~")),
          h("td", { class: "mono" }, w.branch || "(detached)"),
          h("td", { class: "opt" }, w.owner || "unknown"),
          h("td", {}, state(w.verdict)),
          h("td", { class: "why opt" }, w.reasons.join("; ") || (w.integrated ? "integrated, clean, idle" : "")),
          h("td", { class: "num opt" }, w.last_activity ? ago(w.last_activity) : ""),
          h("td", { class: "act" }, w.verdict !== "keep"
            ? h("button", { class: "danger", onclick: () => confirm(`Remove ${w.path}? The branch stays.`) && act("worktrees/remove", { path: w.path }, `Removed ${w.path}`) }, "Remove") : null))),
        "No linked worktrees."),
    ];
  },

  async services() {
    const { host, services } = await api("services");
    return table([["Service"], ["State"], ["Last exit", "opt"], [""]],
      services.map((s) => h("tr", {},
        h("td", { class: "mono" }, s.name, h("div", { class: "muted" }, s.at ? `${s.unit} on ${s.at}` : s.unit)),
        h("td", {}, s.running ? state("running-ok", "running") : state("stopped"), s.pid ? h("span", { class: "muted mono" }, ` pid ${s.pid}`) : null,
          !s.running && s.detail ? h("div", { class: "muted" }, s.detail) : null),
        h("td", { class: "mono opt" }, s.last_exit || ""),
        h("td", { class: "act" },
          h("button", { onclick: () => logOf(`services/${s.name}/logs?lines=300`, s.name) }, "Log"),
          s.restart ? h("button", { onclick: () => confirm(`Restart ${s.name}?`) && act(`services/${s.name}/restart`, {}, `Restarted ${s.name}`) }, "Restart") : null))),
      `No services registered for ${host}.`);
  },

  async skills() {
    const { catalog, targets, generations } = await api("skills");
    const preview = async () => {
      try {
        const g = await api("skills/sync", { dry_run: true });
        showLog("Sync preview", g.changes.length ? g.changes.map((c) => `${c.target}  ${c.action}  ${c.name}`).join("\n") : "Every target matches the registry.");
      } catch (e) {
        toast(e.message, true);
      }
    };
    const off = (t) => t.entries.filter((e) => e.state !== "current" && e.state !== "excluded");
    return [
      h("div", { class: "bar" },
        h("p", {}, `${catalog.length} skills in the registry. Last sync ${generations[0] ? generations[0].id : "never"}.`),
        h("button", { onclick: preview }, "Preview sync"),
        h("button", { onclick: () => confirm("Sync every target now? Edited copies are kept, replaced files are backed up.") && act("skills/sync", { dry_run: false }, "Synced") }, "Sync")),
      ...targets.map((t) => h("section", {},
        h("h2", {}, t.target, h("span", { class: "muted mono" }, "  " + t.dir)),
        table([["Skill"], ["State"], ["Detail", "opt"]],
          off(t).map((e) => h("tr", {}, h("td", { class: "mono" }, e.name), h("td", {}, state(e.state)), h("td", { class: "why opt" }, e.detail || ""))),
          `All ${t.entries.length} skills current.`))),
    ];
  },

  async memory() {
    const { memories, stale } = await api("memory");
    const why = Object.fromEntries(stale.map((s) => [s.id, s.why]));
    const forget = (id) => {
      const reason = prompt(`Why retire ${id}?`);
      if (reason) act(`memory/${id}/forget`, { reason }, `Retired ${id}`);
    };
    return table([["Memory"], ["Scope"], ["Verified", "opt"], [""]],
      memories.map(({ meta: m, body, active }) => h("tr", {},
        h("td", {}, h("details", {}, h("summary", { class: "mono" }, m.id), h("p", { class: "mem-body" }, body),
          h("p", { class: "muted" }, `Source: ${m.source}. Invalid when ${m.invalidate_when}.`)),
          why[m.id] ? h("div", { class: "state keep" }, why[m.id]) : null, active ? null : h("div", { class: "muted" }, m.status)),
        h("td", { class: "mono" }, m.scope),
        h("td", { class: "num opt" }, m.verified || ""),
        h("td", { class: "act" },
          h("button", { onclick: () => act(`memory/${m.id}/verify`, {}, `Verified ${m.id}`) }, "Verified today"),
          active ? h("button", { class: "danger", onclick: () => forget(m.id) }, "Retire") : null))),
      "No memories yet.");
  },
};

// Worktrees can take seconds to scan, so a slow tab must not land on top of
// the one picked after it.
let drawn = null, drawing = 0;

async function draw() {
  const [key, arg] = location.hash.slice(1).split("/");
  const name = key in views || key in live ? key : "overview";
  const nav = document.querySelector(".tabs");
  for (const a of nav.querySelectorAll("a")) {
    if (a.hash !== "#" + name) { a.removeAttribute("aria-current"); continue; }
    a.setAttribute("aria-current", "page");
    // Bring the tab into the scroller on a phone without scrolling the page.
    const r = a.getBoundingClientRect(), n = nav.getBoundingClientRect();
    if (r.left < n.left || r.right > n.right) nav.scrollBy({ left: r.left < n.left ? r.left - n.left : r.right - n.right });
  }
  const main = document.getElementById("view");
  const seq = ++drawing;
  liveView?.stop?.();
  liveView = null;
  if (name in live) {
    liveView = live[name](arg && decodeURIComponent(arg));
    drawn = name;
    main.replaceChildren(liveView.el);
    pollLive();
    return;
  }
  if (drawn !== name) main.replaceChildren(h("p", { class: "empty" }, `Loading ${name}…`));
  let nodes;
  try {
    nodes = [await views[name]()].flat();
  } catch (e) {
    nodes = [h("p", { class: "empty" }, `${name} did not load: ${e.message}`)];
  }
  if (seq !== drawing) return;
  drawn = name;
  main.replaceChildren(...nodes);
}

async function init() {
  try {
    const o = await api("overview");
    document.getElementById("here").textContent = "on " + o.host;
    document.getElementById("meta").textContent = `registry ${o.revision || "?"} · state ${o.state.replace(/^\/Users\/[^/]+/, "~")}`;
  } catch (e) {
    toast(e.message, true);
  }
  window.addEventListener("hashchange", () => draw().then(() => document.getElementById("view").focus()));
  await Promise.all([drawHosts(false), draw()]);
  // Hosts refresh on the probe cache's own cadence, and only while visible.
  setInterval(() => document.visibilityState === "visible" && drawHosts(false), FRESH_MS);
  document.addEventListener("visibilitychange", () => document.visibilityState === "visible" && pollLive());
  setInterval(() => {
    for (const bar of document.querySelectorAll(".age span")) {
      const m = /scaleX\(([\d.]+)\)/.exec(bar.style.transform);
      if (m) bar.style.transform = `scaleX(${Math.max(0, m[1] - 1000 / FRESH_MS)})`;
    }
  }, 1000);
}

init();
