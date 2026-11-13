//! Главный экран: серверы, вкладки, панель ответа, ввод команды. Лога здесь нет.

use super::layout::{self, LOGS_BTN};
use super::theme;
use crate::app::{App, Focus, TABS, Tab, ToastKind, human_bytes, human_duration};
use crate::config::Kind;
use crate::server::{PlayersSource, RconState, ServerState, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Row, Sparkline, Table, TableState, Wrap};

pub fn status_color(s: Status) -> ratatui::style::Color {
    match s {
        Status::Running | Status::Online => theme::OK,
        Status::Starting | Status::Stopping | Status::Restarting | Status::Connecting => theme::WARN,
        Status::Crashed | Status::Unreachable => theme::ERROR,
        Status::Offline => theme::MUTED,
    }
}

fn block(title: &str, focused: bool) -> Block<'static> {
    Block::bordered().title(format!(" {title} ")).border_style(Style::new().fg(if focused {
        theme::BORDER_FOCUS
    } else {
        theme::BORDER
    }))
}

pub fn draw(app: &App, f: &mut Frame) {
    let l = layout::main(f.area());
    draw_header(app, f, l.header, l.logs_btn);
    draw_sidebar(app, f, l.sidebar);
    draw_tabs(app, f, l.tabs);
    let focused = app.focus == Focus::Content;
    match app.tab {
        Tab::Status => draw_status(app, f, l.content, focused),
        Tab::Players => draw_players(app, f, l.content, focused),
        Tab::Settings => draw_settings(app, f, l.content, focused),
        Tab::Backups => draw_backups(app, f, l.content, focused),
    }
    draw_response(app, f, l.response);
    draw_input(app, f, l.input);
    draw_hints(app, f, l.hints);
}

pub fn draw_toast(app: &App, f: &mut Frame, area: Rect) {
    if let Some((msg, kind, _)) = &app.toast {
        let color = match kind {
            ToastKind::Info => theme::ACCENT,
            ToastKind::Ok => theme::OK,
            ToastKind::Error => theme::ERROR,
        };
        f.render_widget(Paragraph::new(Span::styled(msg.clone(), Style::new().fg(color))), area);
    }
}

fn draw_header(app: &App, f: &mut Frame, area: Rect, btn: Rect) {
    let s = app.sel();
    let left = Line::from(vec![
        Span::styled(" mctui ", Style::new().fg(theme::ACCENT).bold()),
        Span::styled("│ ", Style::new().fg(theme::BORDER)),
        Span::styled(s.cfg.name.clone(), Style::new().fg(theme::TEXT).bold()),
        Span::raw(" "),
        Span::styled(format!("● {}", s.status.label()), Style::new().fg(status_color(s.status))),
        Span::raw("  "),
    ]);
    let lw = left.width() as u16;
    f.render_widget(Paragraph::new(left), area);
    let mut right = btn.x;
    if let Some(v) = &app.update_available {
        let badge = format!(" ⬆ v{v}: mctui update ");
        let w = badge.chars().count() as u16;
        if right > area.x + lw + w + 10 {
            right -= w + 1;
            f.render_widget(
                Paragraph::new(Span::styled(badge, Style::new().fg(theme::WARN).bold())),
                Rect { x: right, width: w, ..area },
            );
        }
    }
    let toast_area = Rect { x: area.x + lw, width: right.saturating_sub(area.x + lw + 1), ..area };
    draw_toast(app, f, toast_area);
    f.render_widget(
        Paragraph::new(Span::styled(LOGS_BTN, Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold())),
        btn,
    );
}

fn draw_sidebar(app: &App, f: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .servers
        .iter()
        .map(|s| {
            let kind = if s.cfg.kind == Kind::Rcon { " rcon" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::new().fg(status_color(s.status))),
                Span::styled(s.cfg.name.clone(), Style::new().fg(theme::TEXT)),
                Span::styled(kind, Style::new().fg(theme::MUTED)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block("Серверы", app.focus == Focus::Sidebar))
        .highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).add_modifier(Modifier::BOLD));
    let mut st = ListState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(list, area, &mut st);
}

fn draw_tabs(app: &App, f: &mut Frame, area: Rect) {
    let mut spans = vec![];
    for (i, (t, _)) in TABS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("│", Style::new().fg(theme::BORDER)));
        }
        let style = if *t == app.tab {
            Style::new().fg(theme::BUTTON_FG).bg(theme::HIGHLIGHT_BG).bold()
        } else {
            Style::new().fg(theme::MUTED)
        };
        spans.push(Span::styled(layout::tab_label(i), style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn kv(k: &str, v: impl Into<String>, color: ratatui::style::Color) -> Row<'static> {
    Row::new(vec![
        Span::styled(k.to_string(), Style::new().fg(theme::MUTED)),
        Span::styled(v.into(), Style::new().fg(color)),
    ])
}

fn tps_color(t: f64) -> ratatui::style::Color {
    if t >= 19.0 {
        theme::OK
    } else if t >= 15.0 {
        theme::WARN
    } else {
        theme::ERROR
    }
}

fn draw_status(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let outer = block("Статус", focused);
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let show_spark = s.proc.is_some() && inner.height >= 14;
    let [table_area, spark_area] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(if show_spark { 5 } else { 0 })]).areas(inner);

    let mut rows = vec![kv("Состояние", s.status.label(), status_color(s.status))];
    match s.cfg.kind {
        Kind::Local => {
            rows.push(kv("Папка", s.cfg.dir().display().to_string(), theme::TEXT));
            if let Some(p) = &s.proc {
                rows.push(kv("PID", p.pid.to_string(), theme::TEXT));
            }
            if let Some(t) = s.started_at {
                rows.push(kv("Аптайм", human_duration(t.elapsed()), theme::TEXT));
            }
            if let Some(v) = &s.version {
                rows.push(kv("Версия", v.clone(), theme::TEXT));
            }
            if let Some(port) = s.prop("server-port") {
                rows.push(kv("Порт", port.to_string(), theme::TEXT));
            }
            match (s.cpu, s.mem) {
                (Some(c), Some(m)) => rows.push(kv("CPU / RAM", format!("{c:.0}% / {}", human_bytes(m)), theme::TEXT)),
                _ if s.proc.is_some() => rows.push(kv("CPU / RAM", "…", theme::MUTED)),
                _ => {}
            }
        }
        Kind::Rcon => rows.push(kv("Адрес", format!("{}:{}", s.cfg.host, s.cfg.port), theme::TEXT)),
    }
    if s.proc.is_some() || s.cfg.kind == Kind::Rcon {
        let max = s.max_players.map(|m| m.to_string()).unwrap_or_else(|| "?".into());
        let src = if s.players_source == PlayersSource::Rcon { "" } else { "  (по логу)" };
        rows.push(kv("Игроки", format!("{} / {max}{src}", s.players.len()), theme::TEXT));
        match s.tps {
            Some(t) => {
                rows.push(kv("TPS 1м/5м/15м", format!("{:.1} / {:.1} / {:.1}", t[0], t[1], t[2]), tps_color(t[0])))
            }
            None => rows.push(kv(
                "TPS",
                if s.cfg.kind == Kind::Local && s.rcon_state == RconState::Off {
                    "н/д — включите RCON (Настройки → r)"
                } else {
                    "…"
                },
                theme::MUTED,
            )),
        }
    }
    let (rcon_text, rcon_color) = match &s.rcon_state {
        RconState::Off => ("выключен".to_string(), theme::MUTED),
        RconState::Connecting => ("подключение…".to_string(), theme::WARN),
        RconState::Connected => ("подключён".to_string(), theme::OK),
        RconState::Error(e) => (format!("ошибка: {e}"), theme::ERROR),
    };
    rows.push(kv("RCON", rcon_text, rcon_color));
    if s.cfg.kind == Kind::Local {
        rows.push(kv("Авто-рестарт", if s.cfg.auto_restart { "вкл" } else { "выкл" }, theme::TEXT));
        if let Some(e) = &s.last_exit {
            rows.push(kv("Последний выход", e.clone(), theme::TEXT));
        }
        if let Some(p) = &s.backup_progress {
            rows.push(kv("Бэкап", p.clone(), theme::WARN));
        } else if let Some(b) = &s.last_backup {
            rows.push(kv("Последний бэкап", b.clone(), theme::TEXT));
        }
        if s.needs_restart {
            rows.push(kv("⚠", "настройки изменены — нужен перезапуск (Ctrl+R)", theme::WARN));
        }
    }
    let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(10)]);
    f.render_widget(table, table_area);

    if show_spark {
        let data: Vec<u64> = s.cpu_hist.iter().copied().collect();
        let w = spark_area.width.saturating_sub(2) as usize;
        let data = &data[data.len().saturating_sub(w)..];
        let max = data.iter().copied().max().unwrap_or(100).max(100);
        let spark = Sparkline::default()
            .block(Block::bordered().title(" CPU, % (2 мин) ").border_style(Style::new().fg(theme::BORDER)))
            .data(data)
            .max(max)
            .style(Style::new().fg(theme::ACCENT));
        f.render_widget(spark, spark_area);
    }
}

fn draw_players(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let title = format!("Игроки ({})", s.players.len());
    if s.players.is_empty() {
        let msg = if s.proc.is_none() && s.cfg.kind == Kind::Local {
            "Сервер не запущен"
        } else {
            "Никого нет онлайн"
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::new().fg(theme::MUTED))).block(block(&title, focused)),
            area,
        );
        return;
    }
    // Ширина строки внутри рамки; символ выделения «▶» занимает колонку, только когда список в фокусе.
    let row_w = area.width.saturating_sub(if focused { 3 } else { 2 }) as usize;
    let items: Vec<ListItem> = s
        .players
        .iter()
        .map(|p| {
            let name = format!("  {p}");
            if !s.is_op(p) {
                return ListItem::new(name);
            }
            const OP: &str = "Оп ";
            let pad = row_w.saturating_sub(name.chars().count() + OP.chars().count()).max(1);
            ListItem::new(Line::from(vec![
                Span::raw(name),
                Span::raw(" ".repeat(pad)),
                Span::styled(OP, Style::new().fg(theme::WARN).bold()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block(&title, focused))
        .highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold())
        .highlight_symbol("▶");
    let mut st = ListState::default().with_selected(focused.then_some(app.players_sel));
    f.render_stateful_widget(list, area, &mut st);
}

fn draw_settings(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let Some(props) = &s.props else {
        let msg = match (&s.props_error, s.cfg.kind) {
            (_, Kind::Rcon) => "Для RCON-серверов файл настроек недоступен".to_string(),
            (Some(e), _) => format!("Не удалось прочитать: {e}"),
            (None, _) => "server.properties не найден".to_string(),
        };
        f.render_widget(
            Paragraph::new(msg)
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(theme::MUTED))
                .block(block("Настройки", focused)),
            area,
        );
        return;
    };
    let rows: Vec<Row> = props
        .pairs()
        .into_iter()
        .map(|(k, v)| {
            let shown = if k.contains("password") && !v.is_empty() {
                "••••••••".to_string()
            } else {
                v.clone()
            };
            let color = match v.as_str() {
                "true" => theme::OK,
                "false" => theme::MUTED,
                _ => theme::TEXT,
            };
            Row::new(vec![Span::styled(k, Style::new().fg(theme::TEXT)), Span::styled(shown, Style::new().fg(color))])
        })
        .collect();
    let title =
        if s.needs_restart { "server.properties — нужен перезапуск" } else { "server.properties" };
    let table = Table::new(rows, [Constraint::Percentage(45), Constraint::Percentage(55)])
        .block(block(title, focused))
        .row_highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold());
    let mut st = TableState::default().with_selected(focused.then_some(app.settings_sel));
    f.render_stateful_widget(table, area, &mut st);
}

fn draw_backups(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let outer = block(&format!("Бэкапы → {}", s.cfg.backup_dir().display()), focused);
    if s.cfg.kind == Kind::Rcon {
        f.render_widget(
            Paragraph::new("Бэкапы доступны только для локальных серверов").fg(theme::MUTED).block(outer),
            area,
        );
        return;
    }
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let [top, list_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    let head = match &s.backup_progress {
        Some(p) => Line::styled(format!("⏳ {p}"), Style::new().fg(theme::WARN)),
        None => Line::styled(
            format!(
                "Пути: {} · хранить: {} · n / Ctrl+B — новый бэкап",
                s.cfg.backup_paths.join(", "),
                s.cfg.backup_keep
            ),
            Style::new().fg(theme::MUTED),
        ),
    };
    f.render_widget(Paragraph::new(head), top);
    if app.backups.is_empty() {
        f.render_widget(Paragraph::new("Бэкапов пока нет").fg(theme::MUTED), list_area);
        return;
    }
    let rows: Vec<Row> = app
        .backups
        .iter()
        .map(|b| {
            let name = b.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let when = b
                .modified
                .map(|m| chrono::DateTime::<chrono::Local>::from(m).format("%d.%m.%Y %H:%M").to_string())
                .unwrap_or_default();
            Row::new(vec![name, when, human_bytes(b.size)])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Min(20), Constraint::Length(17), Constraint::Length(11)])
        .header(Row::new(vec!["Файл", "Дата", "Размер"]).style(Style::new().fg(theme::MUTED)))
        .row_highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold());
    let mut st = TableState::default().with_selected(focused.then_some(app.backups_sel));
    f.render_stateful_widget(table, list_area, &mut st);
}

fn draw_response(app: &App, f: &mut Frame, area: Rect) {
    let s = app.sel();
    let h = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if s.response.is_empty() {
        vec![Line::styled(
            "Здесь появится ответ на команду. Полный вывод — кнопка «Логи» (L)",
            Style::new().fg(theme::MUTED),
        )]
    } else {
        // Первая строка — сама команда; затем последние строки ответа, сколько влезет.
        let (cmd, rest) = s.response.split_first().unwrap();
        let tail = &rest[rest.len().saturating_sub(h.saturating_sub(1))..];
        std::iter::once(Line::styled(cmd.clone(), Style::new().fg(theme::ACCENT)))
            .chain(tail.iter().map(|l| Line::styled(l.clone(), Style::new().fg(theme::TEXT))))
            .collect()
    };
    f.render_widget(Paragraph::new(lines).block(block("Ответ", false)), area);
}

fn draw_input(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::Input;
    let s: &ServerState = app.sel();
    let can = s.proc.is_some() || s.rcon_state == RconState::Connected;
    let title = if can {
        format!("Команда → {}", s.cfg.name)
    } else {
        format!("Команда → {} (не запущен)", s.cfg.name)
    };
    let b = block(&title, focused);
    let inner = b.inner(area);
    let text = if app.input.value.is_empty() && !focused {
        Line::styled("Enter или : — ввести команду (без /)", Style::new().fg(theme::MUTED))
    } else {
        Line::from(vec![Span::styled("› ", Style::new().fg(theme::ACCENT)), Span::raw(app.input.value.clone())])
    };
    // Горизонтальная прокрутка, чтобы курсор оставался видимым.
    let cur = app.input.cursor() as u16 + 2;
    let scroll = cur.saturating_sub(inner.width.saturating_sub(1));
    f.render_widget(Paragraph::new(text).scroll((0, scroll)).block(b), area);
    if focused && app.popup.is_none() {
        f.set_cursor_position((inner.x + cur - scroll, inner.y));
    }
}

fn draw_hints(app: &App, f: &mut Frame, area: Rect) {
    let tab_keys = match (app.focus, app.tab) {
        (Focus::Input, _) => "Enter отправить · ↑↓ история · Esc выйти из ввода",
        (Focus::Content, Tab::Players) => "↑↓ выбор · k кик · b бан · o/d оп/деоп",
        (Focus::Content, Tab::Settings) => {
            "↑↓ выбор · Enter изменить · Пробел вкл/выкл · r включить RCON · u перечитать"
        }
        (Focus::Content, Tab::Backups) => "↑↓ выбор · n новый · d удалить · u обновить",
        _ => "↑↓ сервер · a добавить · Del убрать · Tab фокус · 1-4 вкладки",
    };
    let line = Line::from(vec![
        Span::styled(format!(" {tab_keys} "), Style::new().fg(theme::TEXT)),
        Span::styled("│ ^S старт ^X стоп ^R рестарт ^B бэкап ^L логи ^Q выход", Style::new().fg(theme::MUTED)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
