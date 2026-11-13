//! Раскладка экранов. Используется и при отрисовке, и для попаданий мыши.

use crate::app::TABS;
use ratatui::layout::{Constraint, Layout, Rect};

pub fn logs_btn() -> &'static str {
    crate::tr!(" L · Логи ", " L · Logs ")
}
pub fn back_btn() -> &'static str {
    crate::tr!(" Esc · Назад ", " Esc · Back ")
}
pub const SIDEBAR_W: u16 = 26;

pub fn width(s: &str) -> u16 {
    s.chars().count() as u16
}

pub struct Main {
    pub header: Rect,
    pub logs_btn: Rect,
    pub sidebar: Rect,
    pub tabs: Rect,
    pub content: Rect,
    pub response: Rect,
    pub input: Rect,
    pub hints: Rect,
}

pub fn main(area: Rect) -> Main {
    let [header, body, response, input, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(6),
        Constraint::Length(6),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);
    let [sidebar, right] = Layout::horizontal([Constraint::Length(SIDEBAR_W), Constraint::Min(20)]).areas(body);
    let [tabs, content] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(right);
    Main { header, logs_btn: button_rect(header, logs_btn()), sidebar, tabs, content, response, input, hints }
}

pub struct Logs {
    pub header: Rect,
    pub back_btn: Rect,
    pub sources: Rect,
    pub view: Rect,
    pub footer: Rect,
    pub hints: Rect,
}

pub fn logs(area: Rect) -> Logs {
    let [header, body, footer, hints] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)])
            .areas(area);
    let [sources, view] = Layout::horizontal([Constraint::Length(SIDEBAR_W + 4), Constraint::Min(20)]).areas(body);
    Logs { header, back_btn: button_rect(header, back_btn()), sources, view, footer, hints }
}

fn button_rect(header: Rect, label: &str) -> Rect {
    let w = width(label).min(header.width);
    Rect { x: header.right().saturating_sub(w + 1), y: header.y, width: w, height: 1 }
}

/// Окно экрана добавления сервера.
pub fn setup_panel(area: Rect) -> Rect {
    let w = area.width.saturating_sub(4).min(100);
    let h = area.height.saturating_sub(2).min(30);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// Строки полей формы (по одной строке на поле, с отступом от заголовка).
pub fn setup_fields(area: Rect, n: usize) -> Vec<Rect> {
    let p = setup_panel(area);
    (0..n as u16)
        .map(|i| Rect { x: p.x + 2, y: p.y + 5 + i * 2, width: p.width.saturating_sub(4), height: 1 })
        .filter(|r| r.y < p.bottom().saturating_sub(1))
        .collect()
}

/// Заголовок одной вкладки: " 1 Статус ".
pub fn tab_label(i: usize) -> String {
    format!(" {} {} ", i + 1, TABS[i].title())
}

/// Заголовки вкладок под ширину `avail`: полные, а если не помещаются — у неактивной остаётся только " N ".
/// Одно правило для отрисовки и для попаданий мыши.
pub fn tab_labels(avail: u16, active: usize) -> Vec<String> {
    let full: Vec<String> = (0..TABS.len()).map(tab_label).collect();
    let total = full.iter().map(|l| width(l)).sum::<u16>() + TABS.len() as u16 - 1; // + разделители
    if total <= avail {
        return full;
    }
    full.into_iter().enumerate().map(|(i, l)| if i == active { l } else { format!(" {} ", i + 1) }).collect()
}

pub fn tab_at(tabs: Rect, x: u16, active: usize) -> Option<usize> {
    let mut cur = tabs.x;
    for (i, label) in tab_labels(tabs.width, active).iter().enumerate() {
        let w = width(label);
        if x >= cur && x < cur + w {
            return Some(i);
        }
        cur += w + 1; // разделитель
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_button_is_inside_header_right() {
        let m = main(Rect::new(0, 0, 100, 30));
        assert_eq!(m.logs_btn.y, 0);
        assert_eq!(m.logs_btn.right(), 99);
        assert_eq!(tab_at(m.tabs, m.tabs.x, 0), Some(0));
        let second = m.tabs.x + width(&tab_label(0)) + 1;
        assert_eq!(tab_at(m.tabs, second, 0), Some(1));
    }

    #[test]
    fn tabs_go_compact_when_they_do_not_fit() {
        let line = |labels: &[String]| labels.iter().map(|l| width(l)).sum::<u16>() + labels.len() as u16 - 1;
        // Широко: ничего не меняется.
        let wide = tab_labels(200, 3);
        assert_eq!(wide, (0..TABS.len()).map(tab_label).collect::<Vec<_>>());
        // 80 колонок минус боковая панель и кнопка «Логи» — тесно: активная полная, остальные " N ".
        let narrow = tab_labels(56, 3);
        assert!(line(&narrow) <= 56, "{narrow:?}");
        assert_eq!(narrow[3], tab_label(3));
        assert_eq!(narrow[0], " 1 ");
        assert_eq!(narrow[6], " 7 ");
        // Мышь попадает по тем же подписям: и в компактную, и в активную.
        let tabs = Rect::new(10, 1, 56, 1);
        assert_eq!(tab_at(tabs, 10, 3), Some(0));
        assert_eq!(tab_at(tabs, 10 + 3 + 1, 3), Some(1)); // " 1 " + "│"
        let active_x = 10 + 3 * (3 + 1);
        assert_eq!(tab_at(tabs, active_x, 3), Some(3));
        assert_eq!(tab_at(tabs, active_x + width(&tab_label(3)) - 1, 3), Some(3));
        assert_eq!(tab_at(tabs, active_x + width(&tab_label(3)) + 1, 3), Some(4));
    }
}
