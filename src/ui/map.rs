use std::collections::BTreeMap;
use std::f64::consts::PI;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine};
use ratatui::widgets::{Block, BorderType, Cell, Row, Table};

use super::{MUTED, host_color, truncate};
use crate::app::{App, Hit, pct};

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

/// The map view: the mesh on top, every link underneath.
pub fn draw_view(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.topo.links.len() as u16;
    let table_h = (rows + 3).clamp(5, area.height / 2);
    let [top, bottom] = Layout::vertical([Constraint::Min(0), Constraint::Length(table_h)]).areas(area);
    draw(f, top, app);
    links_table(f, bottom, app);
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(" mesh ", Style::new().bold()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 20 || inner.height < 6 {
        return;
    }

    let (w, h) = (inner.width as f64, inner.height as f64);
    let positions = layout(app.hosts.len(), w, h);
    let edges = app.topo.edges();
    let frame = app.frame;

    let canvas =
        Canvas::default().marker(Marker::Braille).x_bounds([0.0, w - 1.0]).y_bounds([0.0, h - 1.0]).paint(move |ctx| {
            for (&(a, b), kinds) in &edges {
                let (pa, pb) = (positions[a], positions[b]);
                let dominant = kinds.iter().max_by_key(|(_, n)| **n).map(|(k, _)| k.as_str()).unwrap_or("");
                ctx.draw(&CanvasLine { x1: pa.0, y1: pa.1, x2: pb.0, y2: pb.1, color: kind_color(dominant) });
            }
            ctx.layer();

            // Traffic dots run from client to server, one per kind, so a busy
            // edge visibly carries more.
            for (&(a, b), kinds) in &edges {
                let (pa, pb) = (positions[a], positions[b]);
                let len = ((pb.0 - pa.0).powi(2) + (pb.1 - pa.1).powi(2)).sqrt().max(1.0);
                let step = 1.5 / len;
                for (k, (kind, _)) in kinds.iter().enumerate() {
                    let t = ((frame as f64 * step) + k as f64 / kinds.len() as f64).fract();
                    let t = 0.12 + t * 0.76;
                    let (x, y) = (pa.0 + (pb.0 - pa.0) * t, pa.1 + (pb.1 - pa.1) * t);
                    ctx.print(x.round(), y.round(), Span::styled("●", kind_color(kind)));
                }
            }

            edge_labels(ctx, &edges, &positions);
            node_labels(ctx, app, &positions, inner);
        });
    f.render_widget(canvas, inner);
}

/// Hosts on an ellipse, first one at the top. Terminal cells are about twice
/// as tall as wide, so the horizontal radius is doubled to look round.
fn layout(n: usize, w: f64, h: f64) -> Vec<(f64, f64)> {
    let (cx, cy) = ((w - 1.0) / 2.0, (h - 1.0) / 2.0);
    if n == 1 {
        return vec![(cx, cy)];
    }
    let ry = (h / 2.0 - 3.0).max(1.0);
    let rx = (ry * 2.6).min(w / 2.0 - 16.0).max(4.0);
    let start = if n == 2 { PI } else { PI / 2.0 };
    (0..n)
        .map(|i| {
            let a = start - 2.0 * PI * i as f64 / n as f64;
            (cx + rx * a.cos(), cy + ry * a.sin())
        })
        .collect()
}

fn arrow(dx: f64, dy: f64) -> &'static str {
    const ARROWS: [&str; 8] = ["→", "↗", "↑", "↖", "←", "↙", "↓", "↘"];
    let a = dy.atan2(dx * 0.5).rem_euclid(2.0 * PI);
    ARROWS[((a / (PI / 4.0)).round() as usize) % 8]
}

fn edge_labels(ctx: &mut Context, edges: &BTreeMap<(usize, usize), BTreeMap<String, usize>>, pos: &[(f64, f64)]) {
    for (&(a, b), kinds) in edges {
        let (pa, pb) = (pos[a], pos[b]);
        let (mx, my) = ((pa.0 + pb.0) / 2.0, (pa.1 + pb.1) / 2.0);
        // Both directions of a pair share a midpoint; offset the second.
        let my = if a > b && edges.contains_key(&(b, a)) { my - 1.0 } else { my };
        let mut spans = vec![Span::styled(format!("{} ", arrow(pb.0 - pa.0, pb.1 - pa.1)), Style::new().bold())];
        for (i, (kind, n)) in kinds.iter().enumerate() {
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
        ctx.print((mx - width as f64 / 2.0).round().max(0.0), my.round(), Line::from(spans));
    }
}

fn node_labels(ctx: &mut Context, app: &App, pos: &[(f64, f64)], inner: Rect) {
    let interval = app.interval();
    for (i, h) in app.hosts.iter().enumerate() {
        let (x, y) = pos[i];
        let color = host_color(i);
        let live = h.live(interval);
        let mut lines: Vec<Line> = Vec::new();

        let mut name = Span::styled(format!(" {} ", h.cfg.name), Style::new().fg(Color::Black).bg(color).bold());
        if !live {
            name = Span::styled(format!(" {} ", h.cfg.name), Style::new().fg(MUTED).reversed());
        }
        let mut head = vec![name];
        if let Some(role) = &h.cfg.role {
            head.push(Span::styled(format!(" {role}"), MUTED));
        }
        lines.push(Line::from(head));

        match h.snap.as_ref().filter(|_| live) {
            Some(s) => {
                let mut stats = vec![
                    Span::styled("cpu ", MUTED),
                    Span::raw(format!("{:.0}%", s.cpu)),
                    Span::styled("  mem ", MUTED),
                    Span::raw(format!("{:.0}%", pct(s.mem_used, s.mem_total))),
                ];
                if let Some(g) = s.gpus.first() {
                    stats.push(Span::styled("  gpu ", MUTED));
                    stats.push(Span::raw(format!("{:.0}%", g.util)));
                }
                lines.push(Line::from(stats));
                let working = s.agents.iter().filter(|a| a.status == "working").count();
                let blocked = s.agents.iter().filter(|a| a.status == "blocked").count();
                if !s.agents.is_empty() {
                    let mut spans = vec![
                        Span::styled("agents ", MUTED),
                        Span::raw(format!("{working}/{} working", s.agents.len())),
                    ];
                    if blocked > 0 {
                        spans.push(Span::styled(format!(" · ▲ {blocked}"), Style::new().fg(Color::Yellow).bold()));
                    }
                    lines.push(Line::from(spans));
                }
                let acts: Vec<String> = s.activity.iter().map(|a| format!("{} {}", a.label, a.count)).collect();
                if !acts.is_empty() {
                    lines.push(Line::from(truncate(&acts.join(" · "), 34)));
                }
                let ext = app.topo.external.get(i).copied().unwrap_or(0);
                if ext > 0 {
                    lines.push(Line::from(Span::styled(format!("{ext} outside the mesh"), MUTED)));
                }
            }
            None => lines.push(Line::from(Span::styled("offline", MUTED))),
        }

        let top = y + (lines.len() as f64 - 1.0) / 2.0;
        let widest = lines.iter().map(Line::width).max().unwrap_or(0) as f64;
        // Canvas y grows upward, screen rows downward.
        let screen_top = inner.y + (inner.height as f64 - 1.0 - top.round()).max(0.0) as u16;
        let screen_left = inner.x + (x - widest / 2.0).round().max(0.0) as u16;
        let hit = Rect { x: screen_left, y: screen_top, width: widest as u16, height: lines.len() as u16 };
        app.hit(hit.intersection(inner), Hit::Host(i));
        for (row, line) in lines.into_iter().enumerate() {
            let width = line.width() as f64;
            ctx.print((x - width / 2.0).round().max(0.0), (top - row as f64).round(), line);
        }
    }
}

fn links_table(f: &mut Frame, area: Rect, app: &App) {
    let mut grouped: BTreeMap<(usize, usize, String, u16, String), usize> = BTreeMap::new();
    for l in &app.topo.links {
        let process = l.client_process.clone().unwrap_or_default();
        *grouped.entry((l.client, l.server, l.kind.clone(), l.port, process)).or_default() += 1;
    }
    let name = |i: usize| Span::styled(app.hosts[i].cfg.name.clone(), host_color(i));
    let rows: Vec<Row> = grouped
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
