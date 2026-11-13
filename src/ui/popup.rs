use super::theme;
use crate::app::{App, Popup};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

pub fn draw(app: &App, f: &mut Frame) {
    let Some(p) = &app.popup else { return };
    match p {
        Popup::Confirm { title, text, .. } => {
            let r = centered(f.area(), 64, 8);
            f.render_widget(Clear, r);
            let b = Block::bordered().title(format!(" {title} ")).border_style(Style::new().fg(theme::WARN));
            let lines = vec![
                Line::raw(text.clone()),
                Line::raw(""),
                Line::from(vec![
                    Span::styled(" Enter / y ", Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold()),
                    Span::raw(" да    "),
                    Span::styled(" Esc / n ", Style::new().fg(theme::TEXT).bg(theme::HIGHLIGHT_BG)),
                    Span::raw(" отмена"),
                ]),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(b), r);
        }
        Popup::Edit { title, input, .. } => {
            let r = centered(f.area(), 70, 6);
            f.render_widget(Clear, r);
            let b = Block::bordered().title(format!(" {title} ")).border_style(Style::new().fg(theme::ACCENT));
            let inner = b.inner(r);
            let lines = vec![
                Line::from(vec![Span::styled("› ", Style::new().fg(theme::ACCENT)), Span::raw(input.value.clone())]),
                Line::raw(""),
                Line::styled("Enter — сохранить в server.properties · Esc — отмена", Style::new().fg(theme::MUTED)),
            ];
            let cur = input.cursor() as u16 + 2;
            let scroll = cur.saturating_sub(inner.width.saturating_sub(1));
            f.render_widget(Paragraph::new(lines).scroll((0, scroll)).block(b), r);
            f.set_cursor_position((inner.x + cur - scroll, inner.y));
        }
    }
}
