use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Sparkline};

use super::{MUTED, duration, load_color, truncate};
use crate::app::{AccountState, App};
use crate::config::AccountKind;
use crate::usage::Window;

const GAUGE: usize = 20;

/// Accounts that are signed in, in config order.
pub fn visible(app: &App) -> Vec<&AccountState> {
    app.accounts.iter().filter(|a| a.reading.is_some()).collect()
}

/// One row per account: each window as a gauge with a mark for how much of
/// the window has passed, so usage ahead of the mark is ahead of pace.
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let accounts = visible(app);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(Span::styled(" plan usage ", Style::new().bold()))
        .title(Line::from(Span::styled(" │ marks how much of the window has passed ", MUTED)).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);

    for (row, a) in accounts.iter().enumerate().take(inner.height as usize) {
        let line_area = Rect { y: inner.y + row as u16, height: 1, ..inner };
        let (kind, color) = match a.account.kind {
            AccountKind::Claude => ("claude", Color::LightMagenta),
            AccountKind::Codex => ("codex", Color::LightCyan),
        };
        let mut spans = vec![
            Span::styled(format!("{:<14}", truncate(&a.account.label(), 13)), Style::new().fg(color).bold()),
            Span::styled(format!("{kind:<7}"), MUTED),
        ];
        match a.reading.as_ref() {
            Some(Ok(u)) => {
                spans.push(Span::styled(format!("{:<10}", truncate(u.plan.as_deref().unwrap_or(""), 9)), MUTED));
                for w in &u.windows {
                    spans.extend(window(w));
                }
                if let Some(note) = &u.note {
                    spans.push(Span::styled(note.clone(), Style::new().fg(Color::Yellow)));
                    spans.push(" ".into());
                }
                let age = jiff::Timestamp::now().duration_since(u.at).as_secs();
                if age > 600 {
                    spans.push(Span::styled(format!("as of {} ago", duration(age as u64)), MUTED));
                }
            }
            Some(Err(e)) => spans.push(Span::styled(truncate(e, inner.width as usize / 2), Color::Red)),
            None => {}
        }
        let used: u16 = spans.iter().map(|s| s.width() as u16).sum();
        let [text, spark] = Layout::horizontal([Constraint::Length(used + 1), Constraint::Min(0)]).areas(line_area);
        f.render_widget(Paragraph::new(Line::from(spans)), text);
        if spark.width >= 8 && a.history.len() >= 5 {
            let data: Vec<u64> = a.history.iter().copied().collect();
            let data = &data[data.len().saturating_sub(spark.width as usize)..];
            f.render_widget(Sparkline::default().data(data).max(100).style(Style::new().fg(color)), spark);
        }
    }
}

fn window(w: &Window) -> Vec<Span<'static>> {
    let used = w.used_now();
    let filled = ((used / 100.0) * GAUGE as f64).round() as usize;
    let mark = w.elapsed().map(|e| ((e * GAUGE as f64) as usize).min(GAUGE - 1));
    let fill = load_color(used, Color::Blue);
    let mut spans = vec![Span::styled(format!("{:<5}", w.label), MUTED)];
    for j in 0..GAUGE {
        spans.push(match (Some(j) == mark, j < filled) {
            (true, _) => Span::styled("│", Style::new().bold()),
            (false, true) => Span::styled("█", fill),
            (false, false) => Span::styled("·", MUTED),
        });
    }
    spans.push(Span::styled(format!(" {used:>3.0}%"), Style::new().fg(load_color(used, Color::Reset)).bold()));
    let resets = w.resets_in().map(|s| format!(" {}", duration(s))).unwrap_or_default();
    spans.push(Span::styled(format!("{resets:<9}"), MUTED));
    spans
}
