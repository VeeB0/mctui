//! Вкладка «Доступ»: белый список, операторы, баны игроков и IP.

use super::sub_tabs::{Arrow, sub_arrow};
use super::*;

impl App {
    pub(super) fn access_mtime(&self) -> Option<SystemTime> {
        let dir = self.sel().cfg.dir.clone()?;
        std::fs::metadata(self.access.list.path(&dir)).and_then(|m| m.modified()).ok()
    }

    /// Перечитать список доступа, если сменился сервер/список или файл изменился.
    pub(super) fn refresh_access(&mut self, force: bool) {
        if !self.has_server() || !self.sel().is_local() {
            self.access.entries.clear();
            self.access.loaded = None;
            return;
        }
        let key = (self.sel().id, self.access.list, self.access_mtime());
        if !force && self.access.loaded == Some(key) {
            return;
        }
        self.access.loaded = Some(key);
        let dir = self.sel().cfg.dir().to_path_buf();
        match access::read(&dir, self.access.list) {
            Ok(e) => {
                self.access.entries = e;
                self.access.error = None;
            }
            Err(e) => {
                self.access.entries.clear();
                self.access.error = Some(format!("{e:#}"));
            }
        }
        self.access.sel = self.access.sel.min(self.access.entries.len().saturating_sub(1));
        if self.access.level == SubLevel::Entries && self.access.entries.is_empty() {
            self.access.level = SubLevel::Lists;
        }
    }

    /// Стрелки на вкладке «Доступ» (фокус на содержимом). ↓ спускается: вкладки окна → списки → записи,
    /// ↑ поднимается обратно. На уровне вкладок ←→ листают вкладки окна, ниже — списки.
    /// `false` — клавиша не наша, её обработает общий код (←→ по вкладкам, ↑↓ по записям).
    pub(super) fn access_arrow(&mut self, code: KeyCode) -> bool {
        if !self.sel().is_local() {
            return false;
        }
        let a = &mut self.access;
        match sub_arrow(&mut a.level, &mut a.sel, a.entries.len(), code) {
            Arrow::NotMine => false,
            Arrow::Handled => true,
            Arrow::Cycle(d) => {
                self.cycle_access_list(d);
                true
            }
        }
    }

    pub(super) fn cycle_access_list(&mut self, d: isize) {
        let all = List::ALL;
        let i = all.iter().position(|l| *l == self.access.list).unwrap_or(0) as isize;
        self.access.list = all[(i + d).rem_euclid(all.len() as isize) as usize];
        self.access.sel = 0;
        self.refresh_access(true);
    }

    pub(super) fn access_add(&mut self, list: List, value: &str) {
        let name = value.trim().to_string();
        let ok = if list.is_ip() { access::valid_ip(&name) } else { access::valid_name(&name) };
        if !ok {
            let msg = if list.is_ip() {
                tr!("неверный IP-адрес", "invalid IP address")
            } else {
                tr!("ник — 3–16 символов: буквы, цифры, _", "a name is 3–16 characters: letters, digits, _")
            };
            return self.toast(msg, ToastKind::Error);
        }
        let s = self.sel();
        if s.running() {
            let cmd = access::command(list, true, &name, None);
            let id = s.id;
            self.send(Request::Command { id, cmd, user: false });
            return;
        }
        let dir = s.cfg.dir().to_path_buf();
        let online = s.prop("online-mode") != Some("false");
        self.access.busy = true;
        self.spawn_task(
            move || access::add(&dir, list, &name, None, online).map(|()| name).map_err(|e| format!("{e:#}")),
            |app, res| {
                app.access.busy = false;
                match res {
                    Ok(name) => app.toast(tr!(format!("{name} добавлен"), format!("{name} added")), ToastKind::Ok),
                    Err(e) => app.toast(e, ToastKind::Error),
                }
                app.refresh_access(true);
            },
        );
    }

    pub(super) fn access_remove(&mut self, list: List, name: &str) {
        let s = self.sel();
        if s.running() {
            let cmd = access::command(list, false, name, None);
            let id = s.id;
            self.send(Request::Command { id, cmd, user: false });
            return;
        }
        match access::remove(s.cfg.dir(), list, name) {
            Ok(()) => self.toast(tr!(format!("{name} убран из списка"), format!("{name} removed")), ToastKind::Ok),
            Err(e) => self.toast(format!("{e:#}"), ToastKind::Error),
        }
        self.refresh_access(true);
    }

    pub fn whitelist_on(&self) -> bool {
        self.sel().prop("white-list") == Some("true")
    }

    pub(super) fn toggle_whitelist(&mut self) {
        let on = !self.whitelist_on();
        let s = self.sel();
        if s.running() {
            let id = s.id;
            self.send(Request::Command { id, cmd: access::whitelist_switch_command(on).into(), user: false });
            // Сервер сам запишет white-list в server.properties; перечитаем через пару секунд.
            let tx = self.tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = tx.send(AppEvent::Ui(UiTask(Box::new(|app: &mut App| {
                    if app.has_server() {
                        app.sel_mut().reload_props()
                    }
                }))));
            });
        } else {
            self.set_property("white-list", if on { "true" } else { "false" });
        }
    }
}
