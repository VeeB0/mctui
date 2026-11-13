//! Инструменты на вкладке «Статус»: тестовая копия сервера (`T`), параметры Java (`J`) и проверка
//! плагинов/модов перед сменой версии Minecraft (в окне обновления ядра, `U`).

use super::maintain::CoreTarget;
use super::panel::{PanelKind, PanelLine};
use super::*;
use crate::clone::{self, ClonePlan};
use crate::compat::{self, Compat, CompatReport};
use crate::core_update::{self as cu, Installed};
use crate::jvm::{self, Change, Profile};

fn mb_text(mb: u32) -> String {
    if mb.is_multiple_of(1024) { format!("{}G", mb / 1024) } else { format!("{mb}M") }
}

/// «6», «6G», «6.5G», «6144M», «6144» → МиБ. Без единиц: до 64 — гигабайты, иначе мегабайты.
pub fn parse_memory(s: &str) -> Option<u32> {
    let t = s.trim().to_lowercase().replace(',', ".");
    let t = t.trim_end_matches(['b', 'б']);
    let (num, mul) = if let Some(n) = t.strip_suffix(['g', 'г']) {
        (n, 1024.0)
    } else if let Some(n) = t.strip_suffix(['m', 'м']) {
        (n, 1.0)
    } else {
        (t, 0.0)
    };
    let v: f64 = num.trim().parse().ok()?;
    let mb = match mul {
        0.0 if v <= 64.0 => v * 1024.0,
        0.0 => v,
        m => v * m,
    };
    (mb >= jvm::MIN_XMX_MB as f64 && mb <= 1024.0 * 1024.0).then_some(mb.round() as u32)
}

impl App {
    // ───────────────────────────── тестовая копия ─────────────────────────────

    pub(super) fn open_clone(&mut self) {
        let s = self.sel();
        if !s.is_local() {
            return self.toast(tr!("Копия — для локальных серверов", "Copies are for local servers"), ToastKind::Info);
        }
        let src = s.cfg.clone();
        let running = s.running();
        let all: Vec<ServerConfig> = self.servers.iter().map(|s| s.cfg.clone()).collect();
        self.toast(tr!("считаю размер…", "measuring…"), ToastKind::Info);
        self.spawn_task(
            move || clone::plan(&src, &all).map_err(|e| format!("{e:#}")),
            move |app, res| match res {
                Err(e) => app.toast(e, ToastKind::Error),
                Ok(plan) => app.confirm_clone(plan, running),
            },
        );
    }

    fn confirm_clone(&mut self, plan: ClonePlan, running: bool) {
        let mut ports = format!("{}", plan.port);
        if let Some(r) = plan.rcon_port {
            ports += &format!(", RCON {r}");
        }
        if let Some(q) = plan.query_port {
            ports += &format!(", query {q}");
        }
        let free = plan
            .free_space
            .map(|f| tr!(format!(" · свободно {}", human_bytes(f)), format!(" · {} free", human_bytes(f))))
            .unwrap_or_default();
        let mut text = tr!(
            format!(
                "Сделать тестовую копию «{}»?\nИмя: {}\nПапка: {}\nПорт: {ports}\nРазмер: {} в {} файлах{free}",
                self.sel().cfg.name,
                plan.name,
                plan.dest.display(),
                human_bytes(plan.bytes),
                plan.files
            ),
            format!(
                "Make a test copy of “{}”?\nName: {}\nFolder: {}\nPort: {ports}\nSize: {} in {} files{free}",
                self.sel().cfg.name,
                plan.name,
                plan.dest.display(),
                human_bytes(plan.bytes),
                plan.files
            )
        );
        if !plan.skipped.is_empty() {
            let mut list: Vec<&str> = plan.skipped.iter().map(String::as_str).take(6).collect();
            if plan.skipped.len() > 6 {
                list.push("…");
            }
            text += &tr!(format!("\nНе копируется: {}", list.join(", ")), format!("\nNot copied: {}", list.join(", ")));
        }
        text += tr!(
            "\nУ копии нет автозапуска и облачных копий; оригинал не меняется.",
            "\nThe copy has no autostart or cloud copies; the original is untouched."
        );
        if running {
            text += tr!(
                "\n⚠ Сервер запущен: перед копированием мир сохраняется (save-all), но надёжнее копировать остановленный.",
                "\n⚠ The server is running: the world is saved first (save-all), but copying a stopped server is safer."
            );
        }
        self.popup = Some(Popup::Confirm {
            title: tr!("Тестовая копия", "Test copy").into(),
            text,
            action: Action::CloneServer(Box::new(plan)),
        });
    }

    pub(super) fn run_clone(&mut self, plan: ClonePlan) {
        let s = self.sel();
        let (id, src, running) = (s.id, s.cfg.clone(), s.running());
        if running {
            self.send(Request::Command { id, cmd: "save-all flush".into(), user: false });
        }
        let mut p = Panel::new(
            tr!(format!(" Тестовая копия · {} ", src.name), format!(" Test copy · {} ", src.name)),
            PanelKind::Task,
            tr!("Esc закрыть (копирование продолжится)", "Esc close (copying continues)").into(),
        );
        p.busy = Some(tr!("готовлюсь…", "preparing…").into());
        self.popup = Some(Popup::Panel(Box::new(p)));
        let mut say = self.panel_progress_sender();
        self.spawn_task(
            move || {
                if running {
                    // Дать серверу дописать мир после save-all.
                    std::thread::sleep(Duration::from_secs(3));
                }
                let mut last = u64::MAX;
                let mut progress = |done: u64, total: u64| {
                    let pct = (done * 100).checked_div(total).unwrap_or(100);
                    if pct != last {
                        last = pct;
                        say(tr!(
                            format!("копирую: {} / {} ({pct}%)", human_bytes(done), human_bytes(total)),
                            format!("copying: {} / {} ({pct}%)", human_bytes(done), human_bytes(total))
                        ));
                    }
                };
                clone::run(&plan, &src, &mut progress).map(|cfg| (cfg, plan)).map_err(|e| format!("{e:#}"))
            },
            |app, res| {
                let mut lines = vec![];
                match res {
                    Ok((cfg, plan)) => {
                        lines.push(PanelLine::new(
                            Tone::Ok,
                            tr!(
                                format!("✓ Копия «{}» готова: {}", cfg.name, plan.dest.display()),
                                format!("✓ Copy “{}” is ready: {}", cfg.name, plan.dest.display())
                            ),
                        ));
                        lines.push(PanelLine::new(
                            Tone::Info,
                            tr!(
                                format!("Порт копии: {} — заходите на localhost:{}", plan.port, plan.port),
                                format!("The copy's port: {} — join localhost:{}", plan.port, plan.port)
                            ),
                        ));
                        lines.push(PanelLine::new(
                            Tone::Info,
                            tr!(
                                "Пробуйте на копии новое ядро (U) или плагины, затем повторите на основном сервере.",
                                "Try a new core (U) or plugins on the copy, then repeat on the main server."
                            ),
                        ));
                        lines.push(PanelLine::new(
                            Tone::Warn,
                            tr!(
                                "⚠ Плагины со своими портами (Dynmap, BlueMap, Geyser, Voice Chat…) в копии не запустятся, пока работает оригинал — поменяйте их порты в настройках плагинов.",
                                "⚠ Plugins with their own ports (Dynmap, BlueMap, Geyser, Voice Chat…) won't start in the copy while the original runs — change their ports in the plugin configs."
                            ),
                        ));
                        lines.push(PanelLine::new(
                            Tone::Muted,
                            tr!(
                                "Копия выбрана в списке слева: Ctrl+S — запустить, Del — убрать из списка.",
                                "The copy is selected on the left: Ctrl+S to start, Del to remove it from the list."
                            ),
                        ));
                        let popup = app.popup.take();
                        app.add_server(cfg);
                        app.popup = popup;
                    }
                    Err(e) => {
                        lines.push(PanelLine::new(Tone::Bad, format!("✗ {e}")));
                        lines.push(PanelLine::new(
                            Tone::Muted,
                            tr!("Оригинал не изменён.", "The original was left unchanged."),
                        ));
                    }
                }
                app.with_panel(|p| {
                    p.busy = None;
                    p.lines = lines;
                    p.hint = tr!("Esc закрыть", "Esc close").into();
                });
            },
        );
    }

    // ───────────────────────────── параметры Java ─────────────────────────────

    pub(super) fn open_jvm(&mut self) {
        let s = self.sel();
        if !s.is_local() {
            return self.toast(
                tr!("Параметры Java — для локальных серверов", "Java settings are for local servers"),
                ToastKind::Info,
            );
        }
        let dir = s.cfg.dir().to_path_buf();
        let command = s.cfg.command.clone();
        let version = s.version.clone();
        let mut p = Panel::new(
            tr!(format!(" Java и память · {} ", s.cfg.name), format!(" Java and memory · {} ", s.cfg.name)),
            PanelKind::Java { changes: vec![], xms_equal: false, xmx: None },
            tr!("↑↓ выбор · Enter применить · Esc закрыть", "↑↓ select · Enter apply · Esc close").into(),
        );
        p.busy = Some(tr!("читаю строку запуска…", "reading the start line…").into());
        self.popup = Some(Popup::Panel(Box::new(p)));
        self.spawn_task(
            move || {
                let launch = jvm::read(&dir, &command).map_err(|e| format!("{e:#}"))?;
                let inst = cu::detect_with(&dir, &command);
                let core = inst.as_ref().map(|i| i.core);
                let mc = inst.as_ref().map(|i| i.mc_version.clone()).or(version);
                let required = mc.as_deref().and_then(|v| {
                    jvm::known_required_java(core, v).or_else(|| core.and_then(|c| jvm::fetch_required_java(c, v)))
                });
                let java = jvm::check_java(&dir, &launch.java);
                let advice = jvm::advise_with(&launch, core, jvm::total_ram_mb(), java, required);
                Ok::<_, String>((launch, advice))
            },
            |app, res| app.with_panel(|p| fill_jvm_panel(p, res)),
        );
    }

    pub(super) fn pick_jvm(&mut self, p: &mut Panel) {
        let PanelKind::Java { changes, xms_equal, xmx } = &p.kind else { return };
        let Some(choice) = changes.get(p.sel).copied() else { return };
        match choice {
            Some(change) => self.preview_jvm(change),
            None => {
                let current = xmx.map(mb_text).unwrap_or_default();
                self.popup = Some(Popup::Edit {
                    title: tr!(
                        " Память сервера (-Xmx), например 6G или 4096M ",
                        " Server memory (-Xmx), e.g. 6G or 4096M "
                    )
                    .into(),
                    input: TextInput::with(&current),
                    target: EditTarget::JvmMemory { xms_equal: *xms_equal },
                });
            }
        }
    }

    pub(super) fn set_jvm_memory(&mut self, v: &str, xms_equal: bool) {
        match parse_memory(v) {
            Some(mb) => self.preview_jvm(Change { xmx_mb: Some(mb), xms_equal, profile: None }),
            None => {
                self.open_jvm();
                self.toast(
                    tr!(
                        format!("не понял «{v}»: напишите, например, 6G или 4096M (не меньше {} МБ)", jvm::MIN_XMX_MB),
                        format!("could not read “{v}”: write e.g. 6G or 4096M (at least {} MB)", jvm::MIN_XMX_MB)
                    ),
                    ToastKind::Error,
                );
            }
        }
    }

    fn preview_jvm(&mut self, change: Change) {
        let s = self.sel();
        let (dir, command, running) = (s.cfg.dir().to_path_buf(), s.cfg.command.clone(), s.running());
        self.with_panel(|p| p.busy = Some(tr!("готовлю изменение…", "preparing the change…").into()));
        self.spawn_task(
            move || jvm::preview(&dir, &command, &change).map_err(|e| format!("{e:#}")),
            move |app, res| {
                app.with_panel(|p| p.busy = None);
                match res {
                    Err(e) => app.toast(e, ToastKind::Error),
                    Ok(a) if !a.changed() => app.toast(tr!("ничего не меняется", "nothing changes"), ToastKind::Info),
                    Ok(a) => {
                        let place = match &a.script {
                            Some(s) => s.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                            None => tr!("команда запуска в настройках", "the start command in the config").into(),
                        };
                        let mut text = tr!(
                            format!("Изменить {place}?\nБыло:  {}\nСтанет: {}", a.before.trim(), a.after.trim()),
                            format!("Change {place}?\nBefore: {}\nAfter:  {}", a.before.trim(), a.after.trim())
                        );
                        if a.script.is_some() {
                            text += tr!(
                                "\nКопия старого скрипта сохранится в .pechka-old/.",
                                "\nA copy of the old script is kept in .pechka-old/."
                            );
                        }
                        if running {
                            text += tr!(
                                "\nСервер запущен: изменения вступят в силу после перезапуска (Ctrl+R).",
                                "\nThe server is running: the change takes effect after a restart (Ctrl+R)."
                            );
                        }
                        app.popup = Some(Popup::Confirm {
                            title: tr!("Параметры Java", "Java settings").into(),
                            text,
                            action: Action::JvmApply(change),
                        });
                    }
                }
            },
        );
    }

    pub(super) fn apply_jvm(&mut self, change: Change) {
        let s = self.sel();
        let (id, dir, command, running) = (s.id, s.cfg.dir().to_path_buf(), s.cfg.command.clone(), s.running());
        self.spawn_task(
            move || jvm::apply(&dir, &command, &change).map_err(|e| format!("{e:#}")),
            move |app, res| match res {
                Err(e) => app.toast(e, ToastKind::Error),
                Ok(a) => {
                    if let Some(cmd) = a.new_command
                        && let Some(s) = app.servers.iter().find(|s| s.id == id)
                    {
                        let mut cfg = s.cfg.clone();
                        cfg.command = cmd;
                        app.send(Request::UpdateServer { id, cfg: Box::new(cfg) });
                    }
                    let restart = if running {
                        tr!(" — перезапустите сервер (Ctrl+R)", " — restart the server (Ctrl+R)")
                    } else {
                        ""
                    };
                    app.toast(
                        tr!(format!("параметры Java сохранены{restart}"), format!("Java settings saved{restart}")),
                        ToastKind::Ok,
                    );
                    if app.popup.is_none() {
                        app.open_jvm();
                    }
                }
            },
        );
    }

    // ───────────────────────────── плагины перед сменой версии ─────────────────────────────

    /// Перед переходом на новую версию Minecraft: какие плагины/моды под неё есть.
    pub(super) fn check_compat(&mut self, p: &mut Panel, inst: Installed, version: String) {
        let dir = self.sel().cfg.dir().to_path_buf();
        p.busy = Some(tr!(
            format!("проверяю плагины и моды под {version}…"),
            format!("checking plugins and mods for {version}…")
        ));
        self.spawn_task(
            move || {
                let r = compat::check(&dir, &inst, &version).map_err(|e| format!("{e:#}"));
                (inst, version, r)
            },
            |app, (inst, version, res)| match res {
                Ok(report) if report.items.is_empty() => {
                    app.with_panel(|p| p.busy = None);
                    app.confirm_core(inst, version, true, Some(report), None);
                }
                Ok(report) => app.with_panel(|p| fill_compat_panel(p, inst, version, report, false)),
                Err(e) => {
                    app.with_panel(|p| p.busy = None);
                    app.confirm_core(inst, version, true, None, Some(e));
                }
            },
        );
    }

    /// Подтверждение обновления ядра (с итогом проверки плагинов, если была).
    pub(super) fn confirm_core(
        &mut self,
        inst: Installed,
        version: String,
        major: bool,
        compat: Option<CompatReport>,
        compat_error: Option<String>,
    ) {
        let paths = self.sel().cfg.backup_paths.join(", ");
        let mut text = if major {
            tr!(
                format!(
                    "Перейти с {} на {version}? Мир будет обновлён до новой версии, вернуться назад нельзя. \
                     Перед заменой будет сделан полный бэкап ({paths}).",
                    inst.mc_version
                ),
                format!(
                    "Move from {} to {version}? The world gets upgraded and can't go back. \
                     A full backup ({paths}) is made first.",
                    inst.mc_version
                )
            )
        } else {
            tr!(
                format!("Поставить новую сборку {version}? Старый jar переносится в .pechka-old/."),
                format!("Install the new {version} build? The old jar is moved to .pechka-old/.")
            )
        };
        if let Some(r) = &compat {
            if !r.items.is_empty() {
                text +=
                    &tr!(format!("\nПлагины и моды: {}", r.summary()), format!("\nPlugins and mods: {}", r.summary()));
                if r.missing() > 0 {
                    text += tr!(
                        "\n⚠ Те, у кого нет сборки, на новой версии могут не загрузиться.",
                        "\n⚠ The ones without a build may fail to load on the new version."
                    );
                }
                let n = r.updates().len();
                if n > 0 {
                    text += &tr!(
                        format!("\nПосле обновления можно поставить новые сборки: {n} (клавиша u)."),
                        format!("\nAfterwards you can install new builds: {n} (key u).")
                    );
                }
            } else if let Some(note) = &r.note {
                text += &format!("\n{note}");
            }
        }
        if let Some(e) = compat_error {
            text +=
                &tr!(format!("\n⚠ Проверить плагины не удалось: {e}"), format!("\n⚠ Could not check the plugins: {e}"));
        }
        self.popup = Some(Popup::Confirm {
            title: tr!("Обновление ядра", "Core update").into(),
            text,
            action: Action::ApplyCore(Box::new(CoreTarget {
                installed: inst,
                version,
                major,
                compat: compat.filter(|r| !r.updates().is_empty()).map(Box::new),
            })),
        });
    }

    /// Поставить новые сборки плагинов/модов из отчёта (после обновления ядра).
    pub(super) fn install_compat(&mut self, p: &mut Panel) {
        let PanelKind::Compat { report, done: true, .. } = &p.kind else { return };
        if report.updates().is_empty() {
            return;
        }
        let report = (**report).clone();
        let dir = self.sel().cfg.dir().to_path_buf();
        p.busy = Some(tr!("скачиваю сборки…", "downloading builds…").into());
        let mut say = self.panel_progress_sender();
        self.spawn_task(
            move || {
                let items = report.updates();
                compat::install(&dir, &items, &mut say).map_err(|e| format!("{e:#}"))
            },
            |app, res| {
                let lines: Vec<PanelLine> = match res {
                    Ok(notes) => {
                        app.toast(tr!("сборки плагинов обновлены", "plugin builds updated"), ToastKind::Ok);
                        notes.into_iter().map(|n| PanelLine::new(Tone::Info, format!("• {n}"))).collect()
                    }
                    Err(e) => vec![PanelLine::new(Tone::Bad, format!("✗ {e}"))],
                };
                app.with_panel(|p| {
                    p.busy = None;
                    p.lines = lines;
                    p.items.clear();
                    p.header = None;
                    p.kind = PanelKind::Task;
                    p.hint = tr!("Esc закрыть", "Esc close").into();
                });
                app.refresh_plugins(true);
            },
        );
    }
}

fn fill_jvm_panel(p: &mut Panel, res: Result<(jvm::Launch, jvm::Advice), String>) {
    p.busy = None;
    let (launch, advice) = match res {
        Ok(v) => v,
        Err(e) => {
            p.lines = vec![
                PanelLine::new(Tone::Bad, format!("✗ {e}")),
                PanelLine::new(
                    Tone::Muted,
                    tr!(
                        "Параметры Java можно поменять вручную в скрипте запуска сервера.",
                        "You can change the Java settings by hand in the server's start script."
                    ),
                ),
            ];
            p.hint = tr!("Esc закрыть", "Esc close").into();
            return;
        }
    };
    let mut lines = vec![];
    let place = match (&launch.script, launch.line_no) {
        (Some(s), Some(n)) => format!("{}:{n}", s.file_name().unwrap_or_default().to_string_lossy()),
        (Some(s), None) => s.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        _ => tr!("команда из настроек", "command from the config").into(),
    };
    lines.push(PanelLine::new(Tone::Header, tr!(format!("Запуск: {place}"), format!("Start line: {place}"))));
    lines.push(PanelLine::new(Tone::Muted, launch.line.trim().to_string()));
    let java = match &advice.java {
        jvm::JavaCheck::Found { major, bits64 } => {
            let v = major.map(|m| m.to_string()).unwrap_or_else(|| "?".into());
            let bits = match bits64 {
                Some(true) => " (64-bit)",
                Some(false) => " (32-bit)",
                None => "",
            };
            format!("Java {v}{bits}")
        }
        jvm::JavaCheck::NotFound => tr!("Java не найдена", "Java not found").into(),
        jvm::JavaCheck::Broken => tr!("Java не запускается", "Java does not run").into(),
        jvm::JavaCheck::Variable => tr!("Java задана переменной", "Java is set by a variable").into(),
    };
    let need =
        advice.java_required.map(|r| tr!(format!(" · нужна {r}+"), format!(" · needs {r}+"))).unwrap_or_default();
    lines.push(PanelLine::new(Tone::Info, format!("{java}{need}")));
    let mem = |v: Option<u32>| v.map(mb_text).unwrap_or_else(|| "—".into());
    lines.push(PanelLine::new(
        Tone::Info,
        tr!(
            format!(
                "Память: -Xms {} · -Xmx {} · в компьютере {} · советуем {}",
                mem(launch.xms_mb),
                mem(launch.xmx_mb),
                human_bytes(advice.total_ram_mb * 1024 * 1024),
                mb_text(advice.recommended_mb)
            ),
            format!(
                "Memory: -Xms {} · -Xmx {} · computer has {} · we suggest {}",
                mem(launch.xms_mb),
                mem(launch.xmx_mb),
                human_bytes(advice.total_ram_mb * 1024 * 1024),
                mb_text(advice.recommended_mb)
            )
        ),
    ));
    lines.push(PanelLine::new(
        Tone::Info,
        tr!(format!("Флаги: {}", launch.profile.label()), format!("Flags: {}", launch.profile.label())),
    ));
    lines.extend(advice.warnings.iter().map(|w| PanelLine::new(Tone::Warn, format!("⚠ {w}"))));
    lines.extend(advice.hints.iter().map(|h| PanelLine::new(Tone::Muted, format!("• {h}"))));

    let mut items = vec![];
    let mut changes = vec![];
    let suggested = advice.suggested_change();
    let differs = launch.xmx_mb != suggested.xmx_mb || Some(launch.profile) != suggested.profile;
    if differs {
        items.push(tr!(
            format!("★ Как советуем: -Xmx {} · {}", mb_text(advice.recommended_mb), advice.recommended_profile.label()),
            format!("★ As suggested: -Xmx {} · {}", mb_text(advice.recommended_mb), advice.recommended_profile.label())
        ));
        changes.push(Some(suggested));
    }
    items.push(tr!("Задать память…", "Set memory…").into());
    changes.push(None);
    for prof in Profile::CHOICES {
        let now = if prof == launch.profile { tr!(" (сейчас)", " (current)") } else { "" };
        items.push(format!("{}{now} — {}", prof.label(), prof.description()));
        changes.push(Some(Change { xmx_mb: None, xms_equal: prof == Profile::Aikar, profile: Some(prof) }));
    }
    p.lines = lines;
    p.items = items;
    p.sel = 0;
    p.kind = PanelKind::Java { changes, xms_equal: launch.profile == Profile::Aikar, xmx: launch.xmx_mb };
}

/// Окно обновления ядра со списком плагинов/модов и их готовностью к `version`.
pub(super) fn fill_compat_panel(p: &mut Panel, inst: Installed, version: String, report: CompatReport, done: bool) {
    p.busy = None;
    let mut lines = vec![PanelLine::new(
        Tone::Header,
        tr!(
            format!("Плагины и моды под {}: {}", report.target, report.summary()),
            format!("Plugins and mods for {}: {}", report.target, report.summary())
        ),
    )];
    if let Some(n) = &report.note {
        lines.push(PanelLine::new(Tone::Muted, n.clone()));
    }
    if report.missing() > 0 && !done {
        lines.push(PanelLine::new(
            Tone::Warn,
            tr!(
                "⚠ Без сборки под новую версию плагин может не загрузиться. Можно подождать обновления или проверить на тестовой копии (T).",
                "⚠ Without a build for the new version a plugin may fail to load. Wait for an update or try it on a test copy (T)."
            ),
        ));
    }
    let n = report.updates().len();
    if done && n > 0 {
        lines.push(PanelLine::new(
            Tone::Info,
            tr!(
                format!("u — поставить новые сборки ({n}); старые файлы уйдут в .pechka-old/"),
                format!("u — install the new builds ({n}); old files go to .pechka-old/")
            ),
        ));
    }
    let items = report
        .items
        .iter()
        .map(|i| {
            let name = &i.plugin.name;
            let cur = i.plugin.version.as_deref().unwrap_or("");
            match &i.compat {
                Compat::Ready { version, update: Some(u) } => {
                    tr!(
                        format!("✓ {name} {cur} → {version} ({})", u.source),
                        format!("✓ {name} {cur} → {version} ({})", u.source)
                    )
                }
                Compat::Ready { .. } => tr!(format!("✓ {name} {cur} — подходит"), format!("✓ {name} {cur} — works")),
                Compat::Missing { latest_supported } => {
                    let last = latest_supported
                        .as_deref()
                        .map(|v| tr!(format!(" (последняя: {v})"), format!(" (latest: {v})")))
                        .unwrap_or_default();
                    let page = i.page.as_deref().map(|u| format!(" · {u}")).unwrap_or_default();
                    tr!(
                        format!("✗ {name} {cur} — нет сборки{last}{page}"),
                        format!("✗ {name} {cur} — no build{last}{page}")
                    )
                }
                Compat::Unknown { why } => format!("? {name} {cur} — {why}"),
            }
        })
        .collect();
    p.lines = lines;
    p.items = items;
    p.sel = 0;
    p.header = None;
    p.hint = if done {
        tr!("u поставить сборки · ↑↓ прокрутка · Esc закрыть", "u install builds · ↑↓ scroll · Esc close").into()
    } else {
        tr!("Enter — продолжить обновление ядра · Esc отмена", "Enter — continue the core update · Esc cancel").into()
    };
    p.kind = PanelKind::Compat { installed: Box::new(inst), version, report: Box::new(report), done };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_input() {
        assert_eq!(parse_memory("6"), Some(6144));
        assert_eq!(parse_memory("6G"), Some(6144));
        assert_eq!(parse_memory("6,5 g"), Some(6656));
        assert_eq!(parse_memory("4096M"), Some(4096));
        assert_eq!(parse_memory("4096"), Some(4096));
        assert_eq!(parse_memory("8гб"), Some(8192));
        assert_eq!(parse_memory("8г"), Some(8192));
        assert_eq!(parse_memory("12GB"), Some(12288));
        assert_eq!(parse_memory("abc"), None);
        assert_eq!(parse_memory("100M"), None);
    }
}
