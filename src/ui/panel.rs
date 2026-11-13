//! Отрисовка окна-панели (`Popup::Panel`).

use super::theme;
use crate::app::{Panel, Tone};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

fn tone_style(t: Tone) -> Style {
    match t {
        Tone::Header => Style::new().fg(theme::ACCENT).bold(),
        Tone::Ok => Style::new().fg(theme::OK),
        Tone::Info => Style::new().fg(theme::TEXT),
        Tone::Warn => Style::new().fg(theme::WARN),
        Tone::Bad => Style::new().fg(theme::ERROR).bold(),
        Tone::Muted => Style::new().fg(theme::MUTED),
    }
}

pub fn draw(p: &Panel, f: &mut Frame) {
    let a = f.area();
    let w = (a.width * 9 / 10).max(40).min(a.width);
    let h = (a.height * 85 / 100).max(12).min(a.height);
    let r = Rect { x: a.x + (a.width - w) / 2, y: a.y + (a.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .title(p.title.clone())
        .title_style(Style::new().fg(theme::ACCENT).bold())
        .border_style(Style::new().fg(theme::BORDER_FOCUS));
    let inner = block.inner(r);
    f.render_widget(block, r);

    let input_h = if p.input.is_some() { 3 } else { 0 };
    let has_items = !p.items.is_empty();
    let text_h = if has_items { (p.lines.len() as u16).min(8) } else { inner.height.saturating_sub(input_h + 2) };
    let [input_area, text_area, list_area, status_area] = Layout::vertical([
        Constraint::Length(input_h),
        Constraint::Length(text_h),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(inner);

    if let Some(inp) = &p.input {
        let border = if p.input_focus { theme::BORDER_FOCUS } else { theme::BORDER };
        let b = Block::bordered().title(crate::tr!(" Поиск ", " Search ")).border_style(Style::new().fg(border));
        let ia = b.inner(input_area);
        f.render_widget(Paragraph::new(inp.value.clone()).style(Style::new().fg(theme::TEXT)).block(b), input_area);
        if p.input_focus {
            f.set_cursor_position((ia.x + inp.cursor() as u16, ia.y));
        }
    }

    let lines: Vec<Line> = p.lines.iter().map(|l| Line::styled(l.text.clone(), tone_style(l.tone))).collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((p.scroll as u16, 0)), text_area);

    if has_items {
        let (head_area, rows_area) = match &p.header {
            Some(_) => {
                let [h, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(list_area);
                (Some(h), rest)
            }
            None => (None, list_area),
        };
        if let (Some(h), Some(text)) = (head_area, &p.header) {
            f.render_widget(Paragraph::new(format!("  {text}")).style(Style::new().fg(theme::MUTED).bold()), h);
        }
        let items: Vec<ListItem> = p.items.iter().map(|t| ListItem::new(t.clone())).collect();
        let list = List::new(items)
            .style(Style::new().fg(theme::TEXT))
            .highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold())
            .highlight_symbol("▶ ");
        let mut st = ListState::default().with_selected((!p.input_focus).then_some(p.sel));
        f.render_stateful_widget(list, rows_area, &mut st);
    }

    let mut status = vec![];
    if let Some(b) = &p.busy {
        status.push(Line::styled(format!("⏳ {b}"), Style::new().fg(theme::WARN)));
    } else {
        status.push(Line::raw(""));
    }
    status.push(Line::from(Span::styled(p.hint.clone(), Style::new().fg(theme::MUTED))));
    f.render_widget(Paragraph::new(status), status_area);
}
