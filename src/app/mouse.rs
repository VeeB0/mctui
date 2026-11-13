//! Мышь и вставка из буфера обмена.

use super::*;

impl App {
    pub fn on_mouse(&mut self, m: MouseEvent) {
        if self.popup.is_some() {
            return;
        }
        let pos = Position::new(m.column, m.row);
        match self.screen {
            Screen::Setup => {
                if let (MouseEventKind::Down(MouseButton::Left), Some(setup)) = (m.kind, &mut self.setup) {
                    let rows = layout::setup_fields(self.area, setup.fields().len());
                    if let Some(i) = rows.iter().position(|r| r.contains(pos)) {
                        setup.focus(i);
                    }
                }
            }
            Screen::Main => {
                if !self.has_server() {
                    return;
                }
                let l = layout::main(self.area);
                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if self.update_badge.get().contains(pos) {
                            self.self_update();
                        } else if l.logs_btn.contains(pos) {
                            self.toggle_logs();
                        } else if l.tabs.contains(pos) {
                            if let Some(t) = layout::tab_at(l.tabs, m.column) {
                                self.set_tab(TABS[t]);
                                self.set_focus(Focus::Content);
                            }
                        } else if l.sidebar.contains(pos) {
                            self.set_focus(Focus::Sidebar);
                            let row = m.row.saturating_sub(l.sidebar.y + 1) as usize;
                            if row < self.servers.len() && row != self.selected {
                                self.selected = row;
                                self.on_select_changed();
                            }
                        } else if l.input.contains(pos) {
                            self.set_focus(Focus::Input);
                        } else if l.content.contains(pos) {
                            self.set_focus(Focus::Content);
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        if l.sidebar.contains(pos) {
                            self.set_focus(Focus::Sidebar)
                        } else if l.content.contains(pos) {
                            self.set_focus(Focus::Content)
                        }
                        self.move_selection(if m.kind == MouseEventKind::ScrollUp { -1 } else { 1 })
                    }
                    _ => {}
                }
            }
            Screen::Logs => {
                let l = layout::logs(self.area);
                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if l.back_btn.contains(pos) {
                            self.screen = Screen::Main;
                        } else if l.sources.contains(pos) {
                            let row = m.row.saturating_sub(l.sources.y + 1) as usize;
                            if row < self.logs.sources.len() {
                                self.open_log_source(row);
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => self.scroll_logs(3),
                    MouseEventKind::ScrollDown => self.scroll_logs(-3),
                    _ => {}
                }
            }
        }
    }

    pub fn on_paste(&mut self, s: &str) {
        if let Some(Popup::Edit { input, .. }) = &mut self.popup {
            input.insert_str(s.trim_end_matches(['\n', '\r']));
        } else if self.screen == Screen::Setup && self.popup.is_none() {
            if let Some(setup) = &mut self.setup {
                setup.paste(s);
            }
        } else if let Some(inp) = &mut self.logs.search_input {
            inp.insert_str(s);
        } else if self.screen == Screen::Main && self.has_server() {
            self.set_focus(Focus::Input);
            self.input.insert_str(s);
        }
    }
}
