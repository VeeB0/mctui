//! Экран добавления сервера.

use super::layout;
use super::theme;
use crate::app::App;
use crate::create::Core;
use crate::setup::{Field, Setup, SetupKind, Versions};
use crate::tr;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

const LABEL_W: u16 = 16;

fn opt(on: bool, text: &str) -> Span<'static> {
    if on {
        Span::styled(format!(" ● {text} "), Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold())
    } else {
        Span::styled(format!(" ○ {text} "), Style::new().fg(theme::MUTED))
    }
}

fn choice_line(s: &Setup, field: Field, active: bool) -> Line<'static> {
    let arrows = Span::styled(if active { "   ←→" } else { "" }, Style::new().fg(theme::MUTED));
    match field {
        Field::Kind => {
            let mut v = vec![];
            for k in SetupKind::ALL {
                v.push(opt(s.kind == k, k.label()));
                v.push(Span::raw(" "));
            }
            v.push(arrows);
            Line::from(v)
        }
        Field::Core => {
            let mut v = vec![];
            for c in Core::WITH_PROXY {
                v.push(opt(s.core == c, c.label()));
                v.push(Span::raw(" "));
            }
            v.push(arrows);
            Line::from(v)
        }
        Field::Version => match s.versions.get(&s.core) {
            None | Some(Versions::Loading) => {
                Line::styled(tr!("загрузка списка версий…", "loading versions…"), Style::new().fg(theme::WARN))
            }
            Some(Versions::Failed(e)) => Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERROR)),
            Some(Versions::Ready(v)) if v.is_empty() => {
                Line::styled(tr!("нет версий", "no versions"), Style::new().fg(theme::ERROR))
            }
            Some(Versions::Ready(v)) => {
                let cur = v.get(s.version_sel).cloned().unwrap_or_default();
                let latest = if s.version_sel == 0 { tr!("  (последняя)", "  (latest)") } else { "" };
                Line::from(vec![
                    Span::styled(" ◀ ", Style::new().fg(theme::MUTED)),
                    Span::styled(cur, Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold()),
                    Span::styled(" ▶ ", Style::new().fg(theme::MUTED)),
                    Span::styled(format!("{}/{}{latest}", s.version_sel + 1, v.len()), Style::new().fg(theme::MUTED)),
                    Span::styled(
                        if active { tr!("   ←→ · PgUp/PgDn", "   ←→ · PgUp/PgDn") } else { "" },
                        Style::new().fg(theme::MUTED),
                    ),
                ])
            }
        },
        Field::Eula => Line::from(vec![
            Span::styled(
                if s.eula { " [✓] " } else { " [ ] " },
                Style::new().fg(if s.eula { theme::OK } else { theme::WARN }).bold(),
            ),
            Span::styled(tr!("Принимаю EULA Minecraft ", "I accept the Minecraft EULA "), Style::new().fg(theme::TEXT)),
            Span::styled(crate::create::EULA_URL, Style::new().fg(theme::ACCENT)),
            Span::styled(if active { tr!("   пробел", "   Space") } else { "" }, Style::new().fg(theme::MUTED)),
        ]),
        _ => Line::raw(""),
    }
}

pub fn draw(app: &App, f: &mut Frame) {
    let Some(s) = &app.setup else { return };
    let area = f.area();
    let panel = layout::setup_panel(area);
    f.render_widget(Clear, panel);
    let title = if s.first_run {
        tr!(" Добро пожаловать в mctui ", " Welcome to mctui ")
    } else {
        tr!(" Добавить сервер ", " Add a server ")
    };
    let block = Block::bordered()
        .title(title)
        .title_style(Style::new().fg(theme::ACCENT).bold())
        .border_style(Style::new().fg(theme::BORDER_FOCUS));
    let inner = block.inner(panel);
    f.render_widget(block, panel);

    let intro = match s.kind {
        SetupKind::Local => tr!(
            "Вставьте путь к папке сервера Minecraft (Cmd+V / Ctrl+V или перетащите папку в окно терминала). mctui сам найдёт, как его запускать.",
            "Paste the path to a Minecraft server folder (Cmd+V / Ctrl+V, or drag the folder into the terminal). mctui works out how to start it."
        ),
        SetupKind::New => tr!(
            "Новый сервер с нуля: mctui скачает ядро и, если нужно, Java, примет EULA и создаст скрипт запуска.",
            "A new server from scratch: mctui downloads the core and Java if needed, accepts the EULA and writes a start script."
        ),
        SetupKind::Rcon => tr!(
            "Подключение к уже работающему серверу по RCON (в его server.properties: enable-rcon=true).",
            "Connect to a running server over RCON (its server.properties needs enable-rcon=true)."
        ),
    };
    f.render_widget(
        Paragraph::new(intro).wrap(Wrap { trim: true }).style(Style::new().fg(theme::TEXT)),
        Rect { height: 3, ..inner },
    );

    let rows = layout::setup_fields(area, s.fields().len());
    for (i, (field, row)) in s.fields().iter().zip(rows.iter()).enumerate() {
        let active = i == s.field && s.creating.is_none();
        let label_style = if active { Style::new().fg(theme::ACCENT).bold() } else { Style::new().fg(theme::MUTED) };
        let marker = if active { "▶ " } else { "  " };
        let label = Span::styled(format!("{marker}{:<w$}", field.label(), w = LABEL_W as usize - 2), label_style);
        let value_area = Rect { x: row.x + LABEL_W, width: row.width.saturating_sub(LABEL_W), ..*row };
        f.render_widget(Paragraph::new(label), Rect { width: LABEL_W, ..*row });
        if field.is_choice() {
            f.render_widget(Paragraph::new(choice_line(s, *field, active)), value_area);
            continue;
        }
        let inp = s.input(*field).unwrap();
        let shown = if *field == Field::Password { "•".repeat(inp.value.chars().count()) } else { inp.value.clone() };
        let bg = if active { theme::HIGHLIGHT_BG } else { theme::SURFACE };
        let placeholder = match field {
            Field::Path if shown.is_empty() => {
                if cfg!(windows) {
                    r"C:\minecraft\server"
                } else {
                    tr!("/Users/вы/minecraft/server", "/Users/you/minecraft/server")
                }
            }
            Field::Host if shown.is_empty() => {
                tr!("127.0.0.1 (этот компьютер) или IP / домен", "127.0.0.1 (this computer) or an IP / domain")
            }
            _ => "",
        };
        let text = if shown.is_empty() {
            Span::styled(placeholder.to_string(), Style::new().fg(theme::MUTED).bg(bg))
        } else {
            Span::styled(shown, Style::new().fg(theme::TEXT).bg(bg))
        };
        let cur = inp.cursor() as u16;
        let scroll = cur.saturating_sub(value_area.width.saturating_sub(1));
        f.render_widget(Paragraph::new(Line::from(text)).style(Style::new().bg(bg)).scroll((0, scroll)), value_area);
        if active && app.popup.is_none() {
            f.set_cursor_position((value_area.x + cur - scroll, value_area.y));
        }
    }

    // Результат проверки папки / ход создания / ошибки формы
    let below = rows.last().map(|r| r.y + 2).unwrap_or(inner.y + 5);
    let mut lines: Vec<Line> = vec![];
    match s.kind {
        SetupKind::Local => {
            let d = &s.detection;
            if let Some(e) = &d.error {
                if !s.path.value.trim().is_empty() {
                    lines.push(Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERROR)));
                }
            } else {
                if let Some(dir) = &d.dir {
                    lines.push(Line::styled(
                        format!("✓ {}: {}", tr!("Папка", "Folder"), dir.display()),
                        Style::new().fg(theme::OK),
                    ));
                }
                for x in &d.found {
                    lines.push(Line::styled(format!("✓ {x}"), Style::new().fg(theme::OK)));
                }
                lines.push(Line::from(vec![
                    Span::styled(tr!("  Команда запуска: ", "  Start command: "), Style::new().fg(theme::MUTED)),
                    Span::styled(d.command.join(" "), Style::new().fg(theme::TEXT)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(tr!("  Бэкапить: ", "  Back up: "), Style::new().fg(theme::MUTED)),
                    Span::styled(d.backup_paths.join(", "), Style::new().fg(theme::TEXT)),
                ]));
                for w in &d.warnings {
                    lines.push(Line::styled(format!("! {w}"), Style::new().fg(theme::WARN)));
                }
            }
        }
        SetupKind::New => {
            lines.push(Line::styled(
                format!("{} — {}", s.core.label(), s.core.description()),
                Style::new().fg(theme::MUTED),
            ));
            if let Some(p) = &s.creating {
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!("⏳ {p}"), Style::new().fg(theme::WARN).bold()));
            }
        }
        SetupKind::Rcon => {}
    }
    if let Some(e) = &s.error {
        lines.push(Line::raw(""));
        lines.push(Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERROR).bold()));
    }
    let footer_h = 3;
    let info = Rect {
        x: inner.x + 2,
        y: below,
        width: inner.width.saturating_sub(4),
        height: inner.bottom().saturating_sub(below + footer_h),
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info);

    let esc = if s.first_run {
        tr!("Esc — выйти", "Esc — quit")
    } else {
        tr!("Esc — отмена", "Esc — cancel")
    };
    let action = if s.kind == SetupKind::New {
        tr!(" создать   ", " create   ")
    } else {
        tr!(" сохранить   ", " save   ")
    };
    let footer = vec![
        Line::from(vec![
            Span::styled(" Enter ", Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold()),
            Span::styled(action, Style::new().fg(theme::TEXT)),
            Span::styled(
                tr!(format!("Tab/↑↓ поля · ←→ выбор · {esc}"), format!("Tab/↑↓ fields · ←→ choose · {esc}")),
                Style::new().fg(theme::MUTED),
            ),
        ]),
        Line::styled(
            tr!(
                format!("Настройки: {}", app.config_path.display()),
                format!("Settings: {}", app.config_path.display())
            ),
            Style::new().fg(theme::MUTED),
        ),
    ];
    f.render_widget(
        Paragraph::new(footer),
        Rect { x: inner.x + 1, y: inner.bottom().saturating_sub(2), width: inner.width.saturating_sub(2), height: 2 },
    );
}
