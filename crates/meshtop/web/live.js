"use strict";

// Overview, Agents and host detail: the web twins of the TUI screens of the
// same names. Each view mounts once and rebuilds its inner nodes on update.
(() => {
  const dur = (s) => {
    s = Math.floor(s);
    const d = Math.floor(s / 86400), hr = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
    return d ? `${d}d ${hr}h` : hr ? `${hr}h ${m}m` : m ? `${m}m` : `${s}s`;
  };
  const num = (n, d = 0) => Number(n).toFixed(d);
  const cls = (...c) => c.filter(Boolean).join(" ");
  const kv = (k, v) => [h("span", { class: "k" }, k), h("span", {}, v)];

  // The fill takes its colour from currentColor: load classes win, else `color`.
  function lbar(p, color) {
    const fill = h("i");
    fill.style.width = Math.max(0, Math.min(100, p)) + "%";
    const el = h("div", { class: cls("lbar", loadClass(p)) }, fill);
    if (color && !loadClass(p)) el.style.color = color;
    return el;
  }

  function metric(label, p, text, values, max, color) {
    const sp = values ? spark(values, max, "") : null;
    if (sp) sp.style.color = color;
    return h("div", { class: "lrow" },
      h("span", { class: "k" }, label),
      h("div", { class: "lm" }, lbar(p, color), h("span", { class: "v" }, text)),
      h("div", { class: "ls" }, sp));
  }

  function tempText(t) {
    const e = h("span", { class: loadClass(t.celsius) }, num(t.celsius) + "°");
    return h("span", {}, t.label + " ", e);
  }

  function batteryText(b) {
    return `${num(b.percent)}%${b.charging ? " charging" : b.ac ? " on ac" : " on battery"}`;
  }

  function agentCounts(agents) {
    const n = (s) => agents.filter((a) => a.status === s).length;
    const out = [];
    for (const s of ["blocked", "working"]) {
      const c = n(s);
      if (c) out.push(h("span", { class: "ag " + s }, `${agentMeta(s).sym} ${c} ${agentMeta(s).label}`));
    }
    return out;
  }

  function offline(hs) {
    const msg = hs.status === "down" ? "unreachable: " + (hs.reason || "no answer")
      : hs.status === "up" ? "no reading for a while"
      : hs.local ? "sampling…" : "connecting…";
    return h("p", { class: cls("lnote", hs.status === "down" && "bad") }, msg);
  }

  function statusLine(hs) {
    const s = hs.snap;
    const bits = s ? [s.os, s.arch, "up " + dur(s.uptime)] : [];
    return h("span", { class: "muted" }, [h("span", { class: "lstat " + hs.status }, hs.status),
      hs.reason ? " " + hs.reason : "", bits.length ? " · " + bits.join(" · ") : ""]);
  }

  // ---- usage --------------------------------------------------------------

  const KIND = { claude: "var(--magenta)", codex: "var(--cyan)" };

  function windowRow(w) {
    const fill = h("i");
    fill.style.width = Math.max(0, Math.min(100, w.used)) + "%";
    const bar = h("div", { class: cls("lbar win", loadClass(w.used)) }, fill);
    if (w.elapsed != null) {
      const mark = h("b");
      mark.style.left = Math.min(99, w.elapsed * 100) + "%";
      bar.append(mark);
    }
    let pace = "";
    if (w.elapsed != null) {
      const d = w.used - w.elapsed * 100;
      pace = d > 5 ? `${num(d)} ahead of pace` : d < -5 ? `${num(-d)} under pace` : "on pace";
    }
    return h("div", { class: "uwin" },
      h("span", { class: "k" }, w.label),
      bar,
      h("span", { class: cls("v", loadClass(w.used)) }, num(w.used) + "%"),
      h("span", { class: "muted opt" }, w.resets_in != null ? "resets in " + dur(w.resets_in) : ""),
      h("span", { class: cls("muted pace", w.elapsed != null && w.used - w.elapsed * 100 > 5 && "warm") }, pace));
  }

  function accountCard(a) {
    const color = KIND[a.kind] || "var(--tx)";
    const name = h("strong", {}, a.label);
    name.style.color = color;
    const head = h("div", { class: "uhead" }, name, h("span", { class: "muted" }, a.kind), a.plan ? h("span", { class: "tag" }, a.plan) : null,
      a.note ? h("span", { class: "warm" }, a.note) : null,
      a.at && Date.now() - a.at > 600000 ? h("span", { class: "muted" }, "as of " + ago(a.at)) : null);
    const kids = [head];
    if (a.error) kids.push(h("p", { class: "lnote bad" }, a.error));
    for (const w of a.windows || []) kids.push(windowRow(w));
    if ((a.history || []).length >= 5) {
      const sp = spark(a.history.slice(-14), 100);
      sp.style.color = color;
      kids.push(h("div", { class: "uspark" }, sp));
    }
    return h("div", { class: "host lcard ucard" }, kids);
  }

  function usagePanel(accounts) {
    const shown = (accounts || []).filter((a) => a.error || a.plan || (a.windows || []).length);
    if (!shown.length) return null;
    return h("section", { class: "lsec" },
      h("h2", {}, "Plan usage ", h("span", { class: "muted" }, "the mark shows how much of the window has passed")),
      h("div", { class: "hosts" }, shown.map(accountCard)));
  }

  // ---- overview -----------------------------------------------------------

  function cardBody(hs, i, d) {
    const s = hs.snap, color = hostColor(i);
    const name = h("h3", {}, hs.name);
    name.style.color = color;
    const kids = [h("div", { class: "host-head" }, name,
      hs.role ? h("span", { class: "tag" }, hs.role) : null, hs.local ? h("span", { class: "tag" }, "local") : null),
      h("div", { class: "lsub" }, statusLine(hs))];
    if (!s || !hs.live) { kids.push(offline(hs)); return kids; }
    const hist = hs.history || {};
    const gpu = s.gpus[0];
    kids.push(h("div", { class: "muted lcpu" }, s.cpuBrand));
    kids.push(metric("cpu", s.cpu, `${num(s.cpu)}%  ${s.cores.length}c`, hist.cpu, 100, color));
    kids.push(metric("mem", pct(s.memUsed, s.memTotal), `${bytes(s.memUsed)}/${bytes(s.memTotal)}`, hist.mem, 100, color));
    if (gpu) kids.push(metric("gpu", gpu.util, `${num(gpu.util)}%${gpu.celsius != null ? " " + num(gpu.celsius) + "°" : ""}`, hist.gpu, 100, color));
    const rxs = hist.rx || [];
    const sp = spark(rxs, Math.max(1, ...rxs.slice(-60)));
    sp.style.color = color;
    kids.push(h("div", { class: "lrow" }, h("span", { class: "k" }, "net"),
      h("div", { class: "lm" }, h("span", { class: "v" }, `↓ ${rate(s.rx)}  ↑ ${rate(s.tx)}`)), h("div", { class: "ls" }, sp)));
    const misc = h("div", { class: "lmisc" }, ...kv("load", num(s.load[0], 2)));
    for (const dk of s.disks.slice(0, 3)) misc.append(...kv(dk.mount, pct(dk.used, dk.total) + "%"));
    const temps = [...s.temps].sort((a, b) => b.celsius - a.celsius).slice(0, 3);
    for (const t of temps) misc.append(...kv(t.label, tempText({ label: "", celsius: t.celsius })));
    if (s.battery) misc.append(...kv("bat", batteryText(s.battery)));
    kids.push(misc);
    kids.push(h("div", { class: "lact" }, s.activity.length
      ? s.activity.map((a) => h("span", {}, a.label + " ", h("strong", {}, a.count)))
      : h("span", { class: "muted" }, "no tracked activity"), agentCounts(s.agents)));
    return kids;
  }

  live.overview = () => {
    const grid = h("div", { class: "hosts lhosts" });
    const slot = h("div", {});
    const el = h("div", {}, grid, slot);
    const cards = new Map();
    return {
      el,
      update(d) {
        const hosts = d.hosts || [];
        const order = [];
        hosts.forEach((hs, i) => {
          let c = cards.get(hs.name);
          if (!c) {
            c = h("a", { class: "host lcard", href: "#host/" + encodeURIComponent(hs.name) });
            cards.set(hs.name, c);
          }
          c.className = cls("host lcard", !hs.live && "stale");
          c.replaceChildren(...cardBody(hs, i, d));
          order.push(c);
        });
        if (grid.children.length !== order.length || order.some((c, i) => grid.children[i] !== c)) grid.replaceChildren(...order);
        const u = usagePanel(d.accounts);
        slot.replaceChildren(...(u ? [u] : []));
      },
    };
  };

  // ---- agents -------------------------------------------------------------

  const AGENT_CLASS = { blocked: "warm", working: "ok", done: "", idle: "dim" };

  function agentRows(d, only) {
    const rows = [];
    (d.hosts || []).forEach((hs, i) => {
      if (!hs.live || !hs.snap || (only && hs.name !== only)) return;
      for (const a of hs.snap.agents) rows.push({ a, hs, i });
    });
    rows.sort((x, y) => agentMeta(x.a.status).rank - agentMeta(y.a.status).rank || x.i - y.i || x.a.cwd.localeCompare(y.a.cwd));
    return rows;
  }

  function agentTable(rows, withHost = true) {
    const title = (a) => (a.name && a.title ? `${a.name}: ${a.title}` : a.name || a.title || "");
    return table(
      [["State"], withHost ? ["Host"] : null, ["Agent", "opt"], ["Project"], ["Task"], ["CPU"], ["Mem", "opt"]].filter(Boolean),
      rows.map(({ a, hs, i }) => {
        const m = agentMeta(a.status);
        const host = h("a", { href: "#host/" + encodeURIComponent(hs.name) }, hs.name);
        host.style.color = hostColor(i);
        return h("tr", {},
          h("td", { class: cls("mono agst", AGENT_CLASS[a.status] ?? "dim") }, `${m.sym} ${m.label}`),
          withHost ? h("td", { class: "mono" }, host) : null,
          h("td", { class: "muted opt" }, a.kind),
          h("td", { class: "mono" }, project(a.cwd)),
          h("td", { class: "muted" }, title(a)),
          h("td", { class: "num" }, num(a.cpu) + "%"),
          h("td", { class: "num opt muted" }, bytes(a.mem)));
      }),
      "No agents running.");
  }

  live.agents = () => {
    const el = h("div", {});
    return {
      el,
      update(d) {
        const rows = agentRows(d);
        const working = rows.filter((r) => r.a.status === "working").length;
        const u = usagePanel(d.accounts);
        el.replaceChildren(
          h("section", { class: "lsec" }, h("h2", {}, "Agents ", h("span", { class: "muted" }, `${working}/${rows.length} working`)), agentTable(rows)),
          ...(u ? [u] : []));
      },
    };
  };

  // ---- host detail --------------------------------------------------------

  function bigSpark(label, values, max, text, color) {
    const sp = spark(values || [], max);
    sp.style.color = color;
    return h("div", { class: "big" }, h("div", { class: "bighead" }, h("span", { class: "k" }, label), h("span", { class: "v" }, text)), sp);
  }

  function systemBlock(s, hs, color) {
    const meter = (label, p, text) => h("div", { class: "lrow flat" }, h("span", { class: "k" }, label), h("div", { class: "lm" }, lbar(p, color), h("span", { class: "v" }, text)));
    const rows = [
      h("div", { class: "kvs" },
        ...kv("host", s.hostname), ...kv("os", s.os), ...kv("kernel", `${s.kernel} ${s.arch}`),
        ...kv("cpu", `${s.cpuBrand} (${s.cores.length} cores)`), ...kv("uptime", dur(s.uptime)),
        ...kv("load", s.load.map((l) => num(l, 2)).join(" ")), ...kv("procs", s.processCount)),
      h("div", { class: "cores" }, s.cores.map((c, n) => h("div", { class: "core" }, h("span", { class: "k" }, n), lbar(c, color), h("span", { class: "v" }, num(c) + "%")))),
      meter("mem", pct(s.memUsed, s.memTotal), `${bytes(s.memUsed)} / ${bytes(s.memTotal)}`),
    ];
    if (s.swapTotal > 0) rows.push(meter("swap", pct(s.swapUsed, s.swapTotal), `${bytes(s.swapUsed)} / ${bytes(s.swapTotal)}`));
    for (const g of s.gpus) {
      const parts = [num(g.util) + "%"];
      if (g.memUsed != null) parts.push(g.memTotal != null ? `${bytes(g.memUsed)}/${bytes(g.memTotal)}` : bytes(g.memUsed));
      if (g.celsius != null) parts.push(num(g.celsius) + "°");
      if (g.watts != null) parts.push(num(g.watts) + "W");
      rows.push(meter("gpu", g.util, parts.join("  ")), h("div", { class: "muted gname" }, g.name));
    }
    for (const dk of s.disks) rows.push(meter(dk.mount.split("/").pop() || "/", pct(dk.used, dk.total), `${bytes(dk.used)} / ${bytes(dk.total)}`));
    if (s.battery) rows.push(meter("battery", s.battery.percent, batteryText(s.battery)));
    if (s.ifaces.length) {
      rows.push(h("div", { class: "kvs" }, s.ifaces.slice(0, 6).flatMap((f) =>
        kv(f.name, `↓${rate(f.rx)} ↑${rate(f.tx)}${f.addrs[0] ? "  " + f.addrs[0] : ""}`))));
    }
    if (s.temps.length) rows.push(h("div", { class: "ltemps" }, s.temps.slice(0, 6).map(tempText)));
    return rows;
  }

  function connTable(s, hosts) {
    const peer = (ip) => hosts.findIndex((o) => o.snap && o.snap.addrs.includes(ip));
    const rows = (s.conns || []).map((c) => ({ c, p: peer(c.remote) }));
    rows.sort((a, b) => (a.p < 0) - (b.p < 0) || (a.c.remote < b.c.remote ? -1 : a.c.remote > b.c.remote ? 1 : 0) || a.c.remotePort - b.c.remotePort);
    return table([["Peer"], ["Connection"], ["Process"], ["UID", "opt"]],
      rows.map(({ c, p }) => {
        const who = h("td", { class: "mono" }, p >= 0 ? hosts[p].name : c.remote);
        if (p >= 0) who.style.color = hostColor(p);
        return h("tr", {}, who,
          h("td", { class: "mono muted" }, `${c.local}:${c.localPort} → ${c.remote}:${c.remotePort}`),
          h("td", {}, c.process || "-"),
          h("td", { class: "num opt muted" }, c.uid ?? ""));
      }), "No outside connections.");
  }

  live.host = (name) => {
    const back = h("a", { class: "back", href: "#overview" }, "← Overview");
    const body = h("div", {});
    const procBody = h("tbody", {});
    let sort = "cpu", last = null;
    const th = (key, label) => h("th", { class: "sortable", tabindex: "0", role: "button", "data-sort": key,
      onclick: () => { sort = key; paintProcs(); },
      onkeydown: (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); sort = key; paintProcs(); } } }, label);
    const cpuTh = th("cpu", "CPU"), memTh = th("mem", "Mem");
    const procs = h("div", { class: "scroll" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "PID"), h("th", { class: "opt" }, "User"), cpuTh, memTh, h("th", {}, "Name"), h("th", { class: "opt" }, "Command"))),
      procBody));
    const procHead = h("h2", {});
    const procSec = h("section", { class: "lsec" }, procHead, procs);

    function paintProcs() {
      const s = last?.snap;
      if (!s) return;
      const seen = new Set();
      const list = (s.procs || []).filter((p) => !seen.has(p.pid) && seen.add(p.pid));
      list.sort(sort === "cpu" ? (a, b) => b.cpu - a.cpu : (a, b) => b.mem - a.mem);
      cpuTh.textContent = "CPU" + (sort === "cpu" ? " ▼" : "");
      memTh.textContent = "Mem" + (sort === "mem" ? " ▼" : "");
      procHead.replaceChildren("Processes ", h("span", { class: "muted" }, `${s.processCount} total`));
      procBody.replaceChildren(...list.slice(0, 150).map((p) => h("tr", {},
        h("td", { class: "num muted" }, p.pid), h("td", { class: "opt" }, p.user),
        h("td", { class: "num" }, num(p.cpu, 1)), h("td", { class: "num" }, bytes(p.mem)),
        h("td", { class: "mono" }, p.name), h("td", { class: "mono muted opt cmd" }, p.cmd))));
    }

    return {
      el: h("div", {}, back, body),
      host: name,
      update(d) {
        const hosts = d.hosts || [];
        const i = hosts.findIndex((o) => o.name === name);
        if (i < 0) { body.replaceChildren(h("p", { class: "empty" }, `Host "${name}" not found.`)); last = null; return; }
        const hs = hosts[i], s = hs.snap, color = hostColor(i);
        last = hs;
        const title = h("h1", {}, hs.name);
        title.style.color = color;
        const head = h("div", { class: "host-head dhead" }, title, hs.role ? h("span", { class: "tag" }, hs.role) : null,
          hs.local ? h("span", { class: "tag" }, "local") : null, statusLine(hs));
        if (!s) { body.replaceChildren(head, h("p", { class: "lnote" }, "No reading yet.")); return; }
        const hist = hs.history || {};
        const agents = agentRows(d, hs.name);
        paintProcs();
        body.className = hs.live ? "" : "stale";
        body.replaceChildren(
          head,
          h("div", { class: "dgrid" },
            h("section", { class: "host lcard" }, systemBlock(s, hs, color)),
            h("div", { class: "dcol" },
              h("section", { class: "host lcard bigs" },
                bigSpark("cpu", hist.cpu, 100, num(s.cpu) + "%", color),
                bigSpark("mem", hist.mem, 100, `${pct(s.memUsed, s.memTotal)}%  ${bytes(s.memUsed)}`, color),
                bigSpark("net in", hist.rx, 0, rate(s.rx), color),
                bigSpark("net out", hist.tx, 0, rate(s.tx), color),
                s.gpus.length ? bigSpark("gpu", hist.gpu, 100, num(s.gpus[0].util) + "%", color) : null),
              ...(agents.length ? [h("section", { class: "lsec" }, h("h2", {}, "Agents"), agentTable(agents, false))] : []))),
          procSec,
          h("section", { class: "lsec" }, h("h2", {}, "Connections ", h("span", { class: "muted" }, (s.conns || []).length)), connTable(s, hosts)));
      },
    };
  };
})();
