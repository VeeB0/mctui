//! Подвкладка «Датапаки»: список датапаков текущего мира, включение/выключение, перезагрузка и обновления.

use super::*;

/// Перезагрузка только датапаков. Bukkit-овский `reload` перезагружает плагины и опасен — его не шлём никогда.
const RELOAD_COMMAND: &str = "minecraft:reload";

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
    /// `/datapack`, включается той же командой на запущенном сервере.
    pub(super) fn toggle_datapack(&mut self) {
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
            let quoted = d.id().replace('\\', "\\\\").replace('"', "\\\"");
            self.send(Request::Command { id, cmd: format!("datapack enable \"{quoted}\""), user: false });
            return self.toast(
                tr!(format!("{}: включён в игре", d.name), format!("{}: enabled in game", d.name)),
                ToastKind::Ok,
            );
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

    /// Переключение после подтверждения: пак ищется по пути в только что перечитанном списке.
    pub(super) fn toggle_datapack_at(&mut self, path: PathBuf) {
        self.refresh_datapacks(true);
        match self.datapacks.items.iter().find(|d| d.path == path).cloned() {
            Some(d) => self.move_datapack(&d),
            None => self.toast(
                tr!("датапак не найден — список перечитан", "datapack not found — the list was rescanned"),
                ToastKind::Error,
            ),
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
            self.popup = Some(Popup::Confirm {
                title: tr!("Перезагрузить датапаки", "Reload datapacks").into(),
                text: format!("{note} {}", tr!("Перезагрузка даёт короткую заминку.", "A reload causes a brief lag.")),
                action: Action::ReloadDatapacks(id),
            });
            return self.toast(what, ToastKind::Ok);
        }
        self.send(Request::Command { id, cmd: RELOAD_COMMAND.into(), user: false });
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
        self.send(Request::Command { id, cmd: RELOAD_COMMAND.into(), user: false });
        self.toast(tr!("датапаки перезагружаются", "datapacks are reloading"), ToastKind::Ok);
    }

    /// Итог фоновой установки/обновления датапаков мира `key`: перечитать список и применить изменения
    /// (`changed` — новые или заменённые файлы). Если выбран уже другой сервер — только сообщение.
    pub(super) fn finish_datapack_job(&mut self, key: (usize, PathBuf), changed: &[PathBuf], what: String) {
        if !self.has_server() || self.datapacks_key() != key {
            return self.toast(what, ToastKind::Ok);
        }
        self.refresh_datapacks(true);
        let restart = self.datapacks.items.iter().any(|d| changed.contains(&d.path) && d.needs_restart);
        self.after_datapack_change(restart, what);
    }

    pub(super) fn check_datapack_updates(&mut self) {
        if self.datapacks.checking || self.datapacks.items.is_empty() {
            return;
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
                    Ok(h) => {
                        app.datapacks.checked = true;
                        let n = h.updates.len();
                        app.datapacks.updates = h.updates;
                        app.datapacks.versions = h.current;
                        if n == 0 {
                            app.toast(
                                tr!(
                                    "все датапаки актуальны (по данным Modrinth)",
                                    "all datapacks are up to date (per Modrinth)"
                                ),
                                ToastKind::Ok,
                            );
                        } else {
                            app.toast(
                                tr!(
                                    format!("доступно обновлений: {n} — Enter или U, чтобы установить"),
                                    format!("{n} update(s) available — Enter or U to install")
                                ),
                                ToastKind::Info,
                            );
                        }
                    }
                    Err(e) => app.toast(e, ToastKind::Error),
                }
            },
        );
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
