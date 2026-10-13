mod agents;
mod detail;
mod map;
mod overview;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Hit, Status, View};

/// Hosts take ANSI colours in this fixed order, so the terminal theme
/// decides the actual hues and a host keeps its colour across views. Green,
/// yellow and red stay out: they mean load.
const HOST_COLORS: [Color; 6] =
    [Color::Cyan, Color::Magenta, Color::Blue, Color::LightCyan, Color::LightMagenta, Color::LightBlue];

pub const MUTED: Color = Color::DarkGray;

pub fn host_color(i: usize) -> Color {
    HOST_COLORS[i % HOST_COLORS.len()]
}

/// Load colour for a percentage: the host's own colour until it gets busy.
pub fn load_color(pct: f64, base: Color) -> Color {
    if pct >= 90.0 {
        Color::Red
    } else if pct >= 75.0 {
        Color::Yellow
    } else {
        base
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    app.hits.borrow_mut().clear();
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    draw_header(f, header, app);
    match app.view {
        View::Overview => overview::draw(f, body, app),
        View::Map => map::draw_view(f, body, app),
        View::Agents => agents::draw_view(f, body, app),
        View::Host => detail::draw(f, body, app),
    }
    draw_footer(f, footer, app);
}

/// Lays spans out left to right from `x`, recording a click target for each
/// span that has one.
fn hit_spans(app: &App, area: Rect, parts: Vec<(Span<'static>, Option<Hit>)>) -> Vec<Span<'static>> {
    let mut x = area.x;
    let mut spans = Vec::with_capacity(parts.len());
    for (span, hit) in parts {
        let w = span.width() as u16;
        if let Some(hit) = hit {
            app.hit(Rect { x, y: area.y, width: w, height: 1 }, hit);
        }
        x += w;
        spans.push(span);
    }
    spans
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let interval = app.interval();
    let mut left: Vec<(Span<'static>, Option<Hit>)> =
        vec![(Span::styled(" meshtop ", Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)), None)];
    left.push((" ".into(), None));
    for (i, h) in app.hosts.iter().enumerate() {
        let (dot, color) = match &h.status {
            _ if h.live(interval) => ("●", host_color(i)),
            Status::Down(_) => ("○", Color::Red),
            _ => ("◌", MUTED),
        };
        let name = Span::raw(h.cfg.name.clone());
        let name = if i == app.selected { name.bold().underlined() } else { name };
        left.push((Span::styled(format!(" {dot} "), color), Some(Hit::Host(i))));
        left.push((name, Some(Hit::Host(i))));
    }

    let blocked: usize = app
        .hosts
        .iter()
        .filter(|h| h.live(interval))
        .filter_map(|h| h.snap.as_ref())
        .map(|s| s.agents.iter().filter(|a| a.status == "blocked").count())
        .sum();

    let mut right: Vec<(Span<'static>, Option<Hit>)> = Vec::new();
    if blocked > 0 {
        let text = format!(" ▲ {blocked} waiting for input ");
        right.push((Span::styled(text, Style::new().fg(Color::Yellow).bold()), Some(Hit::Tab(View::Agents))));
        right.push((" ".into(), None));
    }
    if app.paused {
        right.push((Span::styled(" paused ", Style::new().fg(Color::Yellow).bold()), None));
    }
    let tabs =
        [(View::Overview, "1 overview"), (View::Map, "2 map"), (View::Agents, "3 agents"), (View::Host, "4 host")];
    for (view, label) in tabs {
        let style = if app.view == view { Style::new().bold().reversed() } else { Style::new().fg(MUTED) };
        right.push((Span::styled(format!(" {label} "), style), Some(Hit::Tab(view))));
        right.push((" ".into(), None));
    }

    let right_width: u16 = right.iter().map(|(s, _)| s.width() as u16).sum();
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);
    let left = hit_spans(app, l, left);
    let right = hit_spans(app, r, right);
    f.render_widget(Paragraph::new(Line::from(left)), l);
    f.render_widget(Paragraph::new(Line::from(right)), r);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys: &[(&str, &str)] = match app.view {
        View::Host => {
            &[("←→", "host"), ("↑↓", "scroll"), ("s", "sort"), ("esc", "back"), ("p", "pause"), ("q", "quit")]
        }
        View::Agents => &[("↑↓", "scroll"), ("click", "host"), ("tab", "view"), ("p", "pause"), ("q", "quit")],
        _ => &[("←→", "host"), ("enter", "details"), ("tab", "view"), ("p", "pause"), ("q", "quit")],
    };
    let mut spans = vec![" ".into()];
    for (key, what) in keys {
        spans.push(Span::styled(*key, Style::new().bold()));
        spans.push(Span::styled(format!(" {what}   "), MUTED));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A horizontal bar `width` cells wide, using eighth blocks for the last cell.
pub fn bar(pct: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = ((pct.clamp(0.0, 100.0) / 100.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let mut filled = "█".repeat(full);
    if full < width {
        filled.push_str(EIGHTHS[eighths % 8]);
    }
    let used = full + usize::from(full < width && !eighths.is_multiple_of(8));
    vec![Span::styled(filled, color), Span::styled("·".repeat(width.saturating_sub(used)), Style::new().fg(MUTED))]
}

pub fn bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = b as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 || v >= 100.0 { format!("{v:.0}{}", UNITS[unit]) } else { format!("{v:.1}{}", UNITS[unit]) }
}

pub fn rate(b: u64) -> String {
    format!("{}/s", bytes(b))
}

pub fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

pub fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(width.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// The last `width` samples of a history, zero-padded on the left so the
/// newest sample always sits at the right edge.
pub fn tail(q: &std::collections::VecDeque<u64>, width: u16) -> Vec<u64> {
    let width = width as usize;
    let mut out = vec![0; width.saturating_sub(q.len())];
    out.extend(q.iter().skip(q.len().saturating_sub(width)));
    out
}
