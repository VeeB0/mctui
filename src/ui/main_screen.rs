//! Главный экран: серверы, вкладки, панель ответа, ввод команды. Лога здесь нет.

use super::layout;
use super::theme;
use crate::app::{
    AddonsCore, AddonsSub, App, BackupHealth, Focus, ServerUi, SubLevel, TABS, Tab, ToastKind, WorldRow, ago,
    human_bytes, human_duration, vanilla_addons_text,
};
use crate::config::Kind;
use crate::plugins::PluginKind;
use crate::proto::RconView;
use crate::server::Status;
use crate::tr;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Row, Sparkline, Table, TableState, Wrap};

pub fn status_color(s: Status) -> Color {
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
    if !app.has_server() {
        return draw_connecting(app, f);
    }
    let l = layout::main(f.area());
    draw_header(app, f, l.header, l.logs_btn);
    draw_sidebar(app, f, l.sidebar);
    draw_tabs(app, f, l.tabs);
    let focused = app.focus == Focus::Content;
    match app.tab {
        Tab::Status => draw_status(app, f, l.content, focused),
        Tab::Players => draw_players(app, f, l.content, focused),
        Tab::Access => draw_access(app, f, l.content, focused),
        Tab::Addons => draw_addons(app, f, l.content, focused),
        Tab::Settings => draw_settings(app, f, l.content, focused),
        Tab::Backups => draw_backups(app, f, l.content, focused),
        Tab::World => draw_world(app, f, l.content, focused),
    }
    draw_response(app, f, l.response);
    draw_input(app, f, l.input);
    draw_hints(app, f, l.hints);
}

/// Пока нет связи со службой (или первого снимка).
fn draw_connecting(app: &App, f: &mut Frame) {
    let area = f.area();
    let mut lines = vec![
        Line::styled(" pechka", Style::new().fg(theme::ACCENT).bold()),
        Line::raw(""),
        Line::styled(
            if app.connected {
                tr!(" Загрузка…", " Loading…")
            } else {
                tr!(" Подключение к фоновой службе pechka…", " Connecting to the pechka background service…")
            },
            Style::new().fg(theme::TEXT),
        ),
    ];
    if let Some(e) = &app.daemon_error {
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!(" {e}"), Style::new().fg(theme::WARN)));
        lines.push(Line::styled(
            tr!(" Журнал службы: daemon.log рядом с servers.toml", " Service log: daemon.log next to servers.toml"),
            Style::new().fg(theme::MUTED),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(tr!(" q / Ctrl+Q — выйти", " q / Ctrl+Q — quit"), Style::new().fg(theme::MUTED)));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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
    let mut left = vec![
        Span::styled(" pechka ", Style::new().fg(theme::ACCENT).bold()),
        Span::styled("│ ", Style::new().fg(theme::BORDER)),
        Span::styled(s.cfg.name.clone(), Style::new().fg(theme::TEXT).bold()),
        Span::raw(" "),
        Span::styled(format!("● {}", s.status.label()), Style::new().fg(status_color(s.status))),
        Span::raw("  "),
    ];
    if !app.connected {
        left.push(Span::styled(
            tr!("⚠ нет связи со службой  ", "⚠ no connection to the service  "),
            Style::new().fg(theme::ERROR).bold(),
        ));
    }
    let left = Line::from(left);
    let lw = left.width() as u16;
    f.render_widget(Paragraph::new(left), area);
    let mut right = btn.x;
    let badge = if app.daemon_outdated {
        app.daemon_version.as_ref().map(|v| {
            (
                tr!(format!(" служба v{v}: pechka daemon restart "), format!(" service v{v}: pechka daemon restart ")),
                theme::WARN,
            )
        })
    } else {
        app.update_available.as_ref().map(|v| {
            let text = if app.updating {
                tr!(format!(" ⬆ v{v}: обновляю… "), format!(" ⬆ v{v}: updating… "))
            } else {
                tr!(format!(" ⬆ v{v}: Ctrl+U — обновить "), format!(" ⬆ v{v}: Ctrl+U to update "))
            };
            (text, theme::WARN)
        })
    };
    app.update_badge.set(Rect::default());
    if let Some((badge, color)) = badge {
        let w = badge.chars().count() as u16;
        if right > area.x + lw + w + 10 {
            right -= w + 1;
            if !app.daemon_outdated {
                app.update_badge.set(Rect { x: right, width: w, ..area });
            }
            f.render_widget(
                Paragraph::new(Span::styled(badge, Style::new().fg(color).bold())),
                Rect { x: right, width: w, ..area },
            );
        }
    }
    let toast_area = Rect { x: area.x + lw, width: right.saturating_sub(area.x + lw + 1), ..area };
    draw_toast(app, f, toast_area);
    f.render_widget(
        Paragraph::new(Span::styled(layout::logs_btn(), Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold())),
        btn,
    );
}

fn draw_sidebar(app: &App, f: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .servers
        .iter()
        .map(|s| {
            let kind = if s.cfg.kind == Kind::Rcon { " rcon" } else { "" };
            let players =
                if s.running() && !s.players.is_empty() { format!(" {}👤", s.players.len()) } else { String::new() };
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::new().fg(status_color(s.status))),
                Span::styled(s.cfg.name.clone(), Style::new().fg(theme::TEXT)),
                Span::styled(kind, Style::new().fg(theme::MUTED)),
                Span::styled(players, Style::new().fg(theme::MUTED)),
                Span::styled(if s.alerts.is_empty() { "" } else { " ⚠" }, Style::new().fg(theme::ERROR).bold()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block(tr!("Серверы", "Servers"), app.focus == Focus::Sidebar))
        .highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).add_modifier(Modifier::BOLD));
    let mut st = ListState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(list, area, &mut st);
}

fn draw_tabs(app: &App, f: &mut Frame, area: Rect) {
    let mut spans = vec![];
    for (i, t) in TABS.iter().enumerate() {
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

fn kv(k: &str, v: impl Into<String>, color: Color) -> Row<'static> {
    Row::new(vec![
        Span::styled(k.to_string(), Style::new().fg(theme::MUTED)),
        Span::styled(v.into(), Style::new().fg(color)),
    ])
}

fn tps_color(t: f64) -> Color {
    if t >= 19.0 {
        theme::OK
    } else if t >= 15.0 {
        theme::WARN
    } else {
        theme::ERROR
    }
}

fn when(unix: i64) -> String {
    use chrono::TimeZone;
    let Some(t) = chrono::Local.timestamp_opt(unix, 0).single() else { return String::new() };
    let now = chrono::Local::now();
    let left = (t - now).num_seconds().max(0) as u64;
    let day = if t.date_naive() == now.date_naive() { String::new() } else { t.format("%d.%m ").to_string() };
    format!(
        "{day}{} ({} {})",
        t.format("%H:%M"),
        tr!("через", "in"),
        human_duration(std::time::Duration::from_secs(left))
    )
}

fn status_rows(s: &ServerUi) -> Vec<Row<'static>> {
    let mut rows = vec![kv(tr!("Состояние", "State"), s.status.label(), status_color(s.status))];
    for (i, a) in s.alerts.iter().enumerate() {
        rows.push(kv(if i == 0 { tr!("⚠ Внимание", "⚠ Warning") } else { "" }, a.clone(), theme::ERROR));
    }
    match s.cfg.kind {
        Kind::Local => {
            rows.push(kv(tr!("Папка", "Folder"), s.cfg.dir().display().to_string(), theme::TEXT));
            if let Some(pid) = s.pid {
                rows.push(kv("PID", pid.to_string(), theme::TEXT));
            }
            if let Some(t) = s.started_unix {
                let up = (chrono::Local::now().timestamp() - t).max(0) as u64;
                rows.push(kv(tr!("Аптайм", "Uptime"), human_duration(std::time::Duration::from_secs(up)), theme::TEXT));
            }
            if let Some(v) = &s.version {
                rows.push(kv(tr!("Версия", "Version"), v.clone(), theme::TEXT));
            }
            if let Some(port) = s.prop("server-port") {
                rows.push(kv(tr!("Порт", "Port"), port.to_string(), theme::TEXT));
            }
            let xmx = s.cfg.max_memory_mb().map(|m| format!(" / {}", human_bytes(m * 1024 * 1024))).unwrap_or_default();
            match (s.cpu, s.mem) {
                (Some(c), Some(m)) => {
                    rows.push(kv("CPU / RAM", format!("{c:.0}% / {}{xmx}", human_bytes(m)), theme::TEXT))
                }
                _ if s.running() => rows.push(kv("CPU / RAM", "…", theme::MUTED)),
                _ => {}
            }
        }
        Kind::Rcon => rows.push(kv(tr!("Адрес", "Address"), format!("{}:{}", s.cfg.host, s.cfg.port), theme::TEXT)),
    }
    if s.running() || s.cfg.kind == Kind::Rcon {
        let max = s.max_players.or_else(|| s.prop("max-players").and_then(|v| v.parse().ok()));
        let max = max.map(|m: u32| m.to_string()).unwrap_or_else(|| "?".into());
        let src = if s.players_from_rcon { "" } else { tr!("  (по логу)", "  (from log)") };
        rows.push(kv(tr!("Игроки", "Players"), format!("{} / {max}{src}", s.players.len()), theme::TEXT));
        match s.tps {
            Some(t) => rows.push(kv(
                tr!("TPS 1м/5м/15м", "TPS 1m/5m/15m"),
                format!("{:.1} / {:.1} / {:.1}", t[0], t[1], t[2]),
                tps_color(t[0]),
            )),
            None => rows.push(kv(
                "TPS",
                if s.cfg.kind == Kind::Local && s.rcon == RconView::Off {
                    tr!("н/д — включите RCON (Настройки → r)", "n/a — enable RCON (Settings → r)")
                } else {
                    "…"
                },
                theme::MUTED,
            )),
        }
    }
    let (rcon_text, rcon_color) = match &s.rcon {
        RconView::Off => (tr!("выключен", "off").to_string(), theme::MUTED),
        RconView::Connecting => (tr!("подключение…", "connecting…").to_string(), theme::WARN),
        RconView::Connected => (tr!("подключён", "connected").to_string(), theme::OK),
        RconView::Error(e) => (tr!(format!("ошибка: {e}"), format!("error: {e}")), theme::ERROR),
    };
    rows.push(kv("RCON", rcon_text, rcon_color));
    if s.cfg.kind == Kind::Local {
        let mut auto = vec![];
        if s.cfg.autostart {
            auto.push(tr!("автозапуск", "autostart"));
        }
        if s.cfg.auto_restart {
            auto.push(tr!("рестарт при падении", "restart on crash"));
        }
        rows.push(kv(
            tr!("Авто", "Auto"),
            if auto.is_empty() { tr!("выкл", "off").to_string() } else { auto.join(", ") },
            theme::TEXT,
        ));
        if let Some(sec) = s.restart_in {
            rows.push(kv(
                tr!("Перезапуск", "Restart"),
                tr!(format!("через {sec} с"), format!("in {sec} s")),
                theme::WARN,
            ));
        }
        if s.crash_gave_up {
            rows.push(kv(
                "⚠",
                tr!("падает слишком часто — авто-рестарт приостановлен", "crashing too often — auto-restart paused"),
                theme::ERROR,
            ));
        }
        if let Some(t) = s.next_restart_unix {
            rows.push(kv(tr!("Рестарт по расписанию", "Scheduled restart"), when(t), theme::TEXT));
        }
        if let Some(t) = s.next_backup_unix {
            rows.push(kv(tr!("Следующий авто-бэкап", "Next auto backup"), when(t), theme::TEXT));
        }
        if let Some(e) = &s.last_exit {
            rows.push(kv(tr!("Последний выход", "Last exit"), e.clone(), theme::TEXT));
        }
        if let Some(p) = &s.backup_progress {
            rows.push(kv(tr!("Бэкап", "Backup"), p.clone(), theme::WARN));
        } else {
            rows.extend(backup_row(s));
        }
        if s.needs_restart {
            rows.push(kv(
                "⚠",
                tr!("настройки изменены — нужен перезапуск (Ctrl+R)", "settings changed — restart needed (Ctrl+R)"),
                theme::WARN,
            ));
        }
    }
    rows
}

/// Строка «Последний бэкап» / «Бэкапы»: когда был последний, нет ли их вовсе.
fn backup_row(s: &ServerUi) -> Option<Row<'static>> {
    let last = tr!("Последний бэкап", "Last backup");
    let detail = s.last_backup.as_ref().map(|b| format!(" · {b}")).unwrap_or_default();
    Some(match s.backup_health(std::time::SystemTime::now()) {
        BackupHealth::Unknown => kv(last, s.last_backup.clone()?, theme::TEXT),
        BackupHealth::Missing { auto: false } => kv(
            tr!("⚠ Бэкапы", "⚠ Backups"),
            tr!("ни одного, авто-бэкап выкл · Ctrl+B", "none, auto backup off · Ctrl+B"),
            theme::ERROR,
        ),
        BackupHealth::Missing { auto: true } => kv(
            tr!("Бэкапы", "Backups"),
            tr!("пока ни одного · Ctrl+B — сделать сейчас", "none yet · Ctrl+B to make one now"),
            theme::WARN,
        ),
        BackupHealth::Fresh { age } => kv(last, format!("{}{detail}", ago(age)), theme::TEXT),
        BackupHealth::Stale { age } => kv(
            tr!("⚠ Последний бэкап", "⚠ Last backup"),
            tr!(format!("{}, авто-бэкап выкл", ago(age)), format!("{}, auto backup off", ago(age))),
            theme::WARN,
        ),
    })
}

/// Подпись промежутка графика: «за 2ч 13м».
fn span_label(minutes: usize) -> String {
    tr!(
        format!("за {}", human_duration(std::time::Duration::from_secs(minutes as u64 * 60))),
        format!("last {}", human_duration(std::time::Duration::from_secs(minutes as u64 * 60)))
    )
}

fn graph(f: &mut Frame, area: Rect, title: String, data: &[u64], max: u64, color: Color) {
    let spark = Sparkline::default()
        .block(Block::bordered().title(title).border_style(Style::new().fg(theme::BORDER)))
        .data(data)
        .max(max.max(1))
        .style(Style::new().fg(color));
    f.render_widget(spark, area);
}

fn draw_graphs(s: &ServerUi, f: &mut Frame, area: Rect) {
    let n_graphs: u16 = if s.is_local() { 4 } else { 2 };
    let h = (area.height / n_graphs).clamp(3, 7);
    let rows = Layout::vertical(vec![Constraint::Length(h); n_graphs as usize]).split(area);
    let w = area.width.saturating_sub(2) as usize;
    let hist: Vec<_> = s.history.iter().rev().take(w).rev().collect();
    let span = span_label(hist.len());

    let players: Vec<u64> = hist.iter().map(|x| x.players as u64).collect();
    let pmax = s.max_players.map(|m| m as u64).unwrap_or(0).max(players.iter().copied().max().unwrap_or(0)).max(1);
    let now_p = s.players.len();
    graph(f, rows[0], format!(" {} {now_p} · {span} ", tr!("Игроки", "Players")), &players, pmax, theme::ACCENT);

    let tps: Vec<u64> = hist.iter().map(|x| x.tps.map(|t| (t.min(20.0) * 10.0) as u64).unwrap_or(0)).collect();
    let tps_title = match s.tps {
        Some(t) => format!(" TPS {:.1} · {span} ", t[0]),
        None if hist.iter().all(|x| x.tps.is_none()) => format!(" TPS · {} ", tr!("нужен RCON", "needs RCON")),
        None => format!(" TPS · {span} "),
    };
    let tps_color = s.tps.map(|t| tps_color(t[0])).unwrap_or(theme::OK);
    graph(f, rows[1], tps_title, &tps, 200, tps_color);
    if !s.is_local() {
        return;
    }

    let mem: Vec<u64> = hist.iter().map(|x| x.mem_mb.unwrap_or(0) as u64).collect();
    let xmx = s.cfg.max_memory_mb();
    let mmax = xmx.unwrap_or(0).max(mem.iter().copied().max().unwrap_or(0));
    let now_m = s.mem.map(human_bytes).unwrap_or_else(|| "—".into());
    let of = xmx.map(|x| format!(" / {}", human_bytes(x * 1024 * 1024))).unwrap_or_default();
    graph(f, rows[2], format!(" RAM {now_m}{of} · {span} "), &mem, mmax, theme::WARN);

    let cpu: Vec<u64> = s.cpu_hist.iter().rev().take(w).rev().copied().collect();
    let cmax = cpu.iter().copied().max().unwrap_or(100).max(100);
    let now_c = s.cpu.map(|c| format!("{c:.0}%")).unwrap_or_else(|| "—".into());
    graph(f, rows[3], format!(" CPU {now_c} · {} ", tr!("2 мин", "2 min")), &cpu, cmax, theme::OK);
}

fn draw_status(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let outer = block(tr!("Статус", "Status"), focused);
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let rows = status_rows(s);
    let n = rows.len() as u16;
    let table = Table::new(rows, [Constraint::Length(22), Constraint::Min(10)]);
    let has_graphs = s.running() || !s.history.is_empty() || !s.is_local();
    if !has_graphs {
        f.render_widget(table, inner);
        return;
    }
    if inner.width >= 110 {
        let [left, right] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(inner);
        f.render_widget(table, left);
        draw_graphs(s, f, right);
    } else {
        let graphs_h = inner.height.saturating_sub(n);
        if graphs_h >= 6 {
            let [top, bottom] = Layout::vertical([Constraint::Length(n), Constraint::Min(6)]).areas(inner);
            f.render_widget(table, top);
            draw_graphs(s, f, bottom);
        } else {
            f.render_widget(table, inner);
        }
    }
}

fn muted_block(f: &mut Frame, area: Rect, title: &str, focused: bool, msg: &str) {
    f.render_widget(
        Paragraph::new(msg.to_string())
            .wrap(Wrap { trim: true })
            .style(Style::new().fg(theme::MUTED))
            .block(block(title, focused)),
        area,
    );
}

/// Строка списка с меткой справа (Оп, вкл/выкл…).
fn row_with_tag(text: String, tag: &str, tag_color: Color, width: usize) -> ListItem<'static> {
    if tag.is_empty() {
        return ListItem::new(text);
    }
    let pad = width.saturating_sub(text.chars().count() + tag.chars().count() + 1).max(1);
    ListItem::new(Line::from(vec![
        Span::raw(text),
        Span::raw(" ".repeat(pad)),
        Span::styled(format!("{tag} "), Style::new().fg(tag_color).bold()),
    ]))
}

fn draw_players(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let title = tr!(format!("Игроки ({})", s.players.len()), format!("Players ({})", s.players.len()));
    if s.players.is_empty() {
        let msg = if !s.running() && s.cfg.kind == Kind::Local {
            tr!("Сервер не запущен", "The server is not running")
        } else {
            tr!("Никого нет онлайн", "Nobody is online")
        };
        return muted_block(f, area, &title, focused, msg);
    }
    // Ширина строки внутри рамки; символ выделения «▶» занимает колонку, только когда список в фокусе.
    let row_w = area.width.saturating_sub(if focused { 3 } else { 2 }) as usize;
    let op = tr!("Оп", "Op");
    let items: Vec<ListItem> = s
        .players
        .iter()
        .map(|p| row_with_tag(format!("  {p}"), if s.is_op(p) { op } else { "" }, theme::WARN, row_w))
        .collect();
    let list = List::new(items)
        .block(block(&title, focused))
        .highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold())
        .highlight_symbol("▶");
    let mut st = ListState::default().with_selected(focused.then_some(app.players_sel));
    f.render_stateful_widget(list, area, &mut st);
}

/// Строка подвкладок со стилями и подсказкой справа. Подсветка фоном — там, куда сейчас смотрят
/// стрелки (см. `SubLevel`); без фокуса стрелки не показываем.
pub fn sub_tabs_line(labels: &[String], active: usize, level: SubLevel, focused: bool) -> Line<'static> {
    let level = if focused { level } else { SubLevel::Tabs };
    let mut spans = vec![];
    for (i, l) in labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(theme::BORDER)));
        }
        let st = match (i == active, level) {
            (true, SubLevel::Lists) => Style::new().fg(theme::BUTTON_FG).bg(theme::HIGHLIGHT_BG).bold(),
            (true, _) => Style::new().fg(theme::ACCENT).bold(),
            (false, _) => Style::new().fg(theme::MUTED),
        };
        spans.push(Span::styled(format!(" {l} "), st));
    }
    let arrows = match level {
        SubLevel::Tabs if focused => tr!("  ↓ выбрать список", "  ↓ choose a list"),
        SubLevel::Tabs => "",
        SubLevel::Lists => tr!("  ←→ список · ↓ записи · ↑ назад", "  ←→ list · ↓ entries · ↑ back"),
        SubLevel::Entries => "  ←→",
    };
    spans.push(Span::styled(arrows, Style::new().fg(theme::MUTED)));
    Line::from(spans)
}

fn draw_access(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let title = tr!("Доступ", "Access");
    if !s.is_local() {
        return muted_block(
            f,
            area,
            title,
            focused,
            tr!(
                "Списки доступа читаются из файлов сервера — для RCON-серверов они недоступны. Команды whitelist / ban / op можно вводить в строке команды.",
                "Access lists are read from the server files, so they are not available for RCON servers. You can type whitelist / ban / op commands in the command line."
            ),
        );
    }
    let outer = block(title, focused);
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let [tabs_area, info_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let level = if focused { app.access.level } else { SubLevel::Tabs };
    let labels: Vec<String> = crate::access::List::ALL.iter().map(|l| l.title().to_string()).collect();
    let active = crate::access::List::ALL.iter().position(|l| *l == app.access.list).unwrap_or(0);
    f.render_widget(Paragraph::new(sub_tabs_line(&labels, active, app.access.level, focused)), tabs_area);

    let mut info = vec![];
    if app.access.list == crate::access::List::Whitelist {
        let on = app.whitelist_on();
        info.push(Span::styled(tr!("Белый список: ", "Whitelist: "), Style::new().fg(theme::MUTED)));
        info.push(Span::styled(
            if on { tr!("включён", "on") } else { tr!("выключен", "off") },
            Style::new().fg(if on { theme::OK } else { theme::MUTED }).bold(),
        ));
        info.push(Span::styled(tr!(" (w — переключить) ", " (w to toggle) "), Style::new().fg(theme::MUTED)));
    }
    let how = if s.running() {
        tr!("· сервер запущен — изменения отправляются командами", "· server running — changes are sent as commands")
    } else {
        tr!("· сервер остановлен — правится файл", "· server stopped — the file is edited")
    };
    info.push(Span::styled(how, Style::new().fg(theme::MUTED)));
    if app.access.busy {
        info.push(Span::styled(tr!(" · поиск игрока…", " · looking up player…"), Style::new().fg(theme::WARN)));
    }
    f.render_widget(Paragraph::new(Line::from(info)), info_area);

    if let Some(e) = &app.access.error {
        f.render_widget(Paragraph::new(e.clone()).fg(theme::ERROR).wrap(Wrap { trim: true }), list_area);
        return;
    }
    if app.access.entries.is_empty() {
        f.render_widget(
            Paragraph::new(tr!("Список пуст · a — добавить", "The list is empty · a to add")).fg(theme::MUTED),
            list_area,
        );
        return;
    }
    let rows: Vec<Row> = app
        .access
        .entries
        .iter()
        .map(|e| {
            let extra = match (e.level, &e.reason, &e.expires) {
                (Some(l), _, _) => tr!(format!("уровень {l}"), format!("level {l}")),
                (_, Some(r), Some(x)) if x != crate::access::FOREVER => format!("{r} · {} {x}", tr!("до", "until")),
                (_, Some(r), _) => r.clone(),
                _ => String::new(),
            };
            Row::new(vec![
                Span::styled(e.name.clone(), Style::new().fg(theme::TEXT)),
                Span::styled(extra, Style::new().fg(theme::MUTED)),
                Span::styled(e.uuid.clone().unwrap_or_default(), Style::new().fg(theme::MUTED)),
            ])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(20), Constraint::Min(10), Constraint::Length(36)])
        .row_highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold());
    let mut st = TableState::default().with_selected((level == SubLevel::Entries).then_some(app.access.sel));
    f.render_stateful_widget(table, list_area, &mut st);
}

fn draw_addons(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let title = tr!("Дополнения", "Add-ons");
    if !app.sel().is_local() {
        return muted_block(
            f,
            area,
            title,
            focused,
            tr!("Дополнения доступны только для локальных серверов", "Add-ons are only available for local servers"),
        );
    }
    let p = &app.plugins;
    let n_plugins = p.items.iter().filter(|x| x.kind == PluginKind::Plugin).count();
    let n_mods = p.items.len() - n_plugins;
    let n = p.items.len();
    let by_core_mods = p.core == Some(AddonsCore::Mods);
    let plugins_label = match (n_plugins, n_mods) {
        (_, 0) if !by_core_mods => tr!(format!("Плагины ({n})"), format!("Plugins ({n})")),
        (0, _) | (_, 0) => tr!(format!("Моды ({n})"), format!("Mods ({n})")),
        _ => tr!(format!("Плагины и моды ({n})"), format!("Plugins and mods ({n})")),
    };
    let nd = app.datapacks.items.len();
    let labels = vec![plugins_label, tr!(format!("Датапаки ({nd})"), format!("Datapacks ({nd})")).to_string()];
    let active = if app.addons.sub == AddonsSub::Plugins { 0 } else { 1 };
    let outer = block(title, focused);
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let [tabs_area, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    f.render_widget(Paragraph::new(sub_tabs_line(&labels, active, app.addons.level, focused)), tabs_area);
    let entries = focused && app.addons.level == SubLevel::Entries;
    match app.addons.sub {
        AddonsSub::Plugins => draw_plugins(app, f, body, entries),
        AddonsSub::Datapacks => draw_datapacks(app, f, body, entries),
    }
}

fn draw_plugins(app: &App, f: &mut Frame, inner: Rect, entries: bool) {
    let p = &app.plugins;
    let [top, list_area, about] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    if let Some(x) = p.items.get(p.sel) {
        let mut t = x.description.clone().unwrap_or_default();
        if !x.authors.is_empty() {
            t = format!("{t} — {}", x.authors.join(", "));
        }
        f.render_widget(Paragraph::new(t.trim_start_matches(" — ").to_string()).fg(theme::MUTED), about);
    }
    let head = if let Some(b) = &p.busy {
        Line::styled(format!("⏳ {b}"), Style::new().fg(theme::WARN))
    } else if p.checking {
        Line::styled(
            tr!("⏳ проверка обновлений на Modrinth…", "⏳ checking Modrinth for updates…"),
            Style::new().fg(theme::WARN),
        )
    } else if p.checked {
        let n = p.updates.len();
        Line::styled(
            tr!(
                format!("Обновлений: {n} · Enter — обновить выбранный, U — все"),
                format!("Updates: {n} · Enter updates the selected one, U updates all")
            ),
            Style::new().fg(if n > 0 { theme::ACCENT } else { theme::OK }),
        )
    } else {
        Line::styled(
            tr!(
                "Пробел — вкл/выкл · c — проверить обновления (Modrinth)",
                "Space — on/off · c — check for updates (Modrinth)"
            ),
            Style::new().fg(theme::MUTED),
        )
    };
    f.render_widget(Paragraph::new(head), top);
    if p.items.is_empty() {
        let empty = match p.core {
            Some(AddonsCore::Vanilla) => vanilla_addons_text(),
            Some(AddonsCore::Mods) => tr!("Модов нет · i — найти и установить", "No mods · i to find and install"),
            _ => tr!("В папках plugins/ и mods/ ничего нет", "Nothing in plugins/ or mods/"),
        };
        f.render_widget(Paragraph::new(empty).fg(theme::MUTED), list_area);
        return;
    }
    let rows: Vec<Row> = p
        .items
        .iter()
        .map(|x| {
            let state = if x.enabled {
                Span::styled(tr!("вкл", "on"), Style::new().fg(theme::OK))
            } else {
                Span::styled(tr!("выкл", "off"), Style::new().fg(theme::MUTED))
            };
            let upd = p.updates.get(&x.path).map(|u| format!("→ {}", u.version)).unwrap_or_default();
            let name_style = if x.enabled { Style::new().fg(theme::TEXT) } else { Style::new().fg(theme::MUTED) };
            Row::new(vec![
                state,
                Span::styled(x.name.clone(), name_style),
                Span::styled(x.version.clone().unwrap_or_default(), Style::new().fg(theme::MUTED)),
                Span::styled(upd, Style::new().fg(theme::ACCENT).bold()),
                Span::styled(x.file_name.clone(), Style::new().fg(theme::MUTED)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Min(14),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec![
            "",
            tr!("Название", "Name"),
            tr!("Версия", "Version"),
            tr!("Обновление", "Update"),
            tr!("Файл", "File"),
        ])
        .style(Style::new().fg(theme::MUTED)),
    )
    .row_highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold());
    let mut st = TableState::default().with_selected(entries.then_some(p.sel));
    f.render_stateful_widget(table, list_area, &mut st);
}

fn draw_datapacks(app: &App, f: &mut Frame, inner: Rect, entries: bool) {
    let d = &app.datapacks;
    let [top, list_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let head = if let Some(b) = &d.busy {
        Line::styled(format!("⏳ {b}"), Style::new().fg(theme::WARN))
    } else if d.checking {
        Line::styled(
            tr!("⏳ проверка обновлений на Modrinth…", "⏳ checking Modrinth for updates…"),
            Style::new().fg(theme::WARN),
        )
    } else {
        let desc = d.items.get(d.sel).and_then(|x| x.description.clone()).unwrap_or_default();
        Line::styled(desc, Style::new().fg(theme::MUTED))
    };
    f.render_widget(Paragraph::new(head), top);
    if d.items.is_empty() {
        f.render_widget(
            Paragraph::new(tr!("Датапаков нет · i — найти и установить", "No datapacks · i to find and install"))
                .fg(theme::MUTED),
            list_area,
        );
        return;
    }
    // Колонка обновлений — только когда они есть, иначе она отнимает место у описания.
    let with_updates = !d.updates.is_empty();
    let rows: Vec<Row> = d
        .items
        .iter()
        .map(|x| {
            let state = match (x.enabled, x.disabled_in_game) {
                (true, false) => Span::styled(tr!("вкл", "on"), Style::new().fg(theme::OK)),
                (true, true) => Span::styled(tr!("выкл в игре", "off in game"), Style::new().fg(theme::WARN)),
                (false, _) => Span::styled(tr!("выкл", "off"), Style::new().fg(theme::MUTED)),
            };
            let mark = if x.worldgen {
                tr!("мир", "world")
            } else if x.needs_restart {
                tr!("перезапуск", "restart")
            } else {
                ""
            };
            let version = d.versions.get(&x.path).map(String::as_str).unwrap_or("—");
            let name_style = if x.enabled { Style::new().fg(theme::TEXT) } else { Style::new().fg(theme::MUTED) };
            let mut cells = vec![
                Span::styled(x.name.clone(), name_style),
                Span::styled(version.to_string(), Style::new().fg(theme::MUTED)),
                state,
                Span::styled(mark, Style::new().fg(theme::WARN)),
            ];
            if with_updates {
                let upd = d.updates.get(&x.path).map(|u| format!("→ {}", u.version)).unwrap_or_default();
                cells.push(Span::styled(upd, Style::new().fg(theme::ACCENT).bold()));
            }
            cells.push(Span::styled(x.description.clone().unwrap_or_default(), Style::new().fg(theme::MUTED)));
            Row::new(cells)
        })
        .collect();
    let mut widths =
        vec![Constraint::Length(24), Constraint::Length(14), Constraint::Length(12), Constraint::Length(11)];
    if with_updates {
        widths.push(Constraint::Length(14));
    }
    widths.push(Constraint::Min(10));
    let table = Table::new(rows, widths).row_highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold());
    let mut st = TableState::default().with_selected(entries.then_some(d.sel));
    f.render_stateful_widget(table, list_area, &mut st);
}

fn draw_settings(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let mut rows: Vec<Row> = app
        .opts()
        .into_iter()
        .map(|o| {
            Row::new(vec![
                Span::styled(format!("pechka · {}", o.label()), Style::new().fg(theme::ACCENT)),
                Span::styled(app.opt_value(o), Style::new().fg(theme::TEXT)),
            ])
        })
        .collect();
    match &s.props {
        Some(_) => rows.extend(app.prop_rows().into_iter().map(|(k, v)| {
            let doc = crate::propdocs::doc(&k);
            let secret = doc.is_some_and(|d| d.secret) || k.contains("password");
            let shown = if secret && !v.is_empty() { "••••••••".to_string() } else { v.clone() };
            let color = match v.as_str() {
                "true" => theme::OK,
                "false" => theme::MUTED,
                _ => theme::TEXT,
            };
            let key_color = if doc.is_some_and(|d| d.important) { theme::TEXT } else { theme::MUTED };
            Row::new(vec![Span::styled(k, Style::new().fg(key_color)), Span::styled(shown, Style::new().fg(color))])
        })),
        None => {
            let msg = match (&s.props_error, s.cfg.kind) {
                (_, Kind::Rcon) => tr!(
                    "server.properties недоступен для RCON-серверов",
                    "server.properties is not available for RCON servers"
                )
                .to_string(),
                (Some(e), _) => {
                    tr!(format!("server.properties не прочитан: {e}"), format!("could not read server.properties: {e}"))
                }
                (None, _) => tr!("server.properties не найден", "server.properties not found").to_string(),
            };
            rows.push(Row::new(vec![Span::styled(msg, Style::new().fg(theme::MUTED))]));
        }
    }
    let title = if s.needs_restart {
        tr!("Настройки — нужен перезапуск", "Settings — restart needed")
    } else {
        tr!("Настройки", "Settings")
    };
    let outer = block(title, focused);
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let [table_area, help_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    let table = Table::new(rows, [Constraint::Percentage(50), Constraint::Percentage(50)])
        .row_highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold());
    let mut st = TableState::default().with_selected(focused.then_some(app.settings_sel));
    f.render_stateful_widget(table, table_area, &mut st);
    // Описание выбранного параметра
    let help: Line = match app.selected_setting_pub() {
        Some(Err((k, _))) => match crate::propdocs::doc(&k) {
            Some(d) => {
                let mut v = vec![Span::styled(d.description().to_string(), Style::new().fg(theme::TEXT))];
                if d.effect == crate::propdocs::Effect::NewWorld {
                    v.push(Span::styled(
                        tr!("  · действует только для нового мира", "  · only affects a new world"),
                        Style::new().fg(theme::WARN),
                    ));
                }
                if d.legacy {
                    v.push(Span::styled(
                        tr!("  · из старых версий", "  · from older versions"),
                        Style::new().fg(theme::MUTED),
                    ));
                }
                Line::from(v)
            }
            None => Line::styled(tr!("нет описания", "no description"), Style::new().fg(theme::MUTED)),
        },
        Some(Ok(_)) => Line::styled(
            tr!("Настройка pechka (хранится в servers.toml)", "pechka setting (stored in servers.toml)"),
            Style::new().fg(theme::MUTED),
        ),
        None => Line::raw(""),
    };
    if focused {
        f.render_widget(
            Paragraph::new(help).wrap(Wrap { trim: true }).block(
                Block::new().borders(ratatui::widgets::Borders::TOP).border_style(Style::new().fg(theme::BORDER)),
            ),
            help_area,
        );
    }
}

fn draw_backups(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let outer = block(&format!("{} → {}", tr!("Бэкапы", "Backups"), s.cfg.backup_dir().display()), focused);
    if s.cfg.kind == Kind::Rcon {
        f.render_widget(
            Paragraph::new(tr!(
                "Бэкапы доступны только для локальных серверов",
                "Backups are only available for local servers"
            ))
            .fg(theme::MUTED)
            .block(outer),
            area,
        );
        return;
    }
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let [top, list_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    let auto = if s.cfg.backup_every_hours > 0 {
        tr!(
            format!(" · авто: каждые {} ч", s.cfg.backup_every_hours),
            format!(" · auto: every {} h", s.cfg.backup_every_hours)
        )
    } else {
        String::new()
    };
    let mode = if s.cfg.backup_mode == crate::config::BackupMode::Incremental {
        tr!(" · инкрементальные", " · incremental")
    } else {
        ""
    };
    let copies = if s.cfg.backup_copy_to.is_empty() {
        String::new()
    } else {
        tr!(format!(" · копии: {}", s.cfg.backup_copy_to.len()), format!(" · copies: {}", s.cfg.backup_copy_to.len()))
    };
    let head = match &s.backup_progress {
        Some(p) => Line::styled(format!("⏳ {p}"), Style::new().fg(theme::WARN)),
        None => Line::styled(
            tr!(
                format!("Пути: {} · хранить: {}{auto}{mode}{copies}", s.cfg.backup_paths.join(", "), s.cfg.backup_keep),
                format!("Paths: {} · keep: {}{auto}{mode}{copies}", s.cfg.backup_paths.join(", "), s.cfg.backup_keep)
            ),
            Style::new().fg(theme::MUTED),
        ),
    };
    f.render_widget(Paragraph::new(head), top);
    if app.backups.is_empty() {
        f.render_widget(
            Paragraph::new(tr!("Бэкапов пока нет · n — создать", "No backups yet · n to create")).fg(theme::MUTED),
            list_area,
        );
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
            let (icon, size) = match b.kind {
                crate::backup::BackupKind::Full => ("📦", human_bytes(b.size)),
                crate::backup::BackupKind::Incremental => {
                    ("🔗", format!("+{} / {}", human_bytes(b.size), human_bytes(b.total_size)))
                }
            };
            Row::new(vec![format!("{icon} {name}"), when, size])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Min(20), Constraint::Length(17), Constraint::Length(23)])
        .header(
            Row::new(vec![
                tr!("Бэкап", "Backup"),
                tr!("Дата", "Date"),
                tr!("Размер (новое / всего)", "Size (new / total)"),
            ])
            .style(Style::new().fg(theme::MUTED)),
        )
        .row_highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold());
    let mut st = TableState::default().with_selected(focused.then_some(app.backups_sel));
    f.render_stateful_widget(table, list_area, &mut st);
}

fn draw_response(app: &App, f: &mut Frame, area: Rect) {
    let s = app.sel();
    let h = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if s.response.is_empty() {
        vec![Line::styled(
            tr!(
                "Здесь появится ответ на команду. Полный вывод — кнопка «Логи» (L)",
                "Command output appears here. Full output — the “Logs” button (L)"
            ),
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
    f.render_widget(Paragraph::new(lines).block(block(tr!("Ответ", "Output"), false)), area);
}

fn draw_input(app: &App, f: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::Input;
    let s = app.sel();
    let title = if s.accepts_commands() {
        tr!(format!("Команда → {}", s.cfg.name), format!("Command → {}", s.cfg.name))
    } else {
        tr!(format!("Команда → {} (не запущен)", s.cfg.name), format!("Command → {} (not running)", s.cfg.name))
    };
    let b = block(&title, focused);
    let inner = b.inner(area);
    let text = if app.input.value.is_empty() && !focused {
        Line::styled(
            tr!("Enter или : — ввести команду (без /)", "Enter or : — type a command (no /)"),
            Style::new().fg(theme::MUTED),
        )
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
    if app.focus == Focus::Input
        && let Some(h) = &app.completion_hint
    {
        let line = Line::from(vec![
            Span::styled(" Tab: ", Style::new().fg(theme::MUTED)),
            Span::styled(h.clone(), Style::new().fg(theme::ACCENT)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let tab_keys = match (app.focus, app.tab) {
        (Focus::Input, _) => tr!(
            "Enter отправить · Tab дополнить · ↑↓ история · Esc выйти",
            "Enter send · Tab complete · ↑↓ history · Esc leave"
        ),
        (Focus::Content, Tab::Players) => tr!(
            "↑↓ выбор · k кик · b бан · o/d оп/деоп · w в белый список · s статистика",
            "↑↓ select · k kick · b ban · o/d op/deop · w whitelist · s statistics"
        ),
        (Focus::Content, Tab::Access) => match app.access.level {
            SubLevel::Tabs => tr!(
                "←→ вкладки · ↓ к спискам · a добавить · w вкл/выкл белый список · u обновить",
                "←→ tabs · ↓ to the lists · a add · w whitelist on/off · u reload"
            ),
            SubLevel::Lists => tr!(
                "←→ список · ↓ к записям · ↑ к вкладкам · a добавить · w вкл/выкл белый список · u обновить",
                "←→ list · ↓ to entries · ↑ to tabs · a add · w whitelist on/off · u reload"
            ),
            SubLevel::Entries => tr!(
                "↑↓ выбор · ←→ список · a добавить · d удалить · w вкл/выкл белый список · u обновить",
                "↑↓ select · ←→ list · a add · d remove · w whitelist on/off · u reload"
            ),
        },
        // Клавиши одинаковы на обеих подвкладках: каждая из перечисленных работает и с плагинами, и с датапаками.
        (Focus::Content, Tab::Addons) => match app.addons.level {
            SubLevel::Tabs => tr!(
                "←→ вкладки · ↓ к спискам · i найти и установить · c обновления · U обновить все · u перечитать",
                "←→ tabs · ↓ to the lists · i find & install · c updates · U update all · u rescan"
            ),
            SubLevel::Lists => tr!(
                "←→ плагины/датапаки · ↓ к записям · ↑ к вкладкам · i найти и установить · c обновления · U обновить все · u перечитать",
                "←→ plugins/datapacks · ↓ to entries · ↑ to tabs · i find & install · c updates · U update all · u rescan"
            ),
            SubLevel::Entries => tr!(
                "↑↓ выбор · ←→ плагины/датапаки · i найти и установить · Пробел вкл/выкл · c обновления · Enter/U обновить · u перечитать",
                "↑↓ select · ←→ plugins/datapacks · i find & install · Space on/off · c updates · Enter/U update · u rescan"
            ),
        },
        (Focus::Content, Tab::Settings) => tr!(
            "↑↓ выбор · Enter изменить · Пробел вкл/выкл · r включить RCON · u перечитать",
            "↑↓ select · Enter edit · Space toggle · r enable RCON · u reload"
        ),
        (Focus::Content, Tab::Backups) => tr!(
            "↑↓ выбор · n новый · r восстановить · d удалить · u обновить",
            "↑↓ select · n new · r restore · d delete · u refresh"
        ),
        (Focus::Content, Tab::World) => tr!(
            "↑↓ выбор · R сбросить Незер/Энд · p сгенерировать чанки · P/C пауза/продолжить · x отменить · Enter сменить мир · u обновить",
            "↑↓ select · R reset Nether/End · p pregenerate · P/C pause/continue · x cancel · Enter switch world · u rescan"
        ),
        (Focus::Content, Tab::Status) => tr!(
            "f как друзьям зайти · U обновить ядро · T тестовая копия · J Java и память · v сеть Velocity · Enter команда",
            "f how friends join · U update core · T test copy · J Java & memory · v Velocity network · Enter command"
        ),
        _ => tr!(
            "↑↓ сервер · a добавить · Del убрать · Tab фокус · 1-7 вкладки · f как друзьям зайти",
            "↑↓ server · a add · Del remove · Tab focus · 1-7 tabs · f how friends join"
        ),
    };
    let line = Line::from(vec![
        Span::styled(format!(" {tab_keys} "), Style::new().fg(theme::TEXT)),
        Span::styled(
            tr!(
                "│ ? справка ^S старт ^X стоп ^R рестарт ^B бэкап ^L логи ^Q выход",
                "│ ? help ^S start ^X stop ^R restart ^B backup ^L logs ^Q quit"
            ),
            Style::new().fg(theme::MUTED),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_world(app: &App, f: &mut Frame, area: Rect, focused: bool) {
    let s = app.sel();
    let title = tr!("Мир", "World");
    if s.cfg.kind == Kind::Rcon {
        return muted_block(
            f,
            area,
            title,
            focused,
            tr!("Мир доступен только для локальных серверов", "The world is only available for local servers"),
        );
    }
    let w = &app.world;
    let Some(info) = &w.info else {
        let msg = if w.loading { tr!("Считаю размер мира…", "Measuring the world…") } else { "" };
        return muted_block(f, area, title, focused, msg);
    };
    let outer = block(&format!("{title} · {}", info.level_name), focused);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let mut head: Vec<Line> = vec![];
    if !info.exists {
        head.push(Line::styled(
            tr!(
                "Мир ещё не создан — он появится при первом запуске сервера",
                "The world has not been created yet — it appears on the first server start"
            ),
            Style::new().fg(theme::WARN),
        ));
    } else {
        let layout = match info.layout {
            Some(crate::world::Layout::Modern) => tr!("формат 26.x", "26.x layout"),
            Some(crate::world::Layout::Vanilla) => tr!("формат vanilla", "vanilla layout"),
            Some(crate::world::Layout::Bukkit) => tr!("формат Bukkit", "Bukkit layout"),
            None => "",
        };
        let mut parts = vec![format!("{} {}", tr!("Всего:", "Total:"), human_bytes(info.total_bytes))];
        if let Some(v) = &info.version {
            parts.push(format!("Minecraft {v}"));
        }
        if !layout.is_empty() {
            parts.push(layout.into());
        }
        let seed = info
            .seed
            .map(|x| x.to_string())
            .or_else(|| s.prop("level-seed").filter(|x| !x.is_empty()).map(str::to_string));
        if let Some(seed) = seed {
            parts.push(format!("{} {seed}", tr!("сид", "seed")));
        }
        if let Some(t) = info.last_played {
            parts.push(format!(
                "{} {}",
                tr!("сохранён", "saved"),
                chrono::DateTime::<chrono::Local>::from(t).format("%d.%m %H:%M")
            ));
        }
        head.push(Line::styled(parts.join(" · "), Style::new().fg(theme::TEXT)));
    }
    let chunky = match &w.chunky {
        Some(_) => Line::styled(
            tr!("Chunky установлен — p запускает генерацию чанков", "Chunky is installed — p starts pregeneration"),
            Style::new().fg(theme::MUTED),
        ),
        None => Line::styled(
            tr!(
                "Для генерации чанков нужен Chunky — p предложит установить",
                "Pregeneration needs Chunky — p offers to install it"
            ),
            Style::new().fg(theme::MUTED),
        ),
    };
    head.push(chunky);
    if let Some(p) = app.pregen_progress() {
        let text = if p.finished {
            let t = p.total_time.map(human_duration).unwrap_or_default();
            tr!(
                format!("✓ Генерация {} завершена: {} чанков за {t}", p.world, p.chunks),
                format!("✓ Pregeneration of {} finished: {} chunks in {t}", p.world, p.chunks)
            )
        } else {
            let eta = p.eta.map(human_duration).unwrap_or_else(|| "?".into());
            let rate = p.rate.map(|r| format!(" · {r:.0} {}", tr!("чанк/с", "chunks/s"))).unwrap_or_default();
            tr!(
                format!("⏳ Генерация {}: {:.1}% · {} чанков · осталось {eta}{rate}", p.world, p.percent, p.chunks),
                format!("⏳ Pregenerating {}: {:.1}% · {} chunks · {eta} left{rate}", p.world, p.percent, p.chunks)
            )
        };
        head.push(Line::styled(text, Style::new().fg(if p.finished { theme::OK } else { theme::WARN })));
    }
    let [top, list_area] =
        Layout::vertical([Constraint::Length(head.len() as u16 + 1), Constraint::Min(1)]).areas(inner);
    f.render_widget(Paragraph::new(head), top);

    let rows: Vec<Row> = w
        .rows()
        .into_iter()
        .map(|r| match r {
            WorldRow::Dim(d) => {
                let (size, regions) = if d.exists {
                    (human_bytes(d.size_bytes), d.region_files.to_string())
                } else {
                    (tr!("не создан", "not created").into(), String::new())
                };
                let style = if d.exists { Style::new().fg(theme::TEXT) } else { Style::new().fg(theme::MUTED) };
                Row::new(vec![format!("🌍 {}", d.dim.title()), size, regions]).style(style)
            }
            WorldRow::Other(name) => Row::new(vec![
                format!("🗺  {name}"),
                tr!("другой мир", "other world").into(),
                tr!("Enter — сделать основным", "Enter — make it main").into(),
            ])
            .style(Style::new().fg(theme::MUTED)),
        })
        .collect();
    let table = Table::new(rows, [Constraint::Min(18), Constraint::Length(14), Constraint::Min(8)])
        .header(
            Row::new(vec![tr!("Измерение", "Dimension"), tr!("Размер", "Size"), tr!("Регионов", "Regions")])
                .style(Style::new().fg(theme::MUTED)),
        )
        .row_highlight_style(Style::new().bg(theme::HIGHLIGHT_BG).bold());
    let mut st = TableState::default().with_selected(focused.then_some(w.sel));
    f.render_stateful_widget(table, list_area, &mut st);
}
