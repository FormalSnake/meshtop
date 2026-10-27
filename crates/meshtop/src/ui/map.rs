use std::collections::BTreeMap;
use std::f64::consts::PI;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine, Points};
use ratatui::widgets::{Block, BorderType, Cell, Paragraph, Row, Sparkline, Table};

use super::{MUTED, bytes, duration, host_color, load_color, rate, tail, truncate};
use crate::app::{App, Hit, View, pct};

/// Connection kinds in a fixed colour order, so "nix build" is always the
/// same colour wherever it shows up.
const KIND_COLORS: [(&str, Color); 5] = [
    ("nix build", Color::Magenta),
    ("ssh", Color::Blue),
    ("http", Color::Cyan),
    ("syncthing", Color::LightBlue),
    ("sunshine", Color::LightMagenta),
];

pub fn kind_color(kind: &str) -> Color {
    KIND_COLORS.iter().find(|(k, _)| *k == kind).map(|(_, c)| *c).unwrap_or(Color::White)
}

/// The map view: the mesh and the selected host on top, every link
/// underneath.
pub fn draw_view(f: &mut Frame, area: Rect, app: &App) {
    let rows = grouped(app).len() as u16;
    let table_h = (rows + 3).clamp(5, area.height / 3);
    let [top, bottom] = Layout::vertical([Constraint::Min(0), Constraint::Length(table_h)]).areas(area);
    if top.width >= 110 {
        let [mesh, side] = Layout::horizontal([Constraint::Min(0), Constraint::Length(40)]).areas(top);
        draw(f, mesh, app);
        inspector(f, side, app);
    } else {
        draw(f, top, app);
    }
    links_table(f, bottom, app);
}

/// Sparkline data and its maximum.
type Spark = (Vec<u64>, u64);

/// The selected host: readings with their history, its links and agents.
fn inspector(f: &mut Frame, area: Rect, app: &App) {
    let i = app.selected;
    let Some(h) = app.hosts.get(i) else { return };
    let color = host_color(i);
    let mut title = vec![Span::styled(format!(" {} ", h.cfg.name), Style::new().fg(color).bold())];
    if let Some(role) = &h.cfg.role {
        title.push(Span::styled(format!("{role} "), MUTED));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Line::from(title));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hit(area, Hit::Host(i));
    let width = inner.width as usize;

    let Some(s) = h.snap.as_ref().filter(|_| h.live(app.interval())) else {
        f.render_widget(Paragraph::new(Span::styled("offline", MUTED)), inner);
        return;
    };

    // A line of text, optionally followed by a two-row sparkline.
    let mut rows: Vec<(Line, Option<Spark>)> = vec![
        (
            Line::from(Span::styled(
                truncate(&format!("{} · {} · up {}", s.os, s.arch, duration(s.uptime)), width),
                MUTED,
            )),
            None,
        ),
        (Line::from(Span::styled(truncate(&s.cpu_brand, width), MUTED)), None),
    ];
    let w = inner.width;
    rows.push((
        Line::from(vec![
            Span::styled("cpu  ", MUTED),
            Span::styled(format!("{:.0}%", s.cpu), load_color(s.cpu as f64, Color::Reset)),
            Span::styled(format!("  {} cores  load ", s.cores.len()), MUTED),
            Span::raw(format!("{:.2}", s.load[0])),
        ]),
        Some((tail(&h.cpu, w), 100)),
    ));
    rows.push((
        Line::from(vec![
            Span::styled("mem  ", MUTED),
            Span::raw(format!("{} of {}", bytes(s.mem_used), bytes(s.mem_total))),
            Span::styled("  swap ", MUTED),
            Span::raw(bytes(s.swap_used)),
        ]),
        Some((tail(&h.mem, w), 100)),
    ));
    if let Some(g) = s.gpus.first() {
        let mut spans = vec![Span::styled("gpu  ", MUTED), Span::raw(format!("{:.0}%", g.util))];
        if let Some(c) = g.celsius {
            spans.push(Span::raw(format!("  {c:.0}°")));
        }
        if let Some(watts) = g.watts {
            spans.push(Span::raw(format!("  {watts:.0} W")));
        }
        rows.push((Line::from(spans), Some((tail(&h.gpu, w), 100))));
    }
    let rx = tail(&h.rx, w);
    let net_max = rx.iter().copied().max().unwrap_or(1).max(1);
    rows.push((
        Line::from(vec![
            Span::styled("net  ", MUTED),
            Span::raw(format!("↓ {}", rate(s.rx))),
            Span::raw(format!("  ↑ {}", rate(s.tx))),
        ]),
        Some((rx, net_max)),
    ));

    rows.push((Line::from(Span::styled("links", Style::new().bold())), None));
    let edges = app.topo.edges();
    let mut any = false;
    for (&(a, b), kinds) in edges.iter().filter(|((a, b), _)| *a == i || *b == i) {
        any = true;
        let (dir, peer) = if a == i { ("→ ", b) } else { ("← ", a) };
        let mut spans = vec![
            Span::styled(dir, Style::new().bold()),
            Span::styled(format!("{:<10}", truncate(&app.hosts[peer].cfg.name, 10)), host_color(peer)),
        ];
        for (kind, n) in kinds {
            spans.push(Span::styled(" ■ ", kind_color(kind)));
            spans.push(Span::raw(if *n > 1 { format!("{kind} {n}") } else { kind.clone() }));
        }
        rows.push((Line::from(spans), None));
    }
    if !any {
        rows.push((Line::from(Span::styled("none to other hosts", MUTED)), None));
    }
    let ext = app.topo.external.get(i).copied().unwrap_or(0);
    if ext > 0 {
        rows.push((Line::from(Span::styled(format!("{ext} more outside the mesh"), MUTED)), None));
    }

    if !s.agents.is_empty() {
        rows.push((Line::from(Span::styled("agents", Style::new().bold())), None));
        for a in &s.agents {
            let (sym, _) = super::agents::status(&a.status);
            let what = super::agents::title(a);
            let text = if what.is_empty() { super::agents::project(&a.cwd) } else { what };
            rows.push((
                Line::from(vec![
                    sym,
                    Span::styled(format!(" {:<7}", a.kind), MUTED),
                    Span::raw(truncate(&text, width.saturating_sub(9))),
                ]),
                None,
            ));
        }
    }

    let mut y = inner.y;
    let bottom = inner.y + inner.height;
    for (line, spark) in rows {
        if y >= bottom {
            break;
        }
        f.render_widget(Paragraph::new(line), Rect { x: inner.x, y, width: inner.width, height: 1 });
        y += 1;
        if let Some((data, max)) = spark {
            let height = 2.min(bottom - y);
            let area = Rect { x: inner.x, y, width: inner.width, height };
            f.render_widget(Sparkline::default().data(&data).max(max).style(Style::new().fg(color)), area);
            y += height;
        }
    }
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let interval = app.interval();
    let live = app.hosts.iter().filter(|h| h.live(interval)).count();
    let working: usize = app
        .hosts
        .iter()
        .filter(|h| h.live(interval))
        .filter_map(|h| h.snap.as_ref())
        .map(|s| s.agents.iter().filter(|a| a.status == "working").count())
        .sum();
    let summary =
        format!(" {live}/{} up · {} links · {working} agents working ", app.hosts.len(), app.topo.links.len());
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Line::from(vec![
            Span::styled(" mesh ", Style::new().bold()),
            Span::styled(" height is cpu, ring is memory ", MUTED),
        ]))
        .title_bottom(Line::from(Span::styled(summary, MUTED)));
    if app.view == View::Map {
        let hint = if app.cam.spin {
            " drag to turn · wheel to zoom · r to stop "
        } else {
            " drag to turn · wheel to zoom · r to spin "
        };
        block = block.title_bottom(Line::from(Span::styled(hint, MUTED)).right_aligned());
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 20 || inner.height < 6 {
        return;
    }
    app.hit(inner, Hit::Graph);

    let (w, h) = (inner.width as f64, inner.height as f64);
    // Leave room for the labels; the horizontal scale is twice the vertical
    // one because cells are twice as tall as wide.
    let r = ((h * 0.6 - 2.0).min((w - 30.0) / 4.0)).max(3.0) * app.cam.zoom;
    let scene = Scene { yaw: app.cam.yaw, pitch: app.cam.pitch, cx: (w - 1.0) / 2.0, cy: (h - 1.0) / 2.0, r };
    let n = app.hosts.len();
    let nodes: Vec<V3> = (0..n)
        .map(|i| {
            let (x, z) = floor_coords(i, n);
            scene.rotate(V3 { x, y: FLOOR + load(app, i) / 100.0 * HEIGHT, z })
        })
        .collect();
    let shadows: Vec<V3> = (0..n)
        .map(|i| {
            let (x, z) = floor_coords(i, n);
            scene.rotate(V3 { x, y: FLOOR, z })
        })
        .collect();
    let edges = app.topo.edges();
    let clock = app.clock;

    let canvas =
        Canvas::default().marker(Marker::Braille).x_bounds([0.0, w - 1.0]).y_bounds([0.0, h - 1.0]).paint(move |ctx| {
            floor(ctx, &scene);
            for i in 0..n {
                stem(ctx, &scene, shadows[i], nodes[i], app.hosts[i].live(app.interval()));
            }
            ctx.layer();

            let links: Vec<Link3> = edges
                .iter()
                .map(|(&(a, b), kinds)| {
                    // The two directions of a pair bow to opposite sides.
                    let bow = if a > b && edges.contains_key(&(b, a)) { -0.12 } else { 0.12 };
                    let selected = a == app.selected || b == app.selected;
                    Link3::new(nodes[a], nodes[b], bow, &scene, kinds, selected)
                })
                .collect();
            for link in &links {
                link.draw(ctx);
            }
            ctx.layer();

            let mut order: Vec<usize> = (0..n).collect();
            order.sort_by(|&a, &b| nodes[a].z.total_cmp(&nodes[b].z));
            for &i in &order {
                node_shape(ctx, app, i, nodes[i], &scene, clock);
            }
            for link in &links {
                link.packets(ctx, clock);
            }
            for link in &links {
                link.label(ctx);
            }
            for &i in &order {
                node_label(ctx, app, i, nodes[i], &scene, inner);
            }
        });
    f.render_widget(canvas, inner);
}

/// The floor is 0% CPU; a host at 100% stands `HEIGHT` above it.
const FLOOR: f64 = -0.55;
const HEIGHT: f64 = 1.1;

/// Where host `i` of `n` stands on the floor, on a circle with the first one
/// at the front.
fn floor_coords(i: usize, n: usize) -> (f64, f64) {
    if n == 1 {
        return (0.0, 0.0);
    }
    let a = floor_angle(i, n);
    (a.sin(), a.cos())
}

pub fn floor_angle(i: usize, n: usize) -> f64 {
    2.0 * PI * i as f64 / n.max(1) as f64
}

/// CPU load, eased from the previous reading to the latest over half a
/// second so the nodes glide instead of jumping once per sample.
fn load(app: &App, i: usize) -> f64 {
    let h = &app.hosts[i];
    if !h.live(app.interval()) {
        return 0.0;
    }
    let mut last = h.cpu.iter().rev();
    let (Some(&cur), prev) = (last.next(), last.next().copied()) else { return 0.0 };
    let prev = prev.unwrap_or(cur) as f64;
    let t = h.seen.map(|s| (s.elapsed().as_secs_f64() / 0.5).min(1.0)).unwrap_or(1.0);
    let t = 1.0 - (1.0 - t).powi(3);
    prev + (cur as f64 - prev) * t
}

#[derive(Clone, Copy)]
struct V3 {
    x: f64,
    y: f64,
    z: f64,
}

/// The camera looks down -z from `DIST` units away.
const DIST: f64 = 4.0;

struct Scene {
    yaw: f64,
    pitch: f64,
    cx: f64,
    cy: f64,
    r: f64,
}

impl Scene {
    fn rotate(&self, p: V3) -> V3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (x, z) = (p.x * cy - p.z * sy, p.x * sy + p.z * cy);
        let (sp, cp) = self.pitch.sin_cos();
        V3 { x, y: p.y * cp - z * sp, z: p.y * sp + z * cp }
    }

    /// Canvas position of a rotated point, and its perspective scale.
    fn project(&self, q: V3) -> (f64, f64, f64) {
        let s = DIST / (DIST - q.z);
        (self.cx + q.x * s * self.r * 2.0, self.cy + q.y * s * self.r, s)
    }
}

/// The 0% plane as a dotted grid, so height and tilt have something to be
/// read against.
fn floor(ctx: &mut Context, sc: &Scene) {
    const EXTENT: f64 = 1.35;
    const LINES: usize = 7;
    let mut dots = Vec::new();
    for l in 0..LINES {
        let u = -EXTENT + 2.0 * EXTENT * l as f64 / (LINES - 1) as f64;
        for k in 0..=60 {
            let v = -EXTENT + 2.0 * EXTENT * k as f64 / 60.0;
            for (x, z) in [(u, v), (v, u)] {
                let (px, py, _) = sc.project(sc.rotate(V3 { x, y: FLOOR, z }));
                dots.push((px, py));
            }
        }
    }
    ctx.draw(&Points { coords: &dots, color: MUTED });
}

/// A dotted line from the host's spot on the floor up to the host.
fn stem(ctx: &mut Context, sc: &Scene, base: V3, top: V3, live: bool) {
    let (x0, y0, _) = sc.project(base);
    let (x1, y1, _) = sc.project(top);
    let steps = ((y1 - y0).abs() * 4.0).ceil().max(1.0) as usize;
    let dots: Vec<(f64, f64)> = (0..=steps)
        .step_by(2)
        .map(|k| {
            let t = k as f64 / steps as f64;
            (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
        })
        .collect();
    ctx.draw(&Points { coords: &dots, color: MUTED });
    if live {
        ctx.print(x0.round(), y0.round(), Span::styled("╳", MUTED));
    }
}

const LINK_STEPS: usize = 40;

/// One edge from client to server, bowed sideways so both directions of a
/// pair stay apart.
struct Link3 {
    path: Vec<(f64, f64)>,
    kinds: Vec<(String, usize)>,
    len: f64,
    /// Touches the selected host, the only links that get a label.
    selected: bool,
}

impl Link3 {
    fn new(a: V3, b: V3, bow: f64, sc: &Scene, kinds: &BTreeMap<String, usize>, selected: bool) -> Self {
        let d = V3 { x: b.x - a.x, y: b.y - a.y, z: b.z - a.z };
        let len = (d.x * d.x + d.y * d.y + d.z * d.z).sqrt();
        // Sideways in view space, perpendicular to the edge on screen.
        let side = {
            let l = (d.x * d.x + d.y * d.y).sqrt().max(1e-6);
            V3 { x: -d.y / l, y: d.x / l, z: 0.0 }
        };
        let path = (0..=LINK_STEPS)
            .map(|k| {
                let t = k as f64 / LINK_STEPS as f64;
                let off = bow * len * (PI * t).sin();
                let p = V3 { x: a.x + d.x * t + side.x * off, y: a.y + d.y * t + side.y * off, z: a.z + d.z * t };
                let (x, y, _) = sc.project(p);
                (x, y)
            })
            .collect();
        Self { path, kinds: kinds.iter().map(|(k, n)| (k.clone(), *n)).collect(), len, selected }
    }

    fn dominant(&self) -> Color {
        self.kinds.iter().max_by_key(|(_, n)| *n).map(|(k, _)| kind_color(k)).unwrap_or(Color::White)
    }

    fn draw(&self, ctx: &mut Context) {
        let color = self.dominant();
        for w in self.path.windows(2) {
            ctx.draw(&CanvasLine { x1: w[0].0, y1: w[0].1, x2: w[1].0, y2: w[1].1, color });
        }
    }

    fn at(&self, t: f64) -> (f64, f64) {
        self.path[((t.clamp(0.0, 1.0) * LINK_STEPS as f64).round() as usize).min(LINK_STEPS)]
    }

    /// One packet per kind, client to server, so a busy edge visibly carries
    /// more. Packets keep the same speed on long and short edges.
    fn packets(&self, ctx: &mut Context, clock: f64) {
        let speed = 0.35 / self.len.max(0.3);
        for (k, (kind, _)) in self.kinds.iter().enumerate() {
            let t = 0.1 + 0.8 * (clock * speed + k as f64 / self.kinds.len() as f64).fract();
            let trail: Vec<(f64, f64)> = (1..=4).map(|j| self.at(t - j as f64 * 0.015)).collect();
            ctx.draw(&Points { coords: &trail, color: kind_color(kind) });
            let (x, y) = self.at(t);
            ctx.print(x.round(), y.round(), Span::styled("●", kind_color(kind)));
        }
    }

    fn label(&self, ctx: &mut Context) {
        if !self.selected {
            return;
        }
        let (x, y) = self.at(0.5);
        let (x0, y0) = self.at(0.45);
        let (x1, y1) = self.at(0.55);
        let mut spans = vec![Span::styled(format!("{} ", arrow(x1 - x0, y1 - y0)), Style::new().bold())];
        for (i, (kind, n)) in self.kinds.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", MUTED));
            }
            spans.push(Span::styled("■ ", kind_color(kind)));
            spans.push(Span::raw(kind.clone()));
            if *n > 1 {
                spans.push(Span::styled(format!(" {n}"), Style::new().bold()));
            }
        }
        let width: usize = spans.iter().map(|s| s.width()).sum();
        ctx.print((x - width as f64 / 2.0).round().max(0.0), (y + 1.0).round(), Line::from(spans));
    }
}

fn arrow(dx: f64, dy: f64) -> &'static str {
    const ARROWS: [&str; 8] = ["→", "↗", "↑", "↖", "←", "↙", "↓", "↘"];
    let a = dy.atan2(dx * 0.5).rem_euclid(2.0 * PI);
    ARROWS[((a / (PI / 4.0)).round() as usize) % 8]
}

/// Radius of a node's ring in rows.
fn ring_radius(sc: &Scene, scale: f64) -> f64 {
    (0.8 + sc.r * 0.05) * scale
}

/// A ring around each host that fills clockwise with its memory use, and a
/// ping around the selected one.
fn node_shape(ctx: &mut Context, app: &App, i: usize, q: V3, sc: &Scene, clock: f64) {
    let (x, y, s) = sc.project(q);
    let rr = ring_radius(sc, s);
    let h = &app.hosts[i];
    let mem = h.snap.as_ref().filter(|_| h.live(app.interval())).map(|s| pct(s.mem_used, s.mem_total));
    let color = host_color(i);

    const N: usize = 40;
    let (mut lit, mut dim) = (Vec::new(), Vec::new());
    for k in 0..N {
        let a = PI / 2.0 - 2.0 * PI * k as f64 / N as f64;
        let p = (x + a.cos() * rr * 2.0, y + a.sin() * rr);
        match mem {
            Some(m) if (k as f64) < m / 100.0 * N as f64 => lit.push(p),
            _ => dim.push(p),
        }
    }
    ctx.draw(&Points { coords: &dim, color: MUTED });
    ctx.draw(&Points { coords: &lit, color: load_color(mem.unwrap_or(0.0), color) });

    if i == app.selected {
        let phase = (clock / 2.4).fract();
        if phase < 0.75 {
            let pr = rr * (1.0 + phase * 2.5);
            let step = 1 + (phase * 4.0) as usize;
            let ping: Vec<(f64, f64)> = (0..64)
                .step_by(step)
                .map(|k| {
                    let a = 2.0 * PI * k as f64 / 64.0;
                    (x + a.cos() * pr * 2.0, y + a.sin() * pr)
                })
                .collect();
            ctx.draw(&Points { coords: &ping, color });
        }
    }

    let (dot, style) = match mem {
        Some(_) => (if i == app.selected { "◉" } else { "●" }, Style::new().fg(color).bold()),
        None => ("○", Style::new().fg(MUTED)),
    };
    ctx.print(x.round(), y.round(), Span::styled(dot, style));
}

/// Name above the ring, readings below it.
fn node_label(ctx: &mut Context, app: &App, i: usize, q: V3, sc: &Scene, inner: Rect) {
    let (x, y, s) = sc.project(q);
    let rr = ring_radius(sc, s);
    let h = &app.hosts[i];
    let live = h.live(app.interval());
    let color = host_color(i);

    let name = if live {
        Span::styled(format!(" {} ", h.cfg.name), Style::new().fg(Color::Black).bg(color).bold())
    } else {
        Span::styled(format!(" {} ", h.cfg.name), Style::new().fg(MUTED).reversed())
    };
    let mut head = vec![name];
    if let Some(role) = &h.cfg.role {
        head.push(Span::styled(format!(" {role}"), MUTED));
    }
    let head = Line::from(head);

    let mut below: Vec<Line> = Vec::new();
    match h.snap.as_ref().filter(|_| live) {
        Some(s) => {
            let mem = pct(s.mem_used, s.mem_total);
            let mut stats = vec![
                Span::styled("cpu ", MUTED),
                Span::styled(format!("{:.0}%", s.cpu), load_color(s.cpu as f64, Color::Reset)),
                Span::styled("  mem ", MUTED),
                Span::styled(format!("{mem:.0}%"), load_color(mem, Color::Reset)),
            ];
            if let Some(gpu) = s.gpus.first() {
                stats.push(Span::styled("  gpu ", MUTED));
                stats.push(Span::raw(format!("{:.0}%", gpu.util)));
            }
            below.push(Line::from(stats));
            let working = s.agents.iter().filter(|a| a.status == "working").count();
            let blocked = s.agents.iter().filter(|a| a.status == "blocked").count();
            if !s.agents.is_empty() {
                let mut spans =
                    vec![Span::styled("agents ", MUTED), Span::raw(format!("{working}/{} working", s.agents.len()))];
                if blocked > 0 {
                    spans.push(Span::styled(format!(" · ▲ {blocked}"), Style::new().fg(Color::Yellow).bold()));
                }
                below.push(Line::from(spans));
            }
        }
        None => below.push(Line::from(Span::styled("offline", MUTED))),
    }

    let head_y = y + rr + 1.0;
    let mut rows = vec![(head_y, head)];
    rows.extend(below.into_iter().enumerate().map(|(k, l)| (y - rr - 1.0 - k as f64, l)));

    let widest = rows.iter().map(|(_, l)| l.width()).max().unwrap_or(0) as f64;
    let bottom = rows.last().map(|(y, _)| *y).unwrap_or(head_y);
    // Canvas y grows upward, screen rows downward.
    let row = |cy: f64| inner.y as f64 + (inner.height as f64 - 1.0 - cy.round()).max(0.0);
    let hit = Rect {
        x: inner.x + (x - widest / 2.0).round().max(0.0) as u16,
        y: row(head_y) as u16,
        width: widest as u16,
        height: (row(bottom) - row(head_y) + 1.0).max(1.0) as u16,
    };
    app.hit(hit.intersection(inner), Hit::Host(i));
    for (cy, line) in rows {
        let width = line.width() as f64;
        ctx.print((x - width / 2.0).round().max(0.0), cy.round(), line);
    }
}

/// Links counted per client, server, kind, port and client process.
fn grouped(app: &App) -> BTreeMap<(usize, usize, String, u16, String), usize> {
    let mut grouped = BTreeMap::new();
    for l in &app.topo.links {
        let process = l.client_process.clone().unwrap_or_default();
        *grouped.entry((l.client, l.server, l.kind.clone(), l.port, process)).or_default() += 1;
    }
    grouped
}

fn links_table(f: &mut Frame, area: Rect, app: &App) {
    let name = |i: usize| Span::styled(app.hosts[i].cfg.name.clone(), host_color(i));
    let rows: Vec<Row> = grouped(app)
        .into_iter()
        .map(|((c, s, kind, port, process), n)| {
            Row::new(vec![
                Cell::from(name(c)),
                Cell::from(name(s)),
                Cell::from(Line::from(vec![Span::styled("■ ", kind_color(&kind)), Span::raw(kind)])),
                Cell::from(port.to_string()),
                Cell::from(Span::styled(if process.is_empty() { "-".into() } else { process }, MUTED)),
                Cell::from(n.to_string()),
            ])
        })
        .collect();
    let empty = rows.is_empty();
    let header = Row::new(["from", "to", "kind", "port", "process", "conns"]).style(Style::new().fg(MUTED));
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(16),
            Constraint::Length(7),
            Constraint::Min(12),
            Constraint::Length(6),
        ],
    )
    .header(header)
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(MUTED))
            .title(Span::styled(if empty { " no links between hosts " } else { " links " }, Style::new().bold())),
    );
    f.render_widget(table, area);
}
