//! События от службы (снимки состояния, логи, графики) и таймер окна.

use super::*;

impl App {
    pub fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Daemon(m) => self.on_daemon(*m),
            AppEvent::DaemonLost(e) => {
                if self.connected {
                    self.toast(e.clone(), ToastKind::Error);
                }
                self.connected = false;
                self.daemon_error = Some(e);
            }
            AppEvent::UpdateAvailable(v) => {
                self.toast(
                    tr!(
                        format!("Доступна версия {v} — Ctrl+U, чтобы обновить"),
                        format!("Version {v} is available — press Ctrl+U to update")
                    ),
                    ToastKind::Info,
                );
                self.update_available = Some(v);
            }
            AppEvent::Ui(UiTask(f)) => f(self),
            // Процессы и RCON живут в службе — окну эти события не приходят.
            _ => {}
        }
    }

    pub(super) fn on_daemon(&mut self, m: Msg) {
        match m {
            Msg::Welcome { version, can_detach, .. } => {
                self.connected = true;
                self.daemon_can_detach = can_detach;
                self.daemon_error = None;
                self.version_check_pending = version != crate::update::VERSION;
                self.daemon_outdated = false;
                self.daemon_version = Some(version);
            }
            Msg::Denied { reason } => self.daemon_error = Some(reason),
            Msg::State { servers } => self.on_state(servers),
            Msg::Log { id, lines, reset } => {
                let Some(i) = self.servers.iter().position(|s| s.id == id) else { return };
                if reset {
                    self.servers[i].log = LogBuffer::new(10_000);
                }
                let following = !reset
                    && self.screen == Screen::Logs
                    && i == self.selected
                    && matches!(self.logs.active_source(), Some(LogSource::Live | LogSource::Chat));
                let chat = matches!(self.logs.active_source(), Some(LogSource::Chat));
                for l in &lines {
                    let line = self.servers[i].log.push(l);
                    let shown = if chat {
                        crate::server::stats::is_chat(&line.text)
                    } else {
                        self.logs.filter.pass(line.level)
                    };
                    if following && self.logs.scroll > 0 && shown {
                        self.logs.scroll += 1; // держим вид на месте, пока пользователь читает старое
                    }
                }
            }
            Msg::History { id, samples } => {
                if let Some(s) = self.servers.iter_mut().find(|s| s.id == id) {
                    s.history = samples.into_iter().collect();
                }
            }
            Msg::Sample { id, sample } => {
                if let Some(s) = self.servers.iter_mut().find(|s| s.id == id) {
                    s.history.push_back(sample);
                    while s.history.len() > HISTORY_CAP {
                        s.history.pop_front();
                    }
                }
            }
            Msg::Notice { text, kind } => {
                let k = match kind {
                    NoticeKind::Info => ToastKind::Info,
                    NoticeKind::Ok => ToastKind::Ok,
                    NoticeKind::Error => ToastKind::Error,
                };
                if let Some(setup) = &mut self.setup
                    && k == ToastKind::Error
                    && self.screen == Screen::Setup
                {
                    setup.error = Some(text.clone());
                }
                self.toast(text, k);
                if self.tab == Tab::Backups {
                    self.refresh_backups();
                }
            }
        }
    }

    pub(super) fn on_state(&mut self, views: Vec<ServerView>) {
        let sel_id = self.sel_id();
        let mut old: HashMap<usize, ServerUi> = self.servers.drain(..).map(|s| (s.id, s)).collect();
        for v in views {
            let s = match old.remove(&v.id) {
                Some(mut s) => {
                    let props_changed = s.view.cfg.dir != v.cfg.dir || s.view.needs_restart != v.needs_restart;
                    s.view = v;
                    if props_changed {
                        s.reload_props();
                    }
                    s
                }
                None => ServerUi::new(v),
            };
            self.servers.push(s);
        }
        let first = !self.got_state;
        self.got_state = true;
        // Выбор: только что добавленный сервер, иначе прежний.
        if let Some(after) = self.select_new_after
            && let Some(i) = self.servers.iter().position(|s| s.id > after)
        {
            self.select_new_after = None;
            self.selected = i;
            self.on_select_changed();
        } else if let Some(i) = sel_id.and_then(|id| self.servers.iter().position(|s| s.id == id)) {
            self.selected = i;
        } else {
            self.selected = self.selected.min(self.servers.len().saturating_sub(1));
            if self.has_server() {
                self.on_select_changed();
            }
        }
        if first && self.has_server() {
            self.on_select_changed();
        }
        if self.servers.is_empty() && self.screen != Screen::Setup {
            self.open_setup();
        }
        if self.version_check_pending {
            self.version_check_pending = false;
            if self.daemon_can_detach {
                // Служба другой версии уходит, серверы остаются у хранителей; окно поднимет новую.
                self.send(Request::Detach);
                self.toast(
                    tr!(
                        "служба обновляется до новой версии, серверы продолжают работать…",
                        "updating the service to the new version; servers keep running…"
                    ),
                    ToastKind::Info,
                );
            } else if self.servers.iter().any(|s| s.running()) {
                self.daemon_outdated = true;
            } else {
                // Служба старой версии и ничего не запущено — перезапускаем её на текущей.
                self.send(Request::Shutdown);
                self.toast(
                    tr!("служба обновляется до новой версии…", "updating the service to the new version…"),
                    ToastKind::Info,
                );
            }
        }
    }

    /// Раз в секунду: всплывашки, свежесть списков на открытой вкладке.
    pub fn on_tick(&mut self) {
        let now = Instant::now();
        if self.toast.as_ref().is_some_and(|t| now.duration_since(t.2) > TOAST_FOR) {
            self.toast = None;
        }
        if self.screen != Screen::Main || !self.has_server() {
            return;
        }
        match self.tab {
            Tab::Access => self.refresh_access(false),
            Tab::Backups if self.backups_at.is_none_or(|t| now.duration_since(t) > Duration::from_secs(3)) => {
                self.refresh_backups()
            }
            _ => {}
        }
    }
}
