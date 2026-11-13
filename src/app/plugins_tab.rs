//! Вкладка «Плагины»: список, включение/выключение и обновления через Modrinth.

use super::*;

impl App {
    pub(super) fn refresh_plugins(&mut self, force: bool) {
        if !self.has_server() || !self.sel().is_local() {
            self.plugins.items.clear();
            self.plugins.loaded_for = None;
            return;
        }
        let id = self.sel().id;
        if !force && self.plugins.loaded_for == Some(id) {
            return;
        }
        if self.plugins.loaded_for != Some(id) {
            self.plugins.updates.clear();
            self.plugins.checked = false;
        }
        self.plugins.loaded_for = Some(id);
        self.plugins.items = plugins::scan(self.sel().cfg.dir());
        self.plugins.sel = self.plugins.sel.min(self.plugins.items.len().saturating_sub(1));
    }

    pub(super) fn toggle_plugin(&mut self) {
        let Some(p) = self.plugins.items.get(self.plugins.sel).cloned() else { return };
        match plugins::set_enabled(&p, !p.enabled) {
            Ok(_) => {
                let what =
                    if p.enabled { tr!("выключен", "disabled") } else { tr!("включён", "enabled") };
                let tail = if self.sel().running() {
                    tr!(" — вступит в силу после перезапуска", " — takes effect after a restart")
                } else {
                    ""
                };
                self.toast(format!("{}: {what}{tail}", p.name), ToastKind::Ok);
            }
            Err(e) => self.toast(format!("{e:#}"), ToastKind::Error),
        }
        self.refresh_plugins(true);
    }

    pub(super) fn check_plugin_updates(&mut self) {
        if self.plugins.checking || self.plugins.items.is_empty() {
            return;
        }
        let items = self.plugins.items.clone();
        let game = self.sel().version.clone();
        let id = self.sel().id;
        self.plugins.checking = true;
        self.spawn_task(
            move || plugins::check_updates(&items, game.as_deref()).map_err(|e| format!("{e:#}")),
            move |app, res| {
                app.plugins.checking = false;
                if app.plugins.loaded_for != Some(id) {
                    return;
                }
                match res {
                    Ok(u) => {
                        app.plugins.checked = true;
                        let n = u.len();
                        app.plugins.updates = u;
                        if n == 0 {
                            app.toast(
                                tr!(
                                    "все плагины актуальны (по данным Modrinth)",
                                    "all plugins are up to date (per Modrinth)"
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

    pub(super) fn ask_apply_updates(&mut self, all: bool) {
        let paths: Vec<PathBuf> = if all {
            self.plugins
                .items
                .iter()
                .filter(|p| self.plugins.updates.contains_key(&p.path))
                .map(|p| p.path.clone())
                .collect()
        } else {
            match self.plugins.items.get(self.plugins.sel) {
                Some(p) if self.plugins.updates.contains_key(&p.path) => vec![p.path.clone()],
                Some(_) => {
                    return self.toast(
                        if self.plugins.checked {
                            tr!("для этого файла обновлений нет", "no update for this file")
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
        let names: Vec<String> = self
            .plugins
            .items
            .iter()
            .filter(|p| paths.contains(&p.path))
            .map(|p| format!("{} → {}", p.name, self.plugins.updates[&p.path].version))
            .collect();
        let tail = if self.sel().running() {
            tr!(" Изменения вступят в силу после перезапуска.", " Changes take effect after a restart.")
        } else {
            ""
        };
        self.popup = Some(Popup::Confirm {
            title: tr!("Обновить плагины", "Update plugins").into(),
            text: tr!(
                format!("{}. Старые файлы уйдут в {}/.{tail}", names.join(", "), plugins::OLD_DIR),
                format!("{}. Old files move to {}/.{tail}", names.join(", "), plugins::OLD_DIR)
            ),
            action: Action::ApplyUpdates(paths),
        });
    }

    pub(super) fn apply_updates(&mut self, paths: Vec<PathBuf>) {
        let jobs: Vec<(PluginInfo, UpdateInfo)> = self
            .plugins
            .items
            .iter()
            .filter(|p| paths.contains(&p.path))
            .filter_map(|p| Some((p.clone(), self.plugins.updates.get(&p.path)?.clone())))
            .collect();
        self.plugins.busy = Some(tr!("скачивание обновлений…", "downloading updates…").into());
        self.spawn_task(
            move || {
                let mut ok = 0;
                let mut errs = vec![];
                for (p, u) in &jobs {
                    match plugins::apply_update(p, u) {
                        Ok(_) => ok += 1,
                        Err(e) => errs.push(format!("{}: {e:#}", p.name)),
                    }
                }
                (ok, errs)
            },
            |app, (ok, errs)| {
                app.plugins.busy = None;
                app.plugins.updates.clear();
                app.plugins.checked = false;
                app.refresh_plugins(true);
                if errs.is_empty() {
                    app.toast(tr!(format!("обновлено: {ok}"), format!("updated: {ok}")), ToastKind::Ok);
                } else {
                    app.toast(errs.join("; "), ToastKind::Error);
                }
            },
        );
    }
}
