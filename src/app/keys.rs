//! Клавиатура: главный экран, вкладки, всплывающие окна.

use super::*;

impl App {
    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if let Some(p) = self.popup.take() {
            self.popup_key(p, k);
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.screen == Screen::Setup {
            if ctrl && matches!(k.code, KeyCode::Char(c) if matches!(latin(c), 'q' | 'c')) {
                return self.request_quit();
            }
            return self.setup_key(k);
        }
        if ctrl && let KeyCode::Char(c) = k.code {
            match latin(c) {
                'q' | 'c' => return self.request_quit(),
                's' => return self.start(),
                'x' => return self.stop(false),
                'r' => return self.stop(true),
                'b' => return self.backup(),
                'l' => return self.toggle_logs(),
                'u' => return self.self_update(),
                _ => {}
            }
        }
        if k.code == KeyCode::F(1) {
            return self.open_help();
        }
        if !self.has_server() {
            if matches!(k.code, KeyCode::Char(c) if latin(c) == 'q') || k.code == KeyCode::Esc {
                self.should_quit = true;
            }
            return;
        }
        match self.screen {
            Screen::Main => self.main_key(k),
            Screen::Logs => self.logs_key(k),
            Screen::Setup => {}
        }
    }

    pub(super) fn popup_key(&mut self, p: Popup, k: KeyEvent) {
        let yes = |c: char| matches!(latin(c), 'y' | 'Y') || c == 'д' || c == 'Д';
        match p {
            Popup::Confirm { title, text, action } => match k.code {
                KeyCode::Enter => self.run_action(action),
                KeyCode::Char(c) if yes(c) => self.run_action(action),
                KeyCode::Esc => {}
                KeyCode::Char(c) if matches!(latin(c), 'n' | 'N') => {}
                _ => self.popup = Some(Popup::Confirm { title, text, action }),
            },
            Popup::Quit { running } => match k.code {
                KeyCode::Enter => {
                    self.exit_message = Some(tr!(
                        format!(
                            "Серверы продолжают работать в фоне ({running}). Вернуться: pechka · состояние: pechka daemon status"
                        ),
                        format!(
                            "Servers keep running in the background ({running}). Come back: pechka · status: pechka daemon status"
                        )
                    ));
                    self.should_quit = true;
                }
                KeyCode::Char(c) if latin(c) == 's' => {
                    self.send(Request::StopAll);
                    self.exit_message = Some(
                        tr!(
                            "Серверы штатно останавливаются в фоне (pechka daemon status).",
                            "Servers are shutting down cleanly in the background (pechka daemon status)."
                        )
                        .into(),
                    );
                    // Дать запросу уйти по сети до выхода.
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = tx.send(AppEvent::Ui(UiTask(Box::new(|app: &mut App| app.should_quit = true))));
                    });
                }
                KeyCode::Esc => {}
                KeyCode::Char(c) if matches!(latin(c), 'n' | 'q') => {}
                _ => self.popup = Some(Popup::Quit { running }),
            },
            Popup::Panel(p) => self.panel_key(p, k),
            Popup::Help { scroll } => self.help_key(scroll, k),
            Popup::Edit { title, mut input, target } => match k.code {
                KeyCode::Enter => {
                    let v = input.value.clone();
                    match target {
                        EditTarget::Property(key) => self.set_property(&key, v.trim()),
                        EditTarget::Opt(o) => self.apply_opt(o, &v),
                        EditTarget::AccessAdd(list) => self.access_add(list, &v),
                        EditTarget::PregenRadius(dim) => self.run_pregen(dim, &v),
                        EditTarget::JvmMemory { xms_equal } => self.set_jvm_memory(v.trim(), xms_equal),
                        EditTarget::LogSearch => {
                            if !v.trim().is_empty() {
                                self.run_log_search(v.trim().to_string());
                            }
                        }
                        EditTarget::Chat => {
                            if !v.trim().is_empty() {
                                let id = self.sel().id;
                                self.send(Request::Command { id, cmd: format!("say {}", v.trim()), user: false });
                            }
                        }
                    }
                }
                KeyCode::Esc => {}
                _ => {
                    input.handle(&k);
                    self.popup = Some(Popup::Edit { title, input, target });
                }
            },
        }
    }

    pub(super) fn main_key(&mut self, k: KeyEvent) {
        if self.focus == Focus::Input {
            if k.code != KeyCode::Tab {
                self.completion = None;
                if !matches!(k.code, KeyCode::Char(_) | KeyCode::Backspace) {
                    self.completion_hint = None;
                }
            }
            match k.code {
                KeyCode::Enter => self.submit_input(),
                KeyCode::Esc => {
                    self.completion_hint = None;
                    self.focus = Focus::Sidebar
                }
                KeyCode::Tab => self.complete_input(),
                KeyCode::BackTab => self.focus = Focus::Content,
                KeyCode::Up => self.history_step(-1),
                KeyCode::Down => self.history_step(1),
                _ => {
                    self.input.handle(&k);
                }
            }
            return;
        }
        if self.focus == Focus::Content
            && matches!(self.tab, Tab::Access | Tab::Addons)
            && matches!(k.code, KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down)
            && if self.tab == Tab::Access { self.access_arrow(k.code) } else { self.addons_arrow(k.code) }
        {
            return;
        }
        match k.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Content,
                    _ => Focus::Input,
                }
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Content => Focus::Sidebar,
                    _ => Focus::Input,
                }
            }
            KeyCode::Esc => self.focus = Focus::Sidebar,
            KeyCode::Char(c) => {
                let c = latin(c);
                match c {
                    'l' | 'L' => return self.toggle_logs(),
                    // `,` — та же клавиша, что `?`, в русской раскладке.
                    '?' | ',' => return self.open_help(),
                    '1'..='7' => {
                        self.focus = Focus::Content;
                        return self.set_tab(TABS[c as usize - '1' as usize]);
                    }
                    ':' | '/' => {
                        self.focus = Focus::Input;
                        return;
                    }
                    'i' if self.focus == Focus::Sidebar => {
                        self.focus = Focus::Input;
                        return;
                    }
                    'q' => return self.request_quit(),
                    'f' => return self.open_network(),
                    'a' if self.focus == Focus::Sidebar => return self.open_setup(),
                    '-' | 'x' if self.focus == Focus::Sidebar => return self.ask_remove_server(),
                    _ => {}
                }
                if self.focus == Focus::Content {
                    self.content_char(c);
                }
            }
            KeyCode::Enter if self.focus == Focus::Sidebar => self.focus = Focus::Input,
            KeyCode::Delete if self.focus == Focus::Sidebar => self.ask_remove_server(),
            KeyCode::Delete if self.focus == Focus::Content => self.content_char('d'),
            KeyCode::Left if self.focus == Focus::Content => self.cycle_tab(-1),
            KeyCode::Right => {
                if self.focus == Focus::Content {
                    self.cycle_tab(1)
                } else {
                    self.focus = Focus::Content
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                let delta: isize = match k.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    KeyCode::PageUp => -10,
                    KeyCode::PageDown => 10,
                    KeyCode::Home => -100_000,
                    _ => 100_000,
                };
                self.move_selection(delta);
            }
            KeyCode::Enter if self.focus == Focus::Content => self.content_enter(),
            _ => {}
        }
    }

    pub(super) fn cycle_tab(&mut self, d: isize) {
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        let n = TABS.len() as isize;
        self.set_tab(TABS[((i + d).rem_euclid(n)) as usize]);
    }

    pub fn settings_len(&self) -> usize {
        self.opts().len() + self.sel().props.as_ref().map(|p| p.pairs().len()).unwrap_or(0)
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        let step = |cur: usize, len: usize| -> usize {
            if len == 0 {
                return 0;
            }
            (cur as isize + delta).clamp(0, len as isize - 1) as usize
        };
        match self.focus {
            Focus::Sidebar => {
                let n = step(self.selected, self.servers.len());
                if n != self.selected {
                    self.selected = n;
                    self.on_select_changed();
                }
            }
            Focus::Content => match self.tab {
                Tab::Players => self.players_sel = step(self.players_sel, self.sel().players.len()),
                Tab::Settings => self.settings_sel = step(self.settings_sel, self.settings_len()),
                Tab::Backups => self.backups_sel = step(self.backups_sel, self.backups.len()),
                Tab::Access => {
                    // PgUp/PgDn/Home/End и колесо мыши сразу ведут в записи.
                    if !self.access.entries.is_empty() {
                        self.access.level = SubLevel::Entries;
                    }
                    self.access.sel = step(self.access.sel, self.access.entries.len())
                }
                Tab::Addons => {
                    // PgUp/PgDn/Home/End и колесо мыши сразу ведут в записи.
                    if self.addons_len() > 0 {
                        self.addons.level = SubLevel::Entries;
                    }
                    match self.addons.sub {
                        AddonsSub::Plugins => self.plugins.sel = step(self.plugins.sel, self.plugins.items.len()),
                        AddonsSub::Datapacks => {
                            self.datapacks.sel = step(self.datapacks.sel, self.datapacks.items.len())
                        }
                    }
                }
                Tab::World => self.world.sel = step(self.world.sel, self.world.rows().len()),
                Tab::Status => {}
            },
            Focus::Input => {}
        }
    }

    pub(super) fn history_step(&mut self, d: isize) {
        if self.history.is_empty() {
            return;
        }
        let len = self.history.len();
        let pos = match (self.hist_pos, d) {
            (None, -1) => Some(len - 1),
            (None, _) => None,
            (Some(p), -1) => Some(p.saturating_sub(1)),
            (Some(p), _) if p + 1 < len => Some(p + 1),
            (Some(_), _) => None,
        };
        self.hist_pos = pos;
        match pos {
            Some(p) => self.input.set(&self.history[p].clone()),
            None => self.input.set(""),
        }
    }

    pub(super) fn selected_player(&self) -> Option<String> {
        self.sel().players.get(self.players_sel).cloned()
    }

    /// Пары server.properties в порядке показа: важные первыми (см. propdocs::sort_rank).
    pub fn prop_rows(&self) -> Vec<(String, String)> {
        let Some(p) = self.sel().props.as_ref() else { return vec![] };
        let mut v = p.pairs();
        v.sort_by(|a, b| (crate::propdocs::sort_rank(&a.0), &a.0).cmp(&(crate::propdocs::sort_rank(&b.0), &b.0)));
        v
    }

    /// Выбранная строка вкладки «Настройки»: опция pechka или пара server.properties.
    pub fn selected_setting_pub(&self) -> Option<Result<Opt, (String, String)>> {
        self.selected_setting()
    }

    /// Выбранная строка вкладки «Настройки»: опция pechka или пара server.properties.
    pub(super) fn selected_setting(&self) -> Option<Result<Opt, (String, String)>> {
        let opts = self.opts();
        if self.settings_sel < opts.len() {
            return Some(Ok(opts[self.settings_sel]));
        }
        let i = self.settings_sel - opts.len();
        self.prop_rows().into_iter().nth(i).map(Err)
    }

    pub(super) fn content_char(&mut self, c: char) {
        let id = self.sel().id;
        match self.tab {
            Tab::Players => {
                if c == 's' {
                    return self.open_player_stats();
                }
                let Some(p) = self.selected_player() else {
                    return;
                };
                let (cmd, what) = match c {
                    'k' => (format!("kick {p}"), tr!("Кикнуть", "Kick")),
                    'b' => (format!("ban {p}"), tr!("Забанить", "Ban")),
                    'o' => (format!("op {p}"), tr!("Выдать оператора", "Make operator")),
                    'd' => (format!("deop {p}"), tr!("Снять оператора", "Remove operator")),
                    'w' => (format!("whitelist add {p}"), tr!("В белый список", "Add to whitelist")),
                    _ => return,
                };
                self.popup = Some(Popup::Confirm {
                    title: what.into(),
                    text: tr!(format!("{what} {p}? Будет выполнено: /{cmd}"), format!("{what} {p}? This runs: /{cmd}")),
                    action: Action::Command { id, cmd },
                });
            }
            Tab::Settings => match c {
                ' ' => match self.selected_setting() {
                    Some(Ok(o)) => self.toggle_opt(o),
                    Some(Err((k, v))) => match crate::propdocs::next_choice(&k, &v) {
                        Some(next) => self.set_property(&k, &next),
                        None => self.toast(
                            tr!(
                                "пробел переключает да/нет и варианты из списка; Enter — редактировать",
                                "Space cycles yes/no and listed choices; Enter edits"
                            ),
                            ToastKind::Info,
                        ),
                    },
                    None => {}
                },
                'r' => {
                    if self.sel().is_local() {
                        self.popup = Some(Popup::Confirm {
                            title: "RCON".into(),
                            text: tr!(
                                "Включить RCON на 127.0.0.1 (enable-rcon=true, случайный пароль)? Нужен для TPS и точного списка игроков.",
                                "Enable RCON on 127.0.0.1 (enable-rcon=true, random password)? It is needed for TPS and the exact player list."
                            )
                            .into(),
                            action: Action::EnableRcon,
                        });
                    }
                }
                'u' => {
                    self.sel_mut().reload_props();
                    self.toast(tr!("перечитано с диска", "reloaded from disk"), ToastKind::Info);
                }
                _ => {}
            },
            Tab::Backups => match c {
                'n' => self.backup(),
                'd' => {
                    if let Some(b) = self.backups.get(self.backups_sel) {
                        let name = b.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        let warn = if self.backups.len() == 1 {
                            tr!(
                                "\n⚠ Это единственный бэкап сервера — после удаления откатиться будет не на что.",
                                "\n⚠ This is the server's only backup — nothing will be left to roll back to."
                            )
                        } else if self.backups_sel == 0 {
                            tr!("\nЭто самый свежий бэкап.", "\nThis is the most recent backup.")
                        } else {
                            ""
                        };
                        self.popup = Some(Popup::Confirm {
                            title: tr!("Удалить бэкап", "Delete backup").into(),
                            text: tr!(
                                format!("Удалить {name} безвозвратно?{warn}"),
                                format!("Delete {name} permanently?{warn}")
                            ),
                            action: Action::DeleteBackup(b.path.clone()),
                        });
                    }
                }
                'r' => {
                    if let Some(b) = self.backups.get(self.backups_sel) {
                        let name = b.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        let s = self.sel();
                        let paths = s.cfg.backup_paths.join(", ");
                        let running = if s.running() {
                            let online = players_note(&s.players).map(|n| format!(" {n}")).unwrap_or_default();
                            tr!(
                                format!(" Сервер будет остановлен и запущен снова.{online}"),
                                format!(" The server will be stopped and started again.{online}")
                            )
                        } else {
                            String::new()
                        };
                        self.popup = Some(Popup::Confirm {
                            title: tr!("Восстановить из бэкапа", "Restore from backup").into(),
                            text: tr!(
                                format!(
                                    "Восстановить {paths} из {name}?{running} Текущие файлы переедут в .pechka-before-restore/."
                                ),
                                format!(
                                    "Restore {paths} from {name}?{running} Current files move to .pechka-before-restore/."
                                )
                            ),
                            action: Action::Restore { id, path: b.path.clone() },
                        });
                    }
                }
                'u' => self.refresh_backups(),
                _ => {}
            },
            Tab::Access => {
                // [ ] (х ъ в русской раскладке) листают списки с любого уровня, без спуска стрелкой ↓.
                match c {
                    '[' => return self.cycle_access_list(-1),
                    ']' => return self.cycle_access_list(1),
                    _ => {}
                }
                if !self.sel().is_local() {
                    return;
                }
                let list = self.access.list;
                match c {
                    'a' | 'n' => {
                        let title = if list.is_ip() {
                            tr!(format!("{} — IP-адрес", list.title()), format!("{} — IP address", list.title()))
                        } else {
                            tr!(format!("{} — ник игрока", list.title()), format!("{} — player name", list.title()))
                        };
                        self.popup = Some(Popup::Edit {
                            title,
                            input: TextInput::default(),
                            target: EditTarget::AccessAdd(list),
                        });
                    }
                    // Выделение записи видно только на уровне записей — не удаляем то, чего не видно.
                    'd' if self.access.level != SubLevel::Entries && !self.access.entries.is_empty() => {
                        self.access.level = SubLevel::Entries;
                        self.toast_pick_entry("d");
                    }
                    'd' => {
                        if let Some(e) = self.access.entries.get(self.access.sel) {
                            let name = e.name.clone();
                            self.popup = Some(Popup::Confirm {
                                title: list.title().into(),
                                text: tr!(
                                    format!("Убрать {name} из списка «{}»?", list.title()),
                                    format!("Remove {name} from “{}”?", list.title())
                                ),
                                action: Action::AccessRemove { list, name },
                            });
                        }
                    }
                    'w' => self.toggle_whitelist(),
                    'u' => {
                        self.sel_mut().reload_props();
                        self.refresh_access(true)
                    }
                    _ => {}
                }
            }
            Tab::Addons => {
                // [ ] (х ъ в русской раскладке) листают подвкладки с любого уровня.
                match c {
                    '[' => return self.cycle_addons_sub(-1),
                    ']' => return self.cycle_addons_sub(1),
                    _ => {}
                }
                if !self.sel().is_local() {
                    return;
                }
                match self.addons.sub {
                    // Датапаки: пока только перечитать (действия — позже).
                    AddonsSub::Datapacks => {
                        if c == 'u' {
                            self.refresh_datapacks(true)
                        }
                    }
                    AddonsSub::Plugins => match c {
                        'i' | 'n' => self.open_plugin_search(),
                        // Выделение записи видно только на уровне записей — не трогаем то, чего не видно.
                        ' ' if self.addons.level != SubLevel::Entries && !self.plugins.items.is_empty() => {
                            self.addons.level = SubLevel::Entries;
                            self.toast_pick_entry(tr!("Пробел", "Space"));
                        }
                        ' ' => self.toggle_plugin(),
                        'c' => self.check_plugin_updates(),
                        'U' => self.ask_apply_updates(true),
                        'u' => self.refresh_plugins(true),
                        _ => {}
                    },
                }
            }
            Tab::World => self.world_char(c),
            Tab::Status => match c {
                'U' => self.open_core_update(),
                'v' => self.open_velocity_link(),
                'T' => self.open_clone(),
                'J' => self.open_jvm(),
                _ => {}
            },
        }
    }

    pub(super) fn content_enter(&mut self) {
        match self.tab {
            Tab::Settings => match self.selected_setting() {
                Some(Ok(o)) => self.edit_opt(o),
                Some(Err((k, v))) => {
                    self.popup = Some(Popup::Edit {
                        title: k.clone(),
                        input: TextInput::with(&v),
                        target: EditTarget::Property(k),
                    })
                }
                None => {}
            },
            Tab::Addons => {
                if !self.sel().is_local() || self.addons.sub != AddonsSub::Plugins {
                    return;
                }
                if self.addons.level != SubLevel::Entries && !self.plugins.items.is_empty() {
                    self.addons.level = SubLevel::Entries;
                    return self.toast_pick_entry("Enter");
                }
                self.ask_apply_updates(false)
            }
            Tab::Access => self.content_char('a'),
            Tab::World => self.world_enter(),
            _ => {}
        }
    }
}
