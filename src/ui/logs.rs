//! Отдельный экран логов: живой вывод, latest.log, архивы *.log.gz.

use super::layout::{self, BACK_BTN};
use super::main_screen::{draw_toast, status_color};
use super::theme;
use crate::app::{App, LogSource};
use crate::log::{Level, LogLine};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

fn level_style(l: Level) -> Style {
    match l {
        Level::Error => Style::new().fg(theme::ERROR),
        Level::Warn => Style::new().fg(theme::WARN),
        Level::Info => Style::new().fg(theme::TEXT),
        Level::Other => Style::new().fg(theme::MUTED),
    }
}

pub fn draw(app: &App, f: &mut Frame) {
    let l = layout::logs(f.area());
    let s = app.sel();

    // Шапка
    let left = Line::from(vec![
        Span::styled(" Логи ", Style::new().fg(theme::ACCENT).bold()),
        Span::styled("│ ", Style::new().fg(theme::BORDER)),
        Span::styled(s.cfg.name.clone(), Style::new().fg(theme::TEXT).bold()),
        Span::raw(" "),
        Span::styled(format!("● {}", s.status.label()), Style::new().fg(status_color(s.status))),
        Span::raw("  "),
    ]);
    let lw = left.width() as u16;
    f.render_widget(Paragraph::new(left), l.header);
    draw_toast(
        app,
        f,
        Rect { x: l.header.x + lw, width: l.back_btn.x.saturating_sub(l.header.x + lw + 1), ..l.header },
    );
    f.render_widget(
        Paragraph::new(Span::styled(BACK_BTN, Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold())),
        l.back_btn,
    );

    // Источники
    let items: Vec<ListItem> = app
        .logs
        .sources
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            let style =
                if i == app.logs.active { Style::new().fg(theme::ACCENT).bold() } else { Style::new().fg(theme::TEXT) };
            ListItem::new(Span::styled(name.clone(), style))
        })
        .collect();
    let border = if app.logs.focus_list { theme::BORDER_FOCUS } else { theme::BORDER };
    let list = List::new(items)
        .block(Block::bordered().title(" Источник ").border_style(Style::new().fg(border)))
        .highlight_style(Style::new().bg(theme::HIGHLIGHT_BG));
    let mut st = ListState::default().with_selected(Some(app.logs.source_sel));
    f.render_stateful_widget(list, l.sources, &mut st);

    // Сам лог
    let source_name = app.logs.sources.get(app.logs.active).map(|s| s.0.clone()).unwrap_or_default();
    let view_border = if app.logs.focus_list { theme::BORDER } else { theme::BORDER_FOCUS };
    let view_block = Block::bordered().title(format!(" {source_name} ")).border_style(Style::new().fg(view_border));
    let inner = view_block.inner(l.view);
    app.logs_height.set(inner.height);
    f.render_widget(view_block, l.view);

    let lines = app.visible_log_lines();
    let matches = app.log_matches(&lines);
    let total = lines.len();
    let h = inner.height as usize;
    let scroll = app.logs.scroll.min(total.saturating_sub(h));
    let end = total - scroll;
    let start = end.saturating_sub(h);
    let current = app.logs.match_cursor.and_then(|c| matches.get(c).copied());
    let query = app.logs.search.as_deref().filter(|q| !q.is_empty());

    if let Some(err) = &app.logs.file_error {
        f.render_widget(Paragraph::new(err.clone()).fg(theme::ERROR), inner);
    } else if total == 0 {
        let msg = match app.logs.sources.get(app.logs.active) {
            Some((_, LogSource::Live)) if s.cfg.kind == crate::config::Kind::Rcon => {
                "Живой лог по RCON недоступен. Здесь — история команд этой сессии.\nЧтобы видеть файл лога, задайте log_path в servers.toml (если сервер на этой машине)."
            }
            Some((_, LogSource::Live)) => "Вывода пока нет. Запустите сервер (Ctrl+S) или откройте latest.log слева.",
            _ if app.logs.filter != crate::app::LevelFilter::All => "Нет строк для выбранного фильтра (f — сменить)",
            _ => "Файл пуст",
        };
        f.render_widget(Paragraph::new(msg).fg(theme::MUTED), inner);
    } else {
        let rendered: Vec<Line> = (start..end)
            .map(|i| render_line(lines[i], query, Some(i) == current, matches.binary_search(&i).is_ok()))
            .collect();
        f.render_widget(Paragraph::new(rendered).scroll((0, app.logs.hscroll)), inner);
    }

    // Нижняя строка состояния / поиск
    if let Some(inp) = &app.logs.search_input {
        let line = Line::from(vec![
            Span::styled(" Поиск: ", Style::new().fg(theme::ACCENT).bold()),
            Span::raw(inp.value.clone()),
        ]);
        f.render_widget(Paragraph::new(line), l.footer);
        f.set_cursor_position((l.footer.x + 8 + inp.cursor() as u16, l.footer.y));
    } else {
        let mut parts = vec![format!(" строк: {total}"), format!("фильтр: {}", app.logs.filter.label())];
        if let Some(q) = query {
            let pos = app.logs.match_cursor.map(|c| (c + 1).to_string()).unwrap_or_else(|| "-".into());
            parts.push(format!("поиск «{q}»: {pos}/{}", matches.len()));
        }
        parts.push(if scroll == 0 {
            "▼ следим за концом".into()
        } else {
            format!("↑ пролистано на {scroll}")
        });
        if app.logs.hscroll > 0 {
            parts.push(format!("→ {}", app.logs.hscroll));
        }
        f.render_widget(Paragraph::new(parts.join(" · ")).fg(theme::MUTED), l.footer);
    }
    let hints = " Esc/L назад · ↑↓ PgUp PgDn прокрутка · g/G начало/конец · ←→ по горизонтали · / поиск · n/N выше/ниже · f фильтр · Tab источники · r обновить";
    f.render_widget(Paragraph::new(hints).fg(theme::TEXT), l.hints);
}

fn render_line(l: &LogLine, query: Option<&str>, current: bool, is_match: bool) -> Line<'static> {
    let base = level_style(l.level);
    let base = if current { base.bg(theme::HIGHLIGHT_BG).bold() } else { base };
    let Some(q) = query.filter(|_| is_match) else {
        return Line::styled(l.text.clone(), base);
    };
    let hay = l.text.to_lowercase();
    let needle = q.to_lowercase();
    // Подсветка возможна, только если lowercase не сдвинул байтовые позиции.
    if hay.len() != l.text.len() {
        return Line::styled(l.text.clone(), base.bg(theme::MATCH_BG));
    }
    let mut spans = vec![];
    let mut pos = 0;
    while let Some(off) = hay[pos..].find(&needle) {
        let a = pos + off;
        let b = a + needle.len();
        if !l.text.is_char_boundary(a) || !l.text.is_char_boundary(b) {
            break;
        }
        spans.push(Span::styled(l.text[pos..a].to_string(), base));
        spans.push(Span::styled(l.text[a..b].to_string(), base.bg(theme::MATCH_BG).fg(theme::BUTTON_FG)));
        pos = b;
    }
    spans.push(Span::styled(l.text[pos..].to_string(), base));
    Line::from(spans)
}
