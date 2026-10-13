use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Row, Table};

use super::{MUTED, bytes, host_color, truncate};
use crate::app::{App, Hit};
use crate::model::Agent;

/// Agent states as symbol plus word, so a state never rests on colour alone.
pub fn status(s: &str) -> (Span<'static>, Span<'static>) {
    let (sym, word, style) = match s {
        "blocked" => ("▲", "needs input", Style::new().fg(Color::Yellow).bold()),
        "working" => ("●", "working", Style::new().fg(Color::Green)),
        "done" => ("✓", "done", Style::new()),
        "idle" => ("○", "idle", Style::new().fg(MUTED)),
        _ => ("?", "unknown", Style::new().fg(MUTED)),
    };
    (Span::styled(sym, style), Span::styled(word, style))
}

fn rank(s: &str) -> u8 {
    match s {
        "blocked" => 0,
        "working" => 1,
        "done" => 2,
        "idle" => 3,
        _ => 4,
    }
}

/// Every agent on every live host, the ones waiting on you first.
pub fn collect(app: &App) -> Vec<(usize, &Agent)> {
    let interval = app.interval();
    let mut all: Vec<(usize, &Agent)> = app
        .hosts
        .iter()
        .enumerate()
        .filter(|(_, h)| h.live(interval))
        .filter_map(|(i, h)| h.snap.as_ref().map(|s| (i, s)))
        .flat_map(|(i, s)| s.agents.iter().map(move |a| (i, a)))
        .collect();
    all.sort_by(|a, b| rank(&a.1.status).cmp(&rank(&b.1.status)).then(a.0.cmp(&b.0)).then(a.1.cwd.cmp(&b.1.cwd)));
    all
}

/// `/Users/me/Developer/app` → `app`; the project is what tells sessions apart.
fn project(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.trim_end_matches('/').rsplit('/').take(2).collect();
    match parts.as_slice() {
        [last, parent] if matches!(*parent, "apps" | "packages" | "crates" | "nix") => format!("{parent}/{last}"),
        [last, ..] if !last.is_empty() => last.to_string(),
        _ => "~".into(),
    }
}

fn title(a: &Agent) -> String {
    match (&a.name, &a.title) {
        (Some(n), Some(t)) => format!("{n}: {t}"),
        (Some(n), None) => n.clone(),
        (None, Some(t)) => t.clone(),
        (None, None) => String::new(),
    }
}

/// The agents view: one row per agent, with the resources it is using.
pub fn draw_view(f: &mut Frame, area: Rect, app: &App) {
    let agents = collect(app);
    let visible = area.height.saturating_sub(3) as usize;
    let scroll = app.scroll.min(agents.len().saturating_sub(visible));
    let rows: Vec<Row> = agents
        .iter()
        .skip(scroll)
        .enumerate()
        .map(|(row, (i, a))| {
            app.hit(Rect { x: area.x, y: area.y + 2 + row as u16, width: area.width, height: 1 }, Hit::Host(*i));
            let (sym, word) = status(&a.status);
            Row::new(vec![
                Cell::from(Line::from(vec![sym, " ".into(), word])),
                Cell::from(Span::styled(app.hosts[*i].cfg.name.clone(), host_color(*i))),
                Cell::from(Span::styled(a.kind.clone(), MUTED)),
                Cell::from(project(&a.cwd)),
                Cell::from(title(a)),
                Cell::from(format!("{:.0}%", a.cpu)),
                Cell::from(Span::styled(bytes(a.mem), MUTED)),
            ])
        })
        .collect();
    let header = Row::new(["state", "host", "agent", "project", "task", "cpu", "mem"]).style(Style::new().fg(MUTED));
    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(22),
            Constraint::Min(20),
            Constraint::Length(6),
            Constraint::Length(7),
        ],
    )
    .header(header)
    .block(panel(&format!("agents  {}", agents.len())));
    f.render_widget(table, area);
}

/// The compact list on the overview, next to the map.
pub fn draw_panel(f: &mut Frame, area: Rect, app: &App) {
    let agents = collect(app);
    let inner_w = area.width.saturating_sub(2) as usize;
    let rows: Vec<Row> = agents
        .iter()
        .take(area.height.saturating_sub(2) as usize)
        .enumerate()
        .map(|(row, (i, a))| {
            app.hit(Rect { x: area.x, y: area.y + 1 + row as u16, width: area.width, height: 1 }, Hit::Host(*i));
            let (sym, _) = status(&a.status);
            Row::new(vec![
                Cell::from(sym),
                Cell::from(Span::styled(truncate(&app.hosts[*i].cfg.name, 10), host_color(*i))),
                Cell::from(truncate(&project(&a.cwd), 18)),
                Cell::from(Span::styled(truncate(&title(a), inner_w.saturating_sub(32)), MUTED)),
            ])
        })
        .collect();
    let working = agents.iter().filter(|(_, a)| a.status == "working").count();
    let table =
        Table::new(rows, [Constraint::Length(1), Constraint::Length(10), Constraint::Length(18), Constraint::Min(0)])
            .block(panel(&format!("agents  {working}/{} working", agents.len())));
    f.render_widget(table, area);
}

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(format!(" {title} "), Style::new().bold()))
}
