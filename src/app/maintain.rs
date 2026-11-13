//! Обслуживание сервера из окон-панелей: обновление ядра (клавиша `U` на «Статусе») и сеть за прокси
//! Velocity (клавиша `v` на «Статусе» у прокси).

use super::panel::{PanelKind, PanelLine};
use super::*;
use crate::core_update::{self as cu, Installed};
use crate::velocity::{self, Backend};

/// Что выбрано для обновления ядра.
pub struct CoreTarget {
    pub installed: Installed,
    pub version: String,
    pub major: bool,
}

fn progress_text(p: crate::create::Progress) -> String {
    match p {
        crate::create::Progress::Step(s) => s,
        crate::create::Progress::Download { what, done, total } => match total {
            Some(t) if t > 0 => format!("{what}: {} / {} ({}%)", human_bytes(done), human_bytes(t), done * 100 / t),
            _ => format!("{what}: {}", human_bytes(done)),
        },
    }
}

impl App {
    /// Строка «шаг выполнения» в открытой панели — из фоновой задачи.
    fn panel_progress_sender(&self) -> impl FnMut(String) + Send + 'static {
        let tx = self.tx.clone();
        move |text: String| {
            let _ = tx.send(AppEvent::Ui(UiTask(Box::new(move |app: &mut App| {
                if let Some(Popup::Panel(p)) = &mut app.popup {
                    p.busy = Some(text);
                }
            }))));
        }
    }

    // ───────────────────────────── обновление ядра ─────────────────────────────

    pub(super) fn open_core_update(&mut self) {
        let s = self.sel();
        if !s.is_local() {
            return self.toast(
                tr!("Обновление ядра — для локальных серверов", "Core updates are for local servers"),
                ToastKind::Info,
            );
        }
        let dir = s.cfg.dir().to_path_buf();
        let command = s.cfg.command.clone();
        let mut p = Panel::new(
            tr!(format!("Обновление ядра · {}", s.cfg.name), format!("Core update · {}", s.cfg.name)),
            PanelKind::CoreUpdate { installed: None, targets: vec![] },
            tr!("↑↓ выбор · Enter установить · Esc закрыть", "↑↓ select · Enter install · Esc close").into(),
        );
        p.busy = Some(tr!("проверяю версии…", "checking versions…").into());
        self.popup = Some(Popup::Panel(Box::new(p)));
        self.spawn_task(
            move || {
                let inst = cu::detect_with(&dir, &command)?;
                let check = cu::check(&inst).map_err(|e| format!("{e:#}"));
                Some((inst, check))
            },
            |app, res| app.with_panel(|p| fill_core_panel(p, res)),
        );
    }

    pub(super) fn pick_core_target(&mut self, p: &mut Panel) {
        let PanelKind::CoreUpdate { installed: Some(inst), targets } = &p.kind else { return };
        let Some(version) = targets.get(p.sel).cloned() else { return };
        if self.sel().running() {
            return self
                .toast(tr!("Сначала остановите сервер (Ctrl+X)", "Stop the server first (Ctrl+X)"), ToastKind::Info);
        }
        let major = cu::is_major_upgrade(inst, &version);
        let paths = self.sel().cfg.backup_paths.join(", ");
        let text = if major {
            tr!(
                format!(
                    "Перейти с {} на {version}? Мир будет обновлён до новой версии, вернуться назад нельзя. \
                     Перед заменой будет сделан полный бэкап ({paths}). Проверьте, что плагины/моды есть под {version}.",
                    inst.mc_version
                ),
                format!(
                    "Move from {} to {version}? The world gets upgraded and can't go back. \
                     A full backup ({paths}) is made first. Check that your plugins/mods support {version}.",
                    inst.mc_version
                )
            )
        } else {
            tr!(
                format!("Поставить новую сборку {version}? Старый jar переносится в .mctui-old/."),
                format!("Install the new {version} build? The old jar is moved to .mctui-old/.")
            )
        };
        self.popup = Some(Popup::Confirm {
            title: tr!("Обновление ядра", "Core update").into(),
            text,
            action: Action::ApplyCore(Box::new(CoreTarget { installed: inst.clone(), version, major })),
        });
    }

    pub(super) fn apply_core(&mut self, t: CoreTarget) {
        if self.sel().running() {
            return self
                .toast(tr!("Сначала остановите сервер (Ctrl+X)", "Stop the server first (Ctrl+X)"), ToastKind::Info);
        }
        let s = self.sel();
        let (id, cfg) = (s.id, s.cfg.clone());
        let mut p = Panel::new(
            tr!(format!("Обновление ядра · {}", cfg.name), format!("Core update · {}", cfg.name)),
            PanelKind::CoreUpdate { installed: None, targets: vec![] },
            tr!("Esc закрыть", "Esc close").into(),
        );
        p.busy = Some(tr!("начинаю…", "starting…").into());
        self.popup = Some(Popup::Panel(Box::new(p)));
        let mut say = self.panel_progress_sender();
        self.spawn_task(
            move || -> Result<(cu::Applied, Option<PathBuf>), String> {
                let backup = if t.major { Some(backup_before_upgrade(&cfg, &mut say)?) } else { None };
                let mut progress = |p: crate::create::Progress| say(progress_text(p));
                let applied =
                    cu::apply(cfg.dir(), &t.installed, &t.version, &mut progress).map_err(|e| format!("{e:#}"))?;
                Ok((applied, backup))
            },
            move |app, res| {
                let mut lines = vec![];
                match res {
                    Ok((a, backup)) => {
                        let build = a.build.as_deref().map(|b| format!(" #{b}")).unwrap_or_default();
                        lines.push(PanelLine::new(
                            Tone::Ok,
                            tr!(
                                format!("✓ Установлено: {}{build} ({})", a.version, a.jar),
                                format!("✓ Installed: {}{build} ({})", a.version, a.jar)
                            ),
                        ));
                        if let Some(b) = backup {
                            lines.push(PanelLine::new(
                                Tone::Info,
                                tr!(format!("Бэкап: {}", b.display()), format!("Backup: {}", b.display())),
                            ));
                        }
                        lines.extend(a.notes.iter().map(|n| PanelLine::new(Tone::Info, format!("• {n}"))));
                        lines.extend(a.warnings.iter().map(|w| PanelLine::new(Tone::Warn, format!("⚠ {w}"))));
                        if a.script.is_none()
                            && let Some(s) = app.servers.iter().find(|s| s.id == id)
                            && let Some(cmd) = cu::rewrite_command(&s.cfg.command, &a)
                        {
                            let mut cfg = s.cfg.clone();
                            cfg.command = cmd;
                            app.send(Request::UpdateServer { id, cfg: Box::new(cfg) });
                            lines.push(PanelLine::new(
                                Tone::Info,
                                tr!(
                                    "• Команда запуска в настройках обновлена",
                                    "• The start command in the config was updated"
                                ),
                            ));
                        }
                        lines.push(PanelLine::new(Tone::Muted, ""));
                        lines.push(PanelLine::new(
                            Tone::Info,
                            tr!("Запустите сервер: Ctrl+S", "Start the server: Ctrl+S"),
                        ));
                        app.toast(tr!("ядро обновлено", "core updated"), ToastKind::Ok);
                    }
                    Err(e) => {
                        lines.push(PanelLine::new(Tone::Bad, format!("✗ {e}")));
                        lines.push(PanelLine::new(
                            Tone::Muted,
                            tr!("Файлы сервера не изменены.", "The server files were left unchanged."),
                        ));
                    }
                }
                app.with_panel(|p| {
                    p.busy = None;
                    p.lines = lines;
                });
            },
        );
    }

    // ───────────────────────────── Velocity ─────────────────────────────

    pub(super) fn open_velocity_link(&mut self) {
        let s = self.sel();
        if !s.is_local() || !s.cfg.dir().join(velocity::CONFIG).is_file() {
            return self.toast(
                tr!(
                    "Это не прокси Velocity — создайте его: a → «Создать новый» → ядро Velocity",
                    "This is not a Velocity proxy — create one: a → “Create new” → Velocity core"
                ),
                ToastKind::Info,
            );
        }
        let proxy_dir = s.cfg.dir().to_path_buf();
        let backends: Vec<(String, PathBuf)> = self
            .servers
            .iter()
            .filter(|o| o.is_local() && o.cfg.dir() != proxy_dir && !o.cfg.dir().join(velocity::CONFIG).is_file())
            .map(|o| (o.cfg.name.clone(), o.cfg.dir().to_path_buf()))
            .collect();
        let mut p = Panel::new(
            tr!(format!("Сеть Velocity · {}", s.cfg.name), format!("Velocity network · {}", s.cfg.name)),
            PanelKind::Velocity { backends: vec![], chosen: vec![] },
            tr!(
                "↑↓ выбор · Пробел включить в сеть · Enter связать · Esc закрыть",
                "↑↓ select · Space add to network · Enter link · Esc close"
            )
            .into(),
        );
        p.lines = vec![
            PanelLine::new(
                Tone::Info,
                tr!(
                    "Игроки заходят на прокси, а он пускает их на выбранные серверы (/server <имя>).",
                    "Players join the proxy, which forwards them to the chosen servers (/server <name>)."
                ),
            ),
            PanelLine::new(
                Tone::Muted,
                tr!(
                    "Связка включает modern forwarding и online-mode=false на серверах; после — перезапустите всё.",
                    "Linking turns on modern forwarding and online-mode=false on the servers; restart everything afterwards."
                ),
            ),
        ];
        if backends.is_empty() {
            p.lines.push(PanelLine::new(
                Tone::Warn,
                tr!(
                    "Нет других локальных серверов — сначала добавьте их (a)",
                    "No other local servers — add some first (a)"
                ),
            ));
        }
        let list: Vec<Backend> = backends
            .into_iter()
            .map(|(name, dir)| {
                let port = crate::properties::Properties::load(&dir.join("server.properties"))
                    .ok()
                    .and_then(|p| p.get("server-port").and_then(|v| v.trim().parse().ok()))
                    .unwrap_or(25565);
                Backend { name: velocity::sanitize_name(&name), dir, port }
            })
            .collect();
        p.header = Some(format!("     {:<24} {}", tr!("Имя в прокси", "Name in proxy"), tr!("Адрес", "Address")));
        let chosen = vec![false; list.len()];
        p.items = velocity_items(&list, &chosen);
        p.kind = PanelKind::Velocity { backends: list, chosen };
        self.popup = Some(Popup::Panel(Box::new(p)));
    }

    pub(super) fn toggle_velocity_backend(&mut self, p: &mut Panel) {
        if let PanelKind::Velocity { backends, chosen } = &mut p.kind
            && let Some(c) = chosen.get_mut(p.sel)
        {
            *c = !*c;
            p.items = velocity_items(backends, chosen);
        }
    }

    pub(super) fn link_velocity(&mut self, p: &mut Panel) {
        let PanelKind::Velocity { backends, chosen } = &p.kind else { return };
        let picked: Vec<Backend> = backends.iter().zip(chosen).filter(|(_, c)| **c).map(|(b, _)| b.clone()).collect();
        if picked.is_empty() {
            return self.toast(tr!("Отметьте серверы пробелом", "Mark servers with Space"), ToastKind::Info);
        }
        let proxy_dir = self.sel().cfg.dir().to_path_buf();
        p.busy = Some(tr!("связываю…", "linking…").into());
        self.spawn_task(
            move || velocity::link(&proxy_dir, &picked).map_err(|e| format!("{e:#}")),
            |app, res| {
                let mut lines = vec![];
                match res {
                    Ok(r) => {
                        lines.extend(r.changes.iter().map(|c| PanelLine::new(Tone::Ok, format!("✓ {c}"))));
                        lines.extend(r.warnings.iter().map(|w| PanelLine::new(Tone::Warn, format!("⚠ {w}"))));
                        if !r.linked.is_empty() {
                            lines.push(PanelLine::new(Tone::Muted, ""));
                            lines.push(PanelLine::new(
                                Tone::Info,
                                tr!(
                                    format!("Через прокси после перезапуска: {}", r.linked.join(", ")),
                                    format!("Behind the proxy after a restart: {}", r.linked.join(", "))
                                ),
                            ));
                        }
                        app.toast(tr!("сеть настроена", "network configured"), ToastKind::Ok);
                    }
                    Err(e) => lines.push(PanelLine::new(Tone::Bad, format!("✗ {e}"))),
                }
                app.with_panel(|p| {
                    p.busy = None;
                    p.lines = lines;
                    p.items.clear();
                    p.header = None;
                    p.hint = tr!("Esc закрыть", "Esc close").into();
                });
            },
        );
    }
}

fn velocity_items(list: &[Backend], chosen: &[bool]) -> Vec<String> {
    list.iter()
        .zip(chosen)
        .map(|(b, c)| format!("[{}]  {:<24} 127.0.0.1:{}", if *c { "✓" } else { " " }, b.name, b.port))
        .collect()
}

fn fill_core_panel(p: &mut Panel, res: Option<(Installed, Result<cu::UpdateCheck, String>)>) {
    p.busy = None;
    let Some((inst, check)) = res else {
        p.lines = vec![PanelLine::new(
            Tone::Bad,
            tr!(
                "Не удалось определить ядро: нет jar Paper / Purpur / Fabric / Vanilla / Velocity в скрипте запуска",
                "Could not detect the core: no Paper / Purpur / Fabric / Vanilla / Velocity jar in the start script"
            ),
        )];
        return;
    };
    let mut lines = vec![PanelLine::new(
        Tone::Header,
        tr!(format!("Установлено: {}", inst.label()), format!("Installed: {}", inst.label())),
    )];
    lines.push(PanelLine::new(Tone::Muted, format!("jar: {}", inst.jar_name())));
    let mut items = vec![];
    let mut targets = vec![];
    match check {
        Err(e) => lines.push(PanelLine::new(Tone::Bad, format!("✗ {e}"))),
        Ok(c) if !c.has_updates() => {
            lines.push(PanelLine::new(Tone::Ok, tr!("✓ Установлена последняя версия", "✓ This is the latest version")))
        }
        Ok(c) => {
            if c.build_update {
                let b = c.latest_build.as_deref().map(|b| format!(" #{b}")).unwrap_or_default();
                items.push(tr!(
                    format!("Новая сборка{b} для {} — исправления, мир не меняется", inst.mc_version),
                    format!("New build{b} for {} — fixes, the world stays the same", inst.mc_version)
                ));
                targets.push(inst.mc_version.clone());
            }
            for v in &c.newer_versions {
                let warn = if cu::is_major_upgrade(&inst, v) {
                    tr!(
                        " — ⚠ новая версия, мир обновится (сначала бэкап)",
                        " — ⚠ new version, the world gets upgraded (backup first)"
                    )
                } else {
                    ""
                };
                items.push(format!("{} {v}{warn}", inst.core.label()));
                targets.push(v.clone());
            }
        }
    }
    p.lines = lines;
    p.items = items;
    p.sel = 0;
    p.kind = PanelKind::CoreUpdate { installed: Some(inst), targets };
}

/// Полный архив перед сменой версии Minecraft (сервер остановлен, save-off не нужен).
fn backup_before_upgrade(cfg: &ServerConfig, say: &mut impl FnMut(String)) -> Result<PathBuf, String> {
    let dir = cfg.backup_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let prefix = crate::backup::file_prefix(&cfg.name);
    let dest = dir.join(crate::backup::backup_name(&prefix, crate::config::BackupMode::Full));
    say(tr!("бэкап перед обновлением…", "backing up before the update…").into());
    crate::backup::create_archive(cfg.dir(), &cfg.backup_paths, &dest, |bytes| {
        say(tr!(format!("бэкап: {}", human_bytes(bytes)), format!("backup: {}", human_bytes(bytes))))
    })
    .map_err(|e| {
        tr!(
            format!("бэкап не удался, ядро не тронуто: {e:#}"),
            format!("backup failed, the core was not touched: {e:#}")
        )
    })?;
    Ok(dest)
}
