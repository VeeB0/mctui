//! Экран добавления сервера.

use super::layout;
use super::theme;
use crate::app::App;
use crate::setup::{Field, SetupKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

const LABEL_W: u16 = 16;

pub fn draw(app: &App, f: &mut Frame) {
    let Some(s) = &app.setup else { return };
    let area = f.area();
    let panel = layout::setup_panel(area);
    f.render_widget(Clear, panel);
    let title =
        if s.first_run { " Добро пожаловать в mctui " } else { " Добавить сервер " };
    let block = Block::bordered()
        .title(title)
        .title_style(Style::new().fg(theme::ACCENT).bold())
        .border_style(Style::new().fg(theme::BORDER_FOCUS));
    let inner = block.inner(panel);
    f.render_widget(block, panel);

    let intro = match s.kind {
        SetupKind::Local => {
            "Вставьте путь к папке сервера Minecraft (Cmd+V / Ctrl+V или перетащите папку в окно терминала). mctui сам найдёт, как его запускать."
        }
        SetupKind::Rcon => "Подключение к уже работающему серверу по RCON (в его server.properties: enable-rcon=true).",
    };
    f.render_widget(
        Paragraph::new(intro).wrap(Wrap { trim: true }).style(Style::new().fg(theme::TEXT)),
        Rect { height: 3, ..inner },
    );

    let rows = layout::setup_fields(area, s.fields().len());
    for (i, (field, row)) in s.fields().iter().zip(rows.iter()).enumerate() {
        let active = i == s.field;
        let label_style = if active { Style::new().fg(theme::ACCENT).bold() } else { Style::new().fg(theme::MUTED) };
        let marker = if active { "▶ " } else { "  " };
        let label = Span::styled(format!("{marker}{:<w$}", field.label(), w = LABEL_W as usize - 2), label_style);
        let value_area = Rect { x: row.x + LABEL_W, width: row.width.saturating_sub(LABEL_W), ..*row };
        f.render_widget(Paragraph::new(label), Rect { width: LABEL_W, ..*row });
        if *field == Field::Kind {
            let opt = |on: bool, text: &str| {
                if on {
                    Span::styled(format!(" ● {text} "), Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold())
                } else {
                    Span::styled(format!(" ○ {text} "), Style::new().fg(theme::MUTED))
                }
            };
            let line = Line::from(vec![
                opt(s.kind == SetupKind::Local, "Локальный (запускать здесь)"),
                Span::raw("  "),
                opt(s.kind == SetupKind::Rcon, "Удалённый (RCON)"),
                Span::styled(if active { "   ←→" } else { "" }, Style::new().fg(theme::MUTED)),
            ]);
            f.render_widget(Paragraph::new(line), value_area);
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
                    "/Users/вы/minecraft/server"
                }
            }
            Field::Host if shown.is_empty() => "127.0.0.1 (этот компьютер) или IP / домен",
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

    // Результат проверки папки / ошибки формы
    let below = rows.last().map(|r| r.y + 2).unwrap_or(inner.y + 5);
    let mut lines: Vec<Line> = vec![];
    if s.kind == SetupKind::Local {
        let d = &s.detection;
        if let Some(e) = &d.error {
            if !s.path.value.trim().is_empty() {
                lines.push(Line::styled(format!("✗ {e}"), Style::new().fg(theme::ERROR)));
            }
        } else {
            if let Some(dir) = &d.dir {
                lines.push(Line::styled(format!("✓ Папка: {}", dir.display()), Style::new().fg(theme::OK)));
            }
            for x in &d.found {
                lines.push(Line::styled(format!("✓ {x}"), Style::new().fg(theme::OK)));
            }
            lines.push(Line::from(vec![
                Span::styled("  Команда запуска: ", Style::new().fg(theme::MUTED)),
                Span::styled(d.command.join(" "), Style::new().fg(theme::TEXT)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  Бэкапить: ", Style::new().fg(theme::MUTED)),
                Span::styled(d.backup_paths.join(", "), Style::new().fg(theme::TEXT)),
            ]));
            for w in &d.warnings {
                lines.push(Line::styled(format!("! {w}"), Style::new().fg(theme::WARN)));
            }
        }
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

    let esc = if s.first_run { "Esc — выйти" } else { "Esc — отмена" };
    let footer = vec![
        Line::from(vec![
            Span::styled(" Enter ", Style::new().fg(theme::BUTTON_FG).bg(theme::BUTTON_BG).bold()),
            Span::styled(" сохранить   ", Style::new().fg(theme::TEXT)),
            Span::styled(format!("Tab/↑↓ поля · ←→ на «Тип» · {esc}"), Style::new().fg(theme::MUTED)),
        ]),
        Line::styled(format!("Настройки: {}", app.config_path.display()), Style::new().fg(theme::MUTED)),
    ];
    f.render_widget(
        Paragraph::new(footer),
        Rect { x: inner.x + 1, y: inner.bottom().saturating_sub(2), width: inner.width.saturating_sub(2), height: 2 },
    );
}
