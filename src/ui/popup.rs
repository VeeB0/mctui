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
        Popup::Panel(panel) => super::panel::draw(panel, f),
        Popup::Confirm { title, text, .. } => {
            // Высота — по тексту (с переносами), чтобы длинные подтверждения не обрезались.
            let w: u16 = 76;
            let inner = (w.min(f.area().width).saturating_sub(2)).max(1) as usize;
            let rows: usize = text.lines().map(|l| l.chars().count().div_ceil(inner).max(1)).sum();
            let r = centered(f.area(), w, (rows + 4).max(8) as u16);
            f.render_widget(Clear, r);
            let b = Block::bordered().title(format!(" {title} ")).border_style(Style::new().fg(theme::WARN));
            let mut lines: Vec<Line> = text.lines().map(|l| Line::raw(l.to_string())).collect();
            lines.extend([
                Line::raw(""),
                Line::from(vec![
                    Span::styled(" Enter / y ", Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold()),
                    Span::raw(crate::tr!(" да    ", " yes    ")),
                    Span::styled(" Esc / n ", Style::new().fg(theme::TEXT).bg(theme::HIGHLIGHT_BG)),
                    Span::raw(crate::tr!(" отмена", " cancel")),
                ]),
            ]);
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(b), r);
        }
        Popup::Quit { running } => {
            let r = centered(f.area(), 66, 9);
            f.render_widget(Clear, r);
            let b =
                Block::bordered().title(crate::tr!(" Выход ", " Quit ")).border_style(Style::new().fg(theme::ACCENT));
            let key = |k: &'static str| Span::styled(k, Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold());
            let lines = vec![
                Line::raw(crate::tr!(
                    format!("Запущено серверов: {running}. Они работают в фоновой службе и не зависят от окна."),
                    format!(
                        "Servers running: {running}. They run in the background service, independent of this window."
                    )
                )),
                Line::raw(""),
                Line::from(vec![
                    key(" Enter "),
                    Span::raw(crate::tr!(
                        " закрыть окно, серверы продолжат работать",
                        " close the window, servers keep running"
                    )),
                ]),
                Line::from(vec![
                    key("   s   "),
                    Span::raw(crate::tr!(" остановить все серверы и выйти", " stop all servers and quit")),
                ]),
                Line::from(vec![
                    Span::styled("  Esc  ", Style::new().fg(theme::TEXT).bg(theme::HIGHLIGHT_BG)),
                    Span::raw(crate::tr!(" отмена", " cancel")),
                ]),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(b), r);
        }
        Popup::Edit { title, input, target } => {
            let r = centered(f.area(), 70, 6);
            f.render_widget(Clear, r);
            let b = Block::bordered().title(format!(" {title} ")).border_style(Style::new().fg(theme::ACCENT));
            let inner = b.inner(r);
            let lines = vec![
                Line::from(vec![Span::styled("› ", Style::new().fg(theme::ACCENT)), Span::raw(input.value.clone())]),
                Line::raw(""),
                Line::styled(
                    match target {
                        crate::app::EditTarget::Property(_) => {
                            crate::tr!(
                                "Enter — сохранить в server.properties · Esc — отмена",
                                "Enter — save to server.properties · Esc — cancel"
                            )
                        }
                        crate::app::EditTarget::Chat => {
                            crate::tr!("Enter — отправить · Esc — отмена", "Enter — send · Esc — cancel")
                        }
                        _ => crate::tr!("Enter — сохранить · Esc — отмена", "Enter — save · Esc — cancel"),
                    },
                    Style::new().fg(theme::MUTED),
                ),
            ];
            let cur = input.cursor() as u16 + 2;
            let scroll = cur.saturating_sub(inner.width.saturating_sub(1));
            f.render_widget(Paragraph::new(lines).scroll((0, scroll)).block(b), r);
            f.set_cursor_position((inner.x + cur - scroll, inner.y));
        }
    }
}
