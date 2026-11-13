//! Подвкладка «Датапаки»: список датапаков текущего мира, включение/выключение, перезагрузка и обновления.

use super::*;

/// Перезагрузка только датапаков. Bukkit-овский `reload` перезагружает плагины и опасен — его не шлём никогда.
const RELOAD_COMMAND: &str = "minecraft:reload";

/// Последняя фраза вопросов о перезагрузке датапаков (`minecraft:reload` и `datapack enable`).
fn reload_lag() -> &'static str {
    tr!("Перезагрузка даёт короткую заминку.", "A reload causes a brief lag.")
}

impl App {
    /// (id сервера, папка мира) выбранного сервера.
    fn datapacks_key(&self) -> (usize, PathBuf) {
        (self.sel().id, datapacks::level_dir(self.sel().cfg.dir()))
    }

    pub(super) fn refresh_datapacks(&mut self, force: bool) {
        if !self.has_server() || !self.sel().is_local() {
            self.datapacks.items.clear();
            self.datapacks.loaded_for = None;
            return;
        }
        let key = self.datapacks_key();
        if !force && self.datapacks.loaded_for.as_ref() == Some(&key) {
            return;
        }
        if self.datapacks.loaded_for.as_ref() != Some(&key) {
            self.datapacks.updates.clear();
            self.datapacks.versions.clear();
            self.datapacks.checked = false;
        }
        self.datapacks.items = datapacks::scan(&key.1);
        self.datapacks.loaded_for = Some(key);
        self.datapacks.sel = self.datapacks.sel.min(self.datapacks.items.len().saturating_sub(1));
        if self.addons.sub == AddonsSub::Datapacks
            && self.addons.level == SubLevel::Entries
            && self.datapacks.items.is_empty()
        {
            self.addons.level = SubLevel::Lists;
        }
    }

    /// Пробел: включить/выключить выбранный датапак переносом между `datapacks/` и `datapacks.disabled/`.
    /// Пак генерации мира выключается только после подтверждения; пак, выключенный в игре командой
    /// `/datapack`, включается той же командой на запущенном сервере (при игроках онлайн — после вопроса).
    pub(super) fn toggle_datapack(&mut self) {
        // Пока идёт установка или обновление, файлы датапаков не трогаем.
        if self.datapacks_busy() {
            return;
        }
        let Some(d) = self.datapacks.items.get(self.datapacks.sel).cloned() else { return };
        if d.disabled_in_game {
            let s = self.sel();
            if !s.running() {
                return self.toast(
                    tr!(
                        "Он выключен в игре командой /datapack — включить можно на запущенном сервере",
                        "It was disabled in game with /datapack — it can be enabled on a running server"
                    ),
                    ToastKind::Info,
                );
            }
            let id = s.id;
            // `datapack enable` перезагружает датапаки целиком, как `minecraft:reload`, — те же правила.
            // Сюда попадаем только с клавиши, когда никакого окна не открыто: вопрос ничего не перекрывает
            // (а если бы окно было — не подменяем его и без ответа ничего не отправляем).
            if let Some(note) = players_note(&s.players) {
                if self.popup.is_none() {
                    let lag = reload_lag();
                    self.popup = Some(Popup::Confirm {
                        title: tr!("Включить датапак", "Enable datapack").into(),
                        text: tr!(
                            format!("{}: datapack enable перезагрузит датапаки. {note} {lag}", d.name),
                            format!("{}: datapack enable reloads datapacks. {note} {lag}", d.name)
                        ),
                        action: Action::EnableDatapackInGame(id, Box::new(d)),
                    });
                }
                return;
            }
            return self.enable_datapack_in_game(id, &d);
        }
        if d.enabled && d.worldgen {
            self.popup = Some(Popup::Confirm {
                title: tr!("Выключить датапак", "Disable datapack").into(),
                text: tr!(
                    format!("{} меняет генерацию мира: уже созданные места могут сломаться. Выключить?", d.name),
                    format!("{} changes world generation: places that already exist may break. Disable it?", d.name)
                ),
                action: Action::DatapackToggle(d.path),
            });
            return;
        }
        self.move_datapack(&d);
    }

    /// `datapack enable "file/<имя>"` для пака, выключенного в игре. Отметка «выкл в игре» снимается только
    /// в памяти (`level.dat` не пишем): новый скан перечитает `level.dat`, а игра сохранит его не сразу.
    pub(super) fn enable_datapack_in_game(&mut self, id: usize, d: &DatapackInfo) {
        let quoted = d.id().replace('\\', "\\\\").replace('"', "\\\"");
        self.send_announced(id, format!("datapack enable \"{quoted}\""));
        if let Some(item) = self.datapacks.items.iter_mut().find(|x| x.path == d.path) {
            item.disabled_in_game = false;
        }
        let mut what = tr!(format!("{}: включён в игре", d.name), format!("{}: enabled in game", d.name));
        if d.needs_restart {
            what += tr!(" — вступит в силу после перезапуска", " — takes effect after a restart");
        }
        self.toast(what, ToastKind::Ok);
    }

    /// Переключение после подтверждения: пак ищется по пути в текущем списке (тот, о котором спрашивали);
    /// после переноса список перечитывается один раз.
    pub(super) fn toggle_datapack_at(&mut self, path: PathBuf) {
        match self.datapacks.items.iter().find(|d| d.path == path).cloned() {
            Some(d) => self.move_datapack(&d),
            None => {
                self.refresh_datapacks(true);
                self.toast(
                    tr!("датапак не найден — список перечитан", "datapack not found — the list was rescanned"),
                    ToastKind::Error,
                )
            }
        }
    }

    fn move_datapack(&mut self, d: &DatapackInfo) {
        let res = datapacks::set_enabled(d, !d.enabled, &self.datapacks_key().1);
        self.refresh_datapacks(true);
        match res {
            Ok(_) => {
                let what = if d.enabled {
                    tr!(format!("{}: выключен", d.name), format!("{}: disabled", d.name))
                } else {
                    tr!(format!("{}: включён", d.name), format!("{}: enabled", d.name))
                };
                self.after_datapack_change(d.needs_restart || d.worldgen, what);
            }
            Err(e) => self.toast(format!("{e:#}"), ToastKind::Error),
        }
    }

    /// Датапаки изменились (`what` — что именно). На запущенном сервере — `minecraft:reload`, при игроках
    /// онлайн — после вопроса; паки, которым нужен перезапуск, на ходу не перезагружаются.
    pub(super) fn after_datapack_change(&mut self, needs_restart: bool, what: String) {
        let s = self.sel();
        if !s.running() {
            return self.toast(what, ToastKind::Ok);
        }
        if needs_restart {
            let tail = tr!(" — вступит в силу после перезапуска", " — takes effect after a restart");
            return self.toast(format!("{what}{tail}"), ToastKind::Ok);
        }
        let id = s.id;
        if let Some(note) = players_note(&s.players) {
            // Фоновая загрузка могла закончиться, пока открыто другое окно (поиск, ввод, вопрос об
            // остановке), — не подменяем его: Enter, предназначенный ему, не должен перезагрузить датапаки.
            if self.popup.is_some() {
                let tail = tr!(
                    " · датапаки не перезагружены: чтобы применить, введите minecraft:reload",
                    " · datapacks were not reloaded: type minecraft:reload to apply"
                );
                return self.toast(format!("{what}{tail}"), ToastKind::Info);
            }
            self.popup = Some(Popup::Confirm {
                title: tr!("Перезагрузить датапаки", "Reload datapacks").into(),
                text: format!("{note} {}", reload_lag()),
                action: Action::ReloadDatapacks(id),
            });
            // Видно и после отказа (Esc): изменение ещё не применено.
            let tail = tr!(
                " — применится после minecraft:reload или перезапуска",
                " — applies after minecraft:reload or a restart"
            );
            return self.toast(format!("{what}{tail}"), ToastKind::Ok);
        }
        self.send_announced(id, RELOAD_COMMAND.into());
        let tail = tr!(" · датапаки перезагружаются", " · datapacks are reloading");
        self.toast(format!("{what}{tail}"), ToastKind::Ok);
    }

    /// Идёт установка или обновление датапаков. Второе такое же задание не запускаем: временные файлы
    /// загрузок общие, параллельная загрузка могла бы испортить пак.
    pub(super) fn datapacks_busy(&mut self) -> bool {
        if self.datapacks.busy.is_none() {
            return false;
        }
        self.toast(
            tr!("подождите: датапаки ещё скачиваются", "please wait: datapacks are still downloading"),
            ToastKind::Info,
        );
        true
    }

    pub(super) fn reload_datapacks(&mut self, id: usize) {
        self.send_announced(id, RELOAD_COMMAND.into());
        self.toast(tr!("датапаки перезагружаются", "datapacks are reloading"), ToastKind::Ok);
    }

    /// Итог фоновой установки/обновления датапаков мира `key`: перечитать список и применить изменения
    /// (`changed` — новые или заменённые файлы). Если выбран уже другой сервер — только сообщение с его именем:
    /// датапаки там не перезагружаются.
    pub(super) fn finish_datapack_job(&mut self, key: (usize, PathBuf), changed: &[PathBuf], what: String) {
        if !self.has_server() || self.datapacks_key() != key {
            let msg = match self.servers.iter().find(|s| s.id == key.0) {
                Some(s) if s.running() => tr!(
                    format!("{what} на «{}» — не перезагружено", s.cfg.name),
                    format!("{what} on \"{}\" — not reloaded", s.cfg.name)
                ),
                Some(s) => tr!(format!("{what} на «{}»", s.cfg.name), format!("{what} on \"{}\"", s.cfg.name)),
                None => what,
            };
            return self.toast(msg, ToastKind::Ok);
        }
        self.refresh_datapacks(true);
        let restart = self.datapacks.items.iter().any(|d| changed.contains(&d.path) && d.needs_restart);
        self.after_datapack_change(restart, what);
    }

    pub(super) fn check_datapack_updates(&mut self) {
        if self.datapacks.checking {
            return;
        }
        if self.datapacks.items.is_empty() {
            return self.toast(tr!("датапаков нет", "no datapacks"), ToastKind::Info);
        }
        let items = self.datapacks.items.clone();
        let game = self.sel().version.clone();
        let key = self.datapacks.loaded_for.clone();
        self.datapacks.checking = true;
        self.spawn_task(
            move || datapacks::check_updates(&items, game.as_deref()).map_err(|e| format!("{e:#}")),
            move |app, res| {
                app.datapacks.checking = false;
                if app.datapacks.loaded_for != key {
                    return;
                }
                match res {
                    Ok(h) => app.show_datapack_check(h),
                    Err(e) => app.toast(e, ToastKind::Error),
                }
            },
        );
    }

    /// Итог проверки обновлений датапаков текущего мира.
    pub(super) fn show_datapack_check(&mut self, h: crate::plugins::HashCheck) {
        self.datapacks.checked = true;
        let n = h.updates.len();
        self.datapacks.updates = h.updates;
        self.datapacks.versions = h.current;
        if n == 0 && self.datapacks.versions.is_empty() {
            // Ни один файл Modrinth не узнал — «всё актуально» было бы неправдой.
            self.toast(
                tr!("Modrinth не узнал ни одного датапака", "Modrinth recognised none of the datapacks"),
                ToastKind::Info,
            );
        } else if n == 0 {
            self.toast(
                tr!("все датапаки актуальны (по данным Modrinth)", "all datapacks are up to date (per Modrinth)"),
                ToastKind::Ok,
            );
        } else {
            self.toast(
                tr!(
                    format!("доступно обновлений: {n} — Enter или U, чтобы установить"),
                    format!("{n} update(s) available — Enter or U to install")
                ),
                ToastKind::Info,
            );
        }
    }

    pub(super) fn ask_apply_datapack_updates(&mut self, all: bool) {
        let d = &self.datapacks;
        let paths: Vec<PathBuf> = if all {
            d.items.iter().filter(|x| d.updates.contains_key(&x.path)).map(|x| x.path.clone()).collect()
        } else {
            match d.items.get(d.sel) {
                Some(x) if d.updates.contains_key(&x.path) => vec![x.path.clone()],
                Some(_) => {
                    return self.toast(
                        if d.checked {
                            tr!("для этого датапака обновлений нет", "no update for this datapack")
                        } else {
                            tr!("сначала проверьте обновления: c", "check for updates first: c")
                        },
                        ToastKind::Info,
                    );
                }
                None => return,
            }
        };
        if paths.is_empty() {
            return self.toast(tr!("обновлять нечего", "nothing to update"), ToastKind::Info);
        }
        let chosen: Vec<&DatapackInfo> = d.items.iter().filter(|x| paths.contains(&x.path)).collect();
        let names: Vec<String> =
            chosen.iter().map(|x| format!("{} → {}", x.name, d.updates[&x.path].version)).collect();
        let old = format!("{}/{}/", datapacks::DISABLED_DIR, plugins::OLD_DIR);
        let mut text = tr!(
            format!("{}. Старые файлы уйдут в {old}.", names.join(", ")),
            format!("{}. Old files move to {old}.", names.join(", "))
        );
        let worldgen: Vec<&str> = chosen.iter().filter(|x| x.worldgen).map(|x| x.name.as_str()).collect();
        if !worldgen.is_empty() {
            let list = worldgen.join(", ");
            text += &if worldgen.len() == 1 {
                tr!(
                    format!("\n⚠ {list} меняет генерацию мира: изменения коснутся только новых чанков."),
                    format!("\n⚠ {list} changes world generation: only new chunks are affected.")
                )
            } else {
                tr!(
                    format!("\n⚠ {list} меняют генерацию мира: изменения коснутся только новых чанков."),
                    format!("\n⚠ {list} change world generation: only new chunks are affected.")
                )
            };
        }
        self.popup = Some(Popup::Confirm {
            title: tr!("Обновить датапаки", "Update datapacks").into(),
            text,
            action: Action::ApplyDatapackUpdates(paths),
        });
    }

    /// Скачать и поставить обновления под теми же именами файлов; затем — как после любого изменения
    /// датапаков ([`Self::after_datapack_change`]).
    pub(super) fn apply_datapack_updates(&mut self, paths: Vec<PathBuf>) {
        if self.datapacks_busy() {
            return;
        }
        let jobs: Vec<(DatapackInfo, UpdateInfo)> = self
            .datapacks
            .items
            .iter()
            .filter(|d| paths.contains(&d.path))
            .filter_map(|d| Some((d.clone(), self.datapacks.updates.get(&d.path)?.clone())))
            .collect();
        if jobs.is_empty() {
            return self.toast(tr!("обновлять нечего", "nothing to update"), ToastKind::Info);
        }
        let key = self.datapacks_key();
        let level = key.1.clone();
        self.datapacks.busy = Some(tr!("скачивание обновлений…", "downloading updates…").into());
        self.spawn_task(
            move || {
                let mut done = vec![];
                let mut errs = vec![];
                for (d, u) in &jobs {
                    match datapacks::apply_update(d, u, &level) {
                        Ok(p) => done.push((p, u.version.clone())),
                        Err(e) => errs.push(format!("{}: {e:#}", d.name)),
                    }
                }
                (done, errs)
            },
            move |app, (done, errs)| {
                app.datapacks.busy = None;
                if done.is_empty() {
                    return app.toast(errs.join("; "), ToastKind::Error);
                }
                if app.datapacks.loaded_for.as_ref() == Some(&key) {
                    for (p, v) in &done {
                        app.datapacks.updates.remove(p);
                        app.datapacks.versions.insert(p.clone(), v.clone());
                    }
                    app.datapacks.checked = false;
                }
                let n = done.len();
                let mut what = tr!(format!("обновлено: {n}"), format!("updated: {n}"));
                if !errs.is_empty() {
                    what = format!("{what}; {}", errs.join("; "));
                }
                let changed: Vec<PathBuf> = done.into_iter().map(|(p, _)| p).collect();
                app.finish_datapack_job(key, &changed, what);
            },
        );
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::i18n::{Lang, with_lang};

    #[tokio::test]
    async fn finish_datapack_job_keeps_an_open_window() {
        let dir = PathBuf::from("/nonexistent/pechka-finish-datapack");
        let cfg = crate::config::ServerConfig::new_local("S", dir.join("s"), vec![], vec![]);
        let mut v = crate::server::ServerState::new(cfg, 0).view();
        v.status = Status::Running;
        v.pid = Some(1);
        v.players = vec!["Alex".into()];
        let (client, mut rx) = Client::recording();
        with_lang(Lang::En, || {
            let mut a = App::for_test(dir.join("servers.toml"), client, vec![v]);
            // Пока шла загрузка, пользователь открыл вопрос об остановке (Ctrl+X).
            a.popup = Some(Popup::Confirm {
                title: "Stop".into(),
                text: "Stop?".into(),
                action: Action::Stop { id: 0, restart: false },
            });
            a.finish_datapack_job(a.datapacks_key(), &[], "installed: 1".into());
            assert!(matches!(a.popup, Some(Popup::Confirm { action: Action::Stop { id: 0, restart: false }, .. })));
            let toast = a.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
            assert!(toast.contains("installed: 1") && toast.contains("minecraft:reload"), "{toast}");
            // Окна нет — обычный вопрос о перезагрузке.
            a.popup = None;
            a.finish_datapack_job(a.datapacks_key(), &[], "installed: 1".into());
            assert!(matches!(a.popup, Some(Popup::Confirm { action: Action::ReloadDatapacks(0), .. })));
        });
        assert!(rx.try_recv().is_err(), "без ответа пользователя ничего не отправляется");
    }

    fn running(name: &str, id: usize, dir: &std::path::Path) -> crate::proto::ServerView {
        let cfg = crate::config::ServerConfig::new_local(name, dir.to_path_buf(), vec![], vec![]);
        let mut v = crate::server::ServerState::new(cfg, id).view();
        v.status = Status::Running;
        v.pid = Some(1);
        v
    }

    #[tokio::test]
    async fn finish_datapack_job_on_another_server_names_it() {
        let dir = PathBuf::from("/nonexistent/pechka-finish-other");
        let (client, mut rx) = Client::recording();
        with_lang(Lang::En, || {
            let servers = vec![running("S", 0, &dir.join("s")), running("Other", 1, &dir.join("o"))];
            let mut a = App::for_test(dir.join("servers.toml"), client, servers);
            assert_eq!(a.sel().id, 0);
            let key = (1, datapacks::level_dir(&dir.join("o")));
            a.finish_datapack_job(key, &[], "installed: 1".into());
            assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(r#"installed: 1 on "Other" — not reloaded"#));
            assert!(a.popup.is_none());
            // Остановленному серверу перезагружать нечего: паки загрузятся при запуске.
            a.servers[1].view.pid = None;
            a.servers[1].view.status = Status::Offline;
            a.finish_datapack_job((1, datapacks::level_dir(&dir.join("o"))), &[], "installed: 1".into());
            assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(r#"installed: 1 on "Other""#));
        });
        assert!(rx.try_recv().is_err(), "на чужом сервере ничего не перезагружается");
    }

    #[tokio::test]
    async fn datapack_check_tells_when_modrinth_knows_none() {
        let dir = PathBuf::from("/nonexistent/pechka-check-none");
        with_lang(Lang::En, || {
            let mut a = App::for_test(dir.join("servers.toml"), Client::detached(), vec![running("S", 0, &dir)]);
            let toast = |a: &App| a.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
            a.show_datapack_check(crate::plugins::HashCheck::default());
            assert_eq!(toast(&a), "Modrinth recognised none of the datapacks");
            let mut h = crate::plugins::HashCheck::default();
            h.current.insert(dir.join("a.zip"), "1.0".into());
            a.show_datapack_check(h);
            assert_eq!(toast(&a), "all datapacks are up to date (per Modrinth)");
        });
    }
}
