use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Paragraph, Row, Table};

use super::{MUTED, bar, bytes, duration, host_color, load_color, rate, truncate};
use crate::app::{App, Sort, pct};
use crate::model::Snapshot;

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let Some(h) = app.hosts.get(app.selected) else { return };
    let color = host_color(app.selected);
    let Some(s) = h.snap.as_ref() else {
        let block = panel(&h.cfg.name, color);
        f.render_widget(Paragraph::new(Span::styled("no reading yet", MUTED)).block(block), area);
        return;
    };

    let [left, right] = Layout::horizontal([Constraint::Length(48), Constraint::Min(0)]).areas(area);
    let [procs, conns] = Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(right);

    let title = format!("{}  {}", h.cfg.name, if h.live(app.interval()) { "" } else { "(stale)" });
    let block = panel(title.trim(), color);
    let inner_w = block.inner(left).width as usize;
    f.render_widget(Paragraph::new(system_lines(s, color, inner_w)).block(block), left);
    process_table(f, procs, app, s);
    conn_table(f, conns, app, s);
}

fn panel(title: &str, color: Color) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(format!(" {title} "), Style::new().fg(color).bold()))
}

fn system_lines(s: &Snapshot, color: Color, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let kv = |k: &str, v: String| Line::from(vec![Span::styled(format!("{k:<8}"), MUTED), Span::raw(v)]);
    lines.push(kv("host", s.hostname.clone()));
    lines.push(kv("os", truncate(&s.os, width - 8)));
    lines.push(kv("kernel", format!("{} {}", s.kernel, s.arch)));
    lines.push(kv("cpu", truncate(&s.cpu_brand, width - 8)));
    lines.push(kv("uptime", duration(s.uptime)));
    lines.push(kv("load", format!("{:.2} {:.2} {:.2}", s.load[0], s.load[1], s.load[2])));
    lines.push(kv("procs", s.process_count.to_string()));
    lines.push(Line::default());

    // Cores two to a row: "  3 ▕██····▏ 41%".
    let bar_w = (width.saturating_sub(4) / 2).saturating_sub(10).max(4);
    for (row, pair) in s.cores.chunks(2).enumerate() {
        let mut spans = Vec::new();
        for (j, c) in pair.iter().enumerate() {
            spans.push(Span::styled(format!("{:>3} ", row * 2 + j), MUTED));
            spans.extend(bar(*c as f64, bar_w, load_color(*c as f64, color)));
            spans.push(Span::raw(format!("{:>4.0}% ", c)));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());

    let meter = |label: &str, p: f64, text: String| {
        let mut spans = vec![Span::styled(format!("{label:<8}"), MUTED)];
        spans.extend(bar(p, 16, load_color(p, color)));
        spans.push(Span::raw(format!(" {text}")));
        Line::from(spans)
    };
    lines.push(meter("mem", pct(s.mem_used, s.mem_total), format!("{} / {}", bytes(s.mem_used), bytes(s.mem_total))));
    if s.swap_total > 0 {
        lines.push(meter(
            "swap",
            pct(s.swap_used, s.swap_total),
            format!("{} / {}", bytes(s.swap_used), bytes(s.swap_total)),
        ));
    }
    for g in &s.gpus {
        let mut text = format!("{:.0}%", g.util);
        if let (Some(u), Some(t)) = (g.mem_used, g.mem_total) {
            text.push_str(&format!("  {}/{}", bytes(u), bytes(t)));
        } else if let Some(u) = g.mem_used {
            text.push_str(&format!("  {}", bytes(u)));
        }
        if let Some(c) = g.celsius {
            text.push_str(&format!("  {c:.0}°"));
        }
        if let Some(w) = g.watts {
            text.push_str(&format!("  {w:.0}W"));
        }
        lines.push(meter("gpu", g.util as f64, text));
        lines.push(Line::from(Span::styled(format!("        {}", truncate(&g.name, width - 8)), MUTED)));
    }
    for d in &s.disks {
        lines.push(meter(
            &truncate(&d.mount, 7),
            pct(d.used, d.total),
            format!("{} / {}", bytes(d.used), bytes(d.total)),
        ));
    }
    if let Some(b) = &s.battery {
        let state = if b.charging {
            "charging"
        } else if b.ac {
            "on ac"
        } else {
            "on battery"
        };
        lines.push(meter("battery", b.percent as f64, format!("{:.0}%  {state}", b.percent)));
    }
    lines.push(Line::default());

    for i in s.ifaces.iter().take(6) {
        let addr = i.addrs.first().map(|a| a.to_string()).unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("{:<10}", truncate(&i.name, 9)), MUTED),
            Span::raw(format!("↓{:<9} ↑{:<9}", rate(i.rx), rate(i.tx))),
            Span::styled(truncate(&addr, width.saturating_sub(31)), MUTED),
        ]));
    }
    lines.push(Line::default());

    let temps: Vec<String> =
        s.temps.iter().take(6).map(|t| format!("{} {:.0}°", truncate(&t.label, 18), t.celsius)).collect();
    for t in temps {
        lines.push(Line::from(Span::styled(t, MUTED)));
    }
    lines
}

fn process_table(f: &mut Frame, area: Rect, app: &App, s: &Snapshot) {
    let mut procs: Vec<_> = s.procs.iter().collect();
    match app.sort {
        Sort::Cpu => procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu)),
        Sort::Mem => procs.sort_by_key(|p| std::cmp::Reverse(p.mem)),
    }
    procs.dedup_by_key(|p| p.pid);
    let visible = area.height.saturating_sub(3) as usize;
    let scroll = app.scroll.min(procs.len().saturating_sub(visible));

    let rows = procs.iter().skip(scroll).map(|p| {
        Row::new(vec![
            Cell::from(Span::styled(p.pid.to_string(), MUTED)),
            Cell::from(truncate(&p.user, 12)),
            Cell::from(format!("{:.1}", p.cpu)),
            Cell::from(bytes(p.mem)),
            Cell::from(p.name.clone()),
            Cell::from(Span::styled(p.cmd.clone(), MUTED)),
        ])
    });
    let (cpu_h, mem_h) = match app.sort {
        Sort::Cpu => ("cpu% ▼", "mem"),
        Sort::Mem => ("cpu%", "mem ▼"),
    };
    let header = Row::new(["pid", "user", cpu_h, mem_h, "name", "command"]).style(Style::new().fg(MUTED));
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(18),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(panel(&format!("processes  {} total", s.process_count), Color::Reset));
    f.render_widget(table, area);
}

fn conn_table(f: &mut Frame, area: Rect, app: &App, s: &Snapshot) {
    // Peers that are configured hosts come first and carry their name.
    let peer = |ip: &std::net::IpAddr| {
        app.hosts.iter().enumerate().find(|(_, h)| h.snap.as_ref().is_some_and(|s| s.addrs.contains(ip)))
    };
    let mut conns: Vec<_> = s.conns.iter().collect();
    conns.sort_by_key(|c| (peer(&c.remote).is_none(), c.remote, c.remote_port));

    let rows: Vec<Row> = conns
        .iter()
        .map(|c| {
            let who = match peer(&c.remote) {
                Some((i, h)) => Span::styled(h.cfg.name.clone(), host_color(i)),
                None => Span::styled(c.remote.to_string(), MUTED),
            };
            let dir = if c.remote_port < c.local_port {
                format!("→ :{}", c.remote_port)
            } else {
                format!("← :{}", c.local_port)
            };
            Row::new(vec![
                Cell::from(who),
                Cell::from(dir),
                Cell::from(c.process.clone().unwrap_or_else(|| "-".into())),
                Cell::from(Span::styled(c.uid.map(|u| u.to_string()).unwrap_or_default(), MUTED)),
            ])
        })
        .collect();
    let header = Row::new(["peer", "direction", "process", "uid"]).style(Style::new().fg(MUTED));
    let table =
        Table::new(rows, [Constraint::Length(40), Constraint::Length(12), Constraint::Min(14), Constraint::Length(6)])
            .header(header)
            .block(panel(&format!("connections  {}", s.conns.len()), Color::Reset));
    f.render_widget(table, area);
}
