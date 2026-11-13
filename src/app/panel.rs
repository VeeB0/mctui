//! Окна-панели поверх главного экрана: «Как друзьям зайти», поиск плагинов, поиск по всем логам,
//! статистика игроков. Панель живёт в `App::popup` (`Popup::Panel`), фоновые задачи обновляют её
//! через `with_panel`, если она всё ещё открыта.

use super::*;
use crate::network::{Facts, HintLevel};
use crate::plugin_search::{self as ps, InstallPlan, SearchHit, Target};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Header,
    Ok,
    Info,
    Warn,
    Bad,
    Muted,
}

pub struct PanelLine {
    pub text: String,
    pub tone: Tone,
}

impl PanelLine {
    pub fn new(tone: Tone, text: impl Into<String>) -> Self {
        Self { text: text.into(), tone }
    }
}

pub enum PanelKind {
    Network {
        facts: Box<Facts>,
    },
    PluginSearch {
        target: Target,
        query: String,
        hits: Vec<SearchHit>,
        offset: usize,
        more: bool,
    },
    LogSearch {
        query: String,
        matches: Vec<crate::crash::LogMatch>,
    },
    Players,
    CoreUpdate {
        installed: Option<crate::core_update::Installed>,
        targets: Vec<String>,
    },
    Velocity {
        backends: Vec<crate::velocity::Backend>,
        chosen: Vec<bool>,
    },
    /// Ход фоновой работы и её итог, без своих клавиш.
    Task,
    /// Параметры Java: пункт списка → изменение (`None` — ввести память вручную).
    Java {
        changes: Vec<Option<crate::jvm::Change>>,
        xms_equal: bool,
        xmx: Option<u32>,
    },
    /// Готовность плагинов/модов к версии `version`; `done` — ядро уже обновлено.
    Compat {
        installed: Box<crate::core_update::Installed>,
        version: String,
        report: Box<crate::compat::CompatReport>,
        done: bool,
    },
}

pub struct Panel {
    pub title: String,
    pub kind: PanelKind,
    pub lines: Vec<PanelLine>,
    /// Заголовок колонок списка.
    pub header: Option<String>,
    pub items: Vec<String>,
    pub sel: usize,
    /// Прокрутка текста (когда списка нет).
    pub scroll: usize,
    pub busy: Option<String>,
    pub hint: String,
    pub input: Option<TextInput>,
    pub input_focus: bool,
}

impl Panel {
    pub(super) fn new(title: String, kind: PanelKind, hint: String) -> Self {
        Self {
            title,
            kind,
            lines: vec![],
            header: None,
            items: vec![],
            sel: 0,
            scroll: 0,
            busy: None,
            hint,
            input: None,
            input_focus: false,
        }
    }
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

fn short_duration(d: Duration) -> String {
    let m = d.as_secs() / 60;
    if m < 60 {
        format!("{m} {}", tr!("мин", "min"))
    } else {
        format!("{}:{:02} {}", m / 60, m % 60, tr!("ч", "h"))
    }
}

fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..120 => tr!("только что", "just now").into(),
        _ if secs < 3600 => tr!(format!("{} мин назад", secs / 60), format!("{} min ago", secs / 60)),
        _ if secs < 86_400 => tr!(format!("{} ч назад", secs / 3600), format!("{} h ago", secs / 3600)),
        _ => tr!(format!("{} дн назад", secs / 86_400), format!("{} d ago", secs / 86_400)),
    }
}

impl App {
    /// Изменить открытую панель (если её ещё не закрыли).
    pub(super) fn with_panel(&mut self, f: impl FnOnce(&mut Panel)) {
        if let Some(Popup::Panel(p)) = &mut self.popup {
            f(p);
        }
    }

    // ───────────────────────────── как друзьям зайти ─────────────────────────────

    pub(super) fn open_network(&mut self) {
        let s = self.sel();
        if !s.is_local() {
            return self.toast(
                tr!("Подсказки по сети — для локальных серверов", "Network hints are for local servers"),
                ToastKind::Info,
            );
        }
        let mut facts = Facts { running: s.running(), ..Default::default() };
        if let Some(p) = &s.props {
            facts.apply_properties(p);
        }
        let mut panel = Panel::new(
            tr!(" Как друзьям зайти ", " How friends can join ").into(),
            PanelKind::Network { facts: Box::new(facts.clone()) },
            tr!(
                "p — проверить порт из интернета · ↑↓ прокрутка · Esc — закрыть",
                "p — check the port from the internet · ↑↓ scroll · Esc — close"
            )
            .into(),
        );
        panel.busy = Some(tr!("смотрю адреса…", "looking up addresses…").into());
        self.popup = Some(Popup::Panel(Box::new(panel)));
        self.probe_network(facts, false);
    }

    fn probe_network(&mut self, mut facts: Facts, port_check: bool) {
        self.spawn_task(
            move || {
                facts.probe(port_check);
                facts
            },
            |app, facts| {
                app.with_panel(|p| {
                    p.busy = None;
                    p.lines = crate::network::hints(&facts)
                        .into_iter()
                        .map(|h| {
                            let tone = match h.level {
                                HintLevel::Header => Tone::Header,
                                HintLevel::Ok => Tone::Ok,
                                HintLevel::Info => Tone::Info,
                                HintLevel::Warn => Tone::Warn,
                                HintLevel::Bad => Tone::Bad,
                            };
                            PanelLine::new(tone, h.text)
                        })
                        .collect();
                    p.kind = PanelKind::Network { facts: Box::new(facts) };
                });
            },
        );
    }

    // ───────────────────────────── поиск плагинов ─────────────────────────────

    pub(super) fn open_plugin_search(&mut self) {
        let s = self.sel();
        if !s.is_local() {
            return;
        }
        let hint = s.cfg.command.join(" ");
        let Some(target) = Target::detect(s.cfg.dir(), &hint, s.version.as_deref()) else {
            return self.toast(
                tr!(
                    "Не удалось понять ядро сервера (Paper / Fabric / …) — поиск недоступен",
                    "Could not tell the server core (Paper / Fabric / …) — search is unavailable"
                ),
                ToastKind::Error,
            );
        };
        let what = match target.kind() {
            crate::plugins::PluginKind::Plugin => tr!("плагинов", "plugins"),
            _ => tr!("модов", "mods"),
        };
        let ver = target.game_version.clone().map(|v| format!(" {v}")).unwrap_or_default();
        let mut panel = Panel::new(
            tr!(
                format!(" Поиск {what} · {}{ver} ", target.platform.label()),
                format!(" Search {what} · {}{ver} ", target.platform.label())
            ),
            PanelKind::PluginSearch { target, query: String::new(), hits: vec![], offset: 0, more: false },
            tr!(
                "Enter — искать / установить · ↓ к списку · Tab — к строке поиска · n — ещё · Esc — закрыть",
                "Enter — search / install · ↓ to the list · Tab — to the search box · n — more · Esc — close"
            )
            .into(),
        );
        panel.input = Some(TextInput::default());
        panel.input_focus = true;
        panel.lines = vec![PanelLine::new(
            Tone::Muted,
            tr!(
                "Введите название (пустой запрос — популярное). Источники: Modrinth, Hangar.",
                "Type a name (an empty query shows popular ones). Sources: Modrinth, Hangar."
            ),
        )];
        self.popup = Some(Popup::Panel(Box::new(panel)));
    }

    fn run_plugin_search(&mut self, p: &mut Panel, next_page: bool) {
        let PanelKind::PluginSearch { target, query, hits, offset, .. } = &mut p.kind else { return };
        if !next_page {
            *query = p.input.as_ref().map(|i| i.value.trim().to_string()).unwrap_or_default();
            hits.clear();
            *offset = 0;
            p.items.clear();
            p.sel = 0;
        } else {
            *offset += ps::PAGE_SIZE;
        }
        let (q, t, off) = (query.clone(), target.clone(), *offset);
        p.busy = Some(tr!("ищу…", "searching…").into());
        self.spawn_task(
            move || ps::search(&q, &t, off),
            |app, res| {
                let mut warn = None;
                app.with_panel(|p| {
                    p.busy = None;
                    let PanelKind::PluginSearch { hits, more, .. } = &mut p.kind else { return };
                    match res {
                        Ok(r) => {
                            for h in r.hits {
                                if !hits.iter().any(|x| x.source == h.source && x.id == h.id) {
                                    hits.push(h);
                                }
                            }
                            *more = r.more;
                            warn = (!r.warnings.is_empty()).then(|| r.warnings.join("; "));
                            p.header = Some(format!(
                                "{}{}{}{}",
                                pad(tr!("Название", "Name"), 28),
                                pad(tr!("Автор", "Author"), 18),
                                pad(tr!("Скачиваний", "Downloads"), 12),
                                tr!("Описание", "Description")
                            ));
                            p.items = hits
                                .iter()
                                .map(|h| {
                                    format!(
                                        "{}{}{}{}",
                                        pad(
                                            &format!(
                                                "{} {}",
                                                if h.source == ps::Source::Hangar { "H" } else { "M" },
                                                h.title
                                            ),
                                            28
                                        ),
                                        pad(&h.author, 18),
                                        pad(&human_count(h.downloads), 12),
                                        h.description
                                    )
                                })
                                .collect();
                            p.lines = vec![PanelLine::new(
                                Tone::Muted,
                                if hits.is_empty() {
                                    tr!("ничего не найдено", "nothing found").to_string()
                                } else {
                                    tr!(
                                        format!(
                                            "найдено: {}{} · M — Modrinth, H — Hangar",
                                            hits.len(),
                                            if *more { "+" } else { "" }
                                        ),
                                        format!(
                                            "found: {}{} · M — Modrinth, H — Hangar",
                                            hits.len(),
                                            if *more { "+" } else { "" }
                                        )
                                    )
                                },
                            )];
                            if !hits.is_empty() {
                                p.input_focus = false;
                            }
                        }
                        Err(e) => p.lines = vec![PanelLine::new(Tone::Bad, format!("✗ {e:#}"))],
                    }
                });
                if let Some(w) = warn {
                    app.toast(w, ToastKind::Error);
                }
            },
        );
    }

    fn pick_plugin(&mut self, p: &mut Panel) {
        let PanelKind::PluginSearch { target, hits, .. } = &p.kind else { return };
        let Some(hit) = hits.get(p.sel).cloned() else { return };
        let target = target.clone();
        let dir = self.sel().cfg.dir().to_path_buf();
        p.busy = Some(tr!(format!("подбираю версию {}…", hit.title), format!("picking a version of {}…", hit.title)));
        self.spawn_task(
            move || ps::resolve(&hit, &target, &dir).map(|plan| (hit, plan)),
            |app, res| {
                app.with_panel(|p| p.busy = None);
                match res {
                    Ok((hit, None)) => app.toast(
                        tr!(
                            format!("{}: нет версии для этого сервера", hit.title),
                            format!("{}: no version for this server", hit.title)
                        ),
                        ToastKind::Error,
                    ),
                    Ok((_, Some(plan))) => app.confirm_install(plan),
                    Err(e) => app.toast(format!("{e:#}"), ToastKind::Error),
                }
            },
        );
    }

    pub(super) fn confirm_install(&mut self, plan: InstallPlan) {
        let mut text = tr!(
            format!("Установить {} {} ({})?", plan.main.title, plan.main.version, plan.main.file_name),
            format!("Install {} {} ({})?", plan.main.title, plan.main.version, plan.main.file_name)
        );
        if !plan.deps.is_empty() {
            let deps: Vec<String> = plan.deps.iter().map(|d| format!("{} {}", d.title, d.version)).collect();
            text += &tr!(format!("\nЗависимости: {}", deps.join(", ")), format!("\nDependencies: {}", deps.join(", ")));
        }
        if plan.total_size() > 0 {
            text += &format!(" · {}", human_bytes(plan.total_size()));
        }
        if !plan.already_installed.is_empty() {
            text += &tr!(
                format!("\nУже есть: {}", plan.already_installed.join(", ")),
                format!("\nAlready installed: {}", plan.already_installed.join(", "))
            );
        }
        if !plan.unresolved.is_empty() {
            text += &tr!(
                format!("\n⚠ Поставьте вручную: {}", plan.unresolved.join("; ")),
                format!("\n⚠ Install by hand: {}", plan.unresolved.join("; "))
            );
        }
        if plan.main_installed.is_some() {
            text += tr!("\n⚠ Этот проект уже установлен.", "\n⚠ This project is already installed.");
        }
        self.popup = Some(Popup::Confirm {
            title: tr!("Установка", "Install").into(),
            text,
            action: Action::InstallPlugin(Box::new(plan)),
        });
    }

    pub(super) fn install_plugin(&mut self, plan: InstallPlan) {
        let dir = self.sel().cfg.dir().to_path_buf();
        self.plugins.busy =
            Some(tr!(format!("установка {}…", plan.main.title), format!("installing {}…", plan.main.title)));
        self.spawn_task(
            move || ps::install(&plan, &dir),
            |app, res| {
                app.plugins.busy = None;
                match res {
                    Ok(r) => {
                        let n = r.installed.len();
                        app.toast(
                            tr!(
                                format!("установлено файлов: {n} · {}", r.note),
                                format!("files installed: {n} · {}", r.note)
                            ),
                            ToastKind::Ok,
                        );
                        app.refresh_plugins(true);
                    }
                    Err(e) => app.toast(format!("{e:#}"), ToastKind::Error),
                }
            },
        );
    }

    // ───────────────────────────── поиск по логам ─────────────────────────────

    pub(super) fn run_log_search(&mut self, query: String) {
        let s = self.sel();
        let Some(logs_dir) = s.cfg.logs_dir() else { return };
        let crash_dir = s.cfg.dir().join("crash-reports");
        let mut panel = Panel::new(
            tr!(format!(" Поиск по всем логам: «{query}» "), format!(" Search all logs: “{query}” ")),
            PanelKind::LogSearch { query: query.clone(), matches: vec![] },
            tr!(
                "↑↓ выбор · Enter — открыть в логе · Esc — закрыть",
                "↑↓ select · Enter — open in the log · Esc — close"
            )
            .into(),
        );
        panel.busy = Some(tr!("ищу в логах и отчётах о падениях…", "searching logs and crash reports…").into());
        self.popup = Some(Popup::Panel(Box::new(panel)));
        self.spawn_task(
            move || {
                let files = crate::crash::search_files_in(&logs_dir, &crash_dir);
                crate::crash::search_logs_with(&files, &query, &crate::crash::SearchOpts::default())
            },
            |app, r| {
                app.with_panel(|p| {
                    p.busy = None;
                    let mut info = tr!(
                        format!("совпадений: {} · файлов просмотрено: {}", r.matches.len(), r.files_scanned),
                        format!("matches: {} · files scanned: {}", r.matches.len(), r.files_scanned)
                    );
                    if r.truncated {
                        info += tr!(" · показаны не все", " · not all shown");
                    }
                    p.lines = vec![PanelLine::new(Tone::Muted, info)];
                    for (f, e) in r.errors.iter().take(3) {
                        p.lines.push(PanelLine::new(Tone::Warn, format!("{}: {e}", f.display())));
                    }
                    p.items = r
                        .matches
                        .iter()
                        .map(|m| {
                            let name = m.file.file_name().unwrap_or_default().to_string_lossy();
                            format!("{}{}", pad(&format!("{name}:{}", m.line_no), 34), m.line)
                        })
                        .collect();
                    if let PanelKind::LogSearch { matches, .. } = &mut p.kind {
                        *matches = r.matches;
                    }
                });
            },
        );
    }

    fn open_log_match(&mut self, query: &str, m: &crate::crash::LogMatch) {
        if self.screen != Screen::Logs {
            self.toggle_logs();
        }
        let i = match self.logs.sources.iter().position(|(_, s)| matches!(s, LogSource::File(p) if *p == m.file)) {
            Some(i) => i,
            None => {
                let name = m.file.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.logs.sources.push((name, LogSource::File(m.file.clone())));
                self.logs.sources.len() - 1
            }
        };
        self.open_log_source(i);
        self.logs.filter = LevelFilter::All;
        self.logs.search = Some(query.to_string());
        let lines = self.visible_log_lines();
        let ms = self.log_matches(&lines);
        drop(lines);
        let target = m.line_no.saturating_sub(1);
        let pos = ms.iter().position(|&i| i >= target).unwrap_or(ms.len().saturating_sub(1));
        self.logs.match_cursor = Some(pos);
        self.jump_to_match(0);
    }

    // ───────────────────────────── статистика игроков ─────────────────────────────

    pub(super) fn open_player_stats(&mut self) {
        let s = self.sel();
        if !s.is_local() {
            return self.toast(
                tr!("Статистика — для локальных серверов", "Statistics are for local servers"),
                ToastKind::Info,
            );
        }
        let dir = s.cfg.dir().to_path_buf();
        let level = crate::world::current_level_name(&dir);
        let log_path = crate::playerstats::session_log_path(&self.config_path, &s.cfg.name);
        let mut panel = Panel::new(
            tr!(" Статистика игроков ", " Player statistics ").into(),
            PanelKind::Players,
            tr!("↑↓ прокрутка · Esc — закрыть", "↑↓ scroll · Esc — close").into(),
        );
        panel.busy = Some(tr!("читаю статистику мира…", "reading world statistics…").into());
        self.popup = Some(Popup::Panel(Box::new(panel)));
        self.spawn_task(
            move || {
                let stats = crate::playerstats::load(&dir, &level);
                let log = crate::playerstats::SessionLog::load(log_path);
                let now = chrono::Local::now();
                (stats, log.summary(now), log.daily(now, 14))
            },
            |app, (stats, summary, daily)| {
                app.with_panel(|p| {
                    p.busy = None;
                    p.header = Some(format!(
                        "{}{}{}{}{}{}{}",
                        pad(tr!("Игрок", "Player"), 20),
                        pad(tr!("Всего", "Total"), 11),
                        pad(tr!("Сегодня", "Today"), 10),
                        pad(tr!("7 дней", "7 days"), 10),
                        pad(tr!("Был", "Last seen"), 15),
                        pad(tr!("Смертей", "Deaths"), 9),
                        tr!("Убито мобов · км", "Mob kills · km")
                    ));
                    let find = |name: &str| summary.iter().find(|x| x.name.eq_ignore_ascii_case(name));
                    p.items = stats
                        .iter()
                        .map(|st| {
                            let ses = find(&st.name);
                            let online = ses.is_some_and(|x| x.online_since.is_some());
                            let seen = if online {
                                tr!("в игре", "online").to_string()
                            } else {
                                st.last_seen.map(ago).unwrap_or_default()
                            };
                            format!(
                                "{}{}{}{}{}{}{} · {:.1}",
                                pad(&st.name, 20),
                                pad(&short_duration(st.play_time()), 11),
                                pad(&ses.map(|x| short_duration(x.today)).unwrap_or_else(|| "—".into()), 10),
                                pad(&ses.map(|x| short_duration(x.week)).unwrap_or_else(|| "—".into()), 10),
                                pad(&seen, 15),
                                pad(&st.deaths.to_string(), 9),
                                st.mob_kills,
                                st.distance_km
                            )
                        })
                        .collect();
                    p.lines.clear();
                    if stats.is_empty() {
                        p.lines.push(PanelLine::new(
                            Tone::Muted,
                            tr!("На сервере ещё никто не играл", "Nobody has played on this server yet"),
                        ));
                    }
                    let days: Vec<String> = daily
                        .iter()
                        .rev()
                        .filter(|d| d.unique_players > 0)
                        .take(7)
                        .map(|d| format!("{} {}👤 {}", d.date.format("%d.%m"), d.peak, short_duration(d.play_time)))
                        .collect();
                    if !days.is_empty() {
                        p.lines.push(PanelLine::new(
                            Tone::Info,
                            tr!(
                                format!("Пик онлайна по дням: {}", days.join(" · ")),
                                format!("Daily peak: {}", days.join(" · "))
                            ),
                        ));
                    }
                    p.lines.push(PanelLine::new(
                        Tone::Muted,
                        tr!(
                            "«Всего» — из файлов мира; «Сегодня» и «7 дней» — по журналу mctui (с версии 0.3).",
                            "“Total” comes from the world files; “Today” and “7 days” from the mctui log (since 0.3)."
                        ),
                    ));
                });
            },
        );
    }

    // ───────────────────────────── клавиши ─────────────────────────────

    pub(super) fn panel_key(&mut self, mut p: Box<Panel>, k: KeyEvent) {
        // Строка ввода (поиск плагинов)
        if p.input_focus
            && let Some(inp) = &mut p.input
        {
            match k.code {
                KeyCode::Esc => return,
                KeyCode::Enter => {
                    self.run_plugin_search(&mut p, false);
                }
                KeyCode::Down | KeyCode::Tab if !p.items.is_empty() => p.input_focus = false,
                _ => {
                    inp.handle(&k);
                }
            }
            self.popup = Some(Popup::Panel(p));
            return;
        }
        let n = p.items.len();
        match k.code {
            KeyCode::Esc => return,
            KeyCode::Char(c) if latin(c) == 'q' => return,
            KeyCode::Up => {
                if n > 0 {
                    p.sel = p.sel.saturating_sub(1);
                } else {
                    p.scroll = p.scroll.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if n > 0 {
                    p.sel = (p.sel + 1).min(n - 1);
                } else {
                    p.scroll = (p.scroll + 1).min(p.lines.len().saturating_sub(1));
                }
            }
            KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
            KeyCode::PageDown if n > 0 => p.sel = (p.sel + 10).min(n - 1),
            KeyCode::Home => p.sel = 0,
            KeyCode::End if n > 0 => p.sel = n - 1,
            KeyCode::Tab | KeyCode::BackTab if p.input.is_some() => p.input_focus = true,
            KeyCode::Char('/') if p.input.is_some() => p.input_focus = true,
            KeyCode::Enter => match &p.kind {
                PanelKind::PluginSearch { .. } => self.pick_plugin(&mut p),
                PanelKind::CoreUpdate { .. } if p.busy.is_none() => self.pick_core_target(&mut p),
                PanelKind::Velocity { .. } if p.busy.is_none() && !p.items.is_empty() => self.link_velocity(&mut p),
                PanelKind::Java { .. } if p.busy.is_none() => self.pick_jvm(&mut p),
                PanelKind::Compat { installed, version, report, done: false } => {
                    let (inst, version, report) = ((**installed).clone(), version.clone(), (**report).clone());
                    return self.confirm_core(inst, version, true, Some(report), None);
                }
                PanelKind::LogSearch { query, matches } => {
                    if let Some(m) = matches.get(p.sel) {
                        let (q, m) = (query.clone(), m.clone());
                        self.open_log_match(&q, &m);
                        return;
                    }
                }
                _ => {}
            },
            KeyCode::Char(c) => match (latin(c), &p.kind) {
                ('p', PanelKind::Network { facts }) => {
                    let facts = (**facts).clone();
                    if !facts.running {
                        self.toast(
                            tr!(
                                "Сначала запустите сервер — у остановленного порт всегда закрыт",
                                "Start the server first — a stopped server's port is always closed"
                            ),
                            ToastKind::Info,
                        );
                    } else {
                        p.busy =
                            Some(tr!("проверяю порт из интернета…", "checking the port from the internet…").into());
                        self.popup = Some(Popup::Panel(p));
                        return self.probe_network(facts, true);
                    }
                }
                ('n', PanelKind::PluginSearch { more: true, .. }) => self.run_plugin_search(&mut p, true),
                (' ', PanelKind::Velocity { .. }) => self.toggle_velocity_backend(&mut p),
                ('u', PanelKind::Compat { done: true, .. }) if p.busy.is_none() => self.install_compat(&mut p),
                _ => {}
            },
            _ => {}
        }
        // Задача могла заменить окно (например, подтверждением установки) — не затираем его.
        if self.popup.is_none() {
            self.popup = Some(Popup::Panel(p));
        }
    }
}

/// 1234567 → «1.2M».
pub fn human_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}K", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}
