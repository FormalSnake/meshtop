use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Sparkline};

use super::{MUTED, bar, bytes, duration, host_color, load_color, map, rate, tail, truncate};
use crate::app::{App, Hit, HostState, Status, pct};

const CARD_HEIGHT: u16 = 12;
const CARD_MIN_WIDTH: u16 = 46;
const BAR: usize = 12;

/// Sparkline data, its maximum and colour, drawn at the right of a row.
type Spark = (Vec<u64>, u64, Color);

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let n = app.hosts.len().max(1) as u16;
    let cols = (area.width / CARD_MIN_WIDTH).clamp(1, n);
    let rows = n.div_ceil(cols);
    let cards_height = (rows * CARD_HEIGHT).min(area.height);
    let [cards, rest] = Layout::vertical([Constraint::Length(cards_height), Constraint::Min(0)]).areas(area);

    let row_areas = Layout::vertical(vec![Constraint::Length(CARD_HEIGHT); rows as usize]).split(cards);
    for (r, row) in row_areas.iter().enumerate() {
        let col_areas = Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols as usize]).split(*row);
        for (c, cell) in col_areas.iter().enumerate() {
            let i = r * cols as usize + c;
            if let Some(h) = app.hosts.get(i) {
                card(f, *cell, app, i, h);
            }
        }
    }

    if rest.height >= 8 {
        let [mesh, agents] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(rest);
        map::draw(f, mesh, app);
        super::agents::draw_panel(f, agents, app);
    }
}

fn card(f: &mut Frame, area: Rect, app: &App, i: usize, h: &HostState) {
    let color = host_color(i);
    let live = h.live(app.interval());
    let selected = i == app.selected;

    let mut title = vec![Span::styled(format!(" {} ", h.cfg.name), Style::new().fg(color).bold())];
    if let Some(role) = &h.cfg.role {
        title.push(Span::styled(format!("{role} "), MUTED));
    }
    if h.local {
        title.push(Span::styled("local ", MUTED));
    }
    let uptime = h.snap.as_ref().map(|s| format!(" up {} ", duration(s.uptime))).unwrap_or_default();

    let block = Block::bordered()
        .border_type(if selected { BorderType::Thick } else { BorderType::Rounded })
        .border_style(if selected { Style::new().fg(color) } else { Style::new().fg(MUTED) })
        .title(Line::from(title))
        .title_bottom(Line::from(Span::styled(uptime, MUTED)).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hit(area, Hit::Host(i));

    let Some(s) = h.snap.as_ref().filter(|_| live) else {
        let (msg, style) = match &h.status {
            Status::Down(reason) => (format!("unreachable: {reason}"), Style::new().fg(Color::Red)),
            Status::Up => ("no reading for a while".into(), Style::new().fg(Color::Yellow)),
            Status::Connecting if h.local => ("sampling…".into(), Style::new().fg(MUTED)),
            Status::Connecting => (format!("connecting to {}…", h.cfg.destination()), Style::new().fg(MUTED)),
        };
        f.render_widget(Paragraph::new(msg).style(style).wrap(ratatui::widgets::Wrap { trim: true }), inner);
        return;
    };

    let mem_pct = pct(s.mem_used, s.mem_total);
    let gpu = s.gpus.first();
    let mut rows: Vec<(Line, Option<Spark>)> = Vec::new();
    let spark_w = inner.width.saturating_sub(31);

    rows.push((
        Line::from(Span::styled(truncate(&format!("{} · {}", s.os, s.cpu_brand), inner.width as usize), MUTED)),
        None,
    ));

    let metric = |label: &str, value: f64, text: String| {
        let mut spans = vec![Span::styled(format!("{label:<5}"), MUTED)];
        spans.extend(bar(value, BAR, load_color(value, color)));
        spans.push(Span::raw(format!(" {text:<13}")));
        Line::from(spans)
    };
    rows.push((
        metric("cpu", s.cpu as f64, format!("{:>3.0}%  {}c", s.cpu, s.cores.len())),
        Some((tail(&h.cpu, spark_w), 100, color)),
    ));
    rows.push((
        metric("mem", mem_pct, format!("{}/{}", bytes(s.mem_used), bytes(s.mem_total))),
        Some((tail(&h.mem, spark_w), 100, color)),
    ));
    if let Some(g) = gpu {
        let mut text = format!("{:>3.0}%", g.util);
        if let Some(c) = g.celsius {
            text.push_str(&format!(" {c:.0}°"));
        }
        rows.push((metric("gpu", g.util as f64, text), Some((tail(&h.gpu, spark_w), 100, color))));
    }
    let rx = tail(&h.rx, spark_w);
    let net_max = rx.iter().copied().max().unwrap_or(1).max(1);
    rows.push((
        Line::from(vec![Span::styled("net  ", MUTED), Span::raw(format!("↓ {:<9} ↑ {:<11}", rate(s.rx), rate(s.tx)))]),
        Some((rx, net_max, color)),
    ));

    let mut misc = vec![Span::styled("load ", MUTED), Span::raw(format!("{:.2}", s.load[0]))];
    if let Some(root) = s.disks.first() {
        misc.push(Span::styled(format!("  {} ", root.mount), MUTED));
        misc.push(Span::raw(format!("{:.0}%", pct(root.used, root.total))));
    }
    if let Some(t) = s.temps.first() {
        misc.push(Span::styled("  temp ", MUTED));
        misc.push(Span::styled(format!("{:.0}°", t.celsius), load_color(t.celsius as f64, Color::Reset)));
    }
    if let Some(b) = &s.battery {
        misc.push(Span::styled("  bat ", MUTED));
        misc.push(Span::raw(format!("{:.0}%{}", b.percent, if b.charging || b.ac { "+" } else { "" })));
    }
    rows.push((Line::from(misc), None));

    rows.push((activity_line(s, inner.width as usize), None));

    let mut procs: Vec<_> = s.procs.iter().collect();
    procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    for p in procs.iter().take(inner.height.saturating_sub(rows.len() as u16) as usize) {
        rows.push((
            Line::from(vec![
                Span::styled(format!("{:>5.0}% ", p.cpu), MUTED),
                Span::raw(truncate(&p.name, 20)),
                Span::styled(format!("  {}", truncate(&p.user, 12)), MUTED),
            ]),
            None,
        ));
    }

    let areas = Layout::vertical(vec![Constraint::Length(1); inner.height as usize]).split(inner);
    for ((line, spark), row) in rows.into_iter().zip(areas.iter()) {
        f.render_widget(Paragraph::new(line), *row);
        if let Some((data, max, color)) = spark
            && spark_w >= 4
        {
            let [_, s] = Layout::horizontal([Constraint::Min(0), Constraint::Length(spark_w)]).areas(*row);
            f.render_widget(Sparkline::default().data(&data).max(max).style(Style::new().fg(color)), s);
        }
    }
}

pub fn activity_line(s: &crate::model::Snapshot, width: usize) -> Line<'static> {
    if s.activity.is_empty() {
        return Line::from(Span::styled("no tracked activity", MUTED));
    }
    let mut spans = Vec::new();
    let mut used = 0;
    for a in &s.activity {
        let text = format!("{} {}", a.label, a.count);
        if used + text.len() + 3 > width {
            break;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", MUTED));
        }
        used += text.len() + 3;
        spans.push(Span::raw(a.label.clone()));
        spans.push(Span::styled(format!(" {}", a.count), Style::new().bold()));
    }
    Line::from(spans)
}
