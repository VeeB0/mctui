//! Состояние приложения и обработка событий (Update в терминах TEA).

use crate::backup::{self, BackupInfo, BackupJob};
use crate::config::Kind;
use crate::event::{AppEvent, BackupEvent, RconEvent, ReqKind};
use crate::input::TextInput;
use crate::log::{Level, LogLine};
use crate::server::local::LocalProc;
use crate::server::stats::{self, PlayerEvent};
use crate::server::{PlayersSource, RconState, ServerState, Status, rcon_task};
use crate::setup::{Outcome as SetupOutcome, Setup};
use crate::ui::layout;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const POLL_EVERY: Duration = Duration::from_secs(10);
const RESPONSE_WINDOW: Duration = Duration::from_millis(1500);
const TOAST_FOR: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Main,
    Logs,
    /// Добавление сервера (первый запуск или клавиша `a`).
    Setup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Status,
    Players,
    Settings,
    Backups,
}

pub const TABS: [(Tab, &str); 4] =
    [(Tab::Status, "Статус"), (Tab::Players, "Игроки"), (Tab::Settings, "Настройки"), (Tab::Backups, "Бэкапы")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Content,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Ok,
    Error,
}

pub enum Action {
    QuitStopAll,
    ForceKill(usize),
    RemoteStop(usize),
    Command { idx: usize, cmd: String },
    DeleteBackup(PathBuf),
    EnableRcon(usize),
    RemoveServer(usize),
}

pub enum Popup {
    Confirm { title: String, text: String, action: Action },
    Edit { title: String, input: TextInput, key: String },
}

#[derive(Debug, Clone)]
pub enum LogSource {
    /// Буфер вывода в памяти (живой лог процесса или сессия RCON).
    Live,
    File(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelFilter {
    All,
    WarnUp,
    ErrorOnly,
}

impl LevelFilter {
    pub fn label(self) -> &'static str {
        match self {
            LevelFilter::All => "все",
            LevelFilter::WarnUp => "WARN+",
            LevelFilter::ErrorOnly => "ERROR",
        }
    }
    pub fn pass(self, l: Level) -> bool {
        match self {
            LevelFilter::All => true,
            LevelFilter::WarnUp => matches!(l, Level::Warn | Level::Error),
            LevelFilter::ErrorOnly => l == Level::Error,
        }
    }
}

pub struct LogsView {
    pub sources: Vec<(String, LogSource)>,
    pub source_sel: usize,
    pub active: usize,
    pub file_lines: Vec<LogLine>,
    pub file_error: Option<String>,
    /// Сколько строк от конца пролистано (0 — следим за концом).
    pub scroll: usize,
    pub hscroll: u16,
    pub filter: LevelFilter,
    pub search: Option<String>,
    pub search_input: Option<TextInput>,
    pub match_cursor: Option<usize>,
    pub focus_list: bool,
}

impl LogsView {
    fn new() -> Self {
        Self {
            sources: vec![],
            source_sel: 0,
            active: 0,
            file_lines: vec![],
            file_error: None,
            scroll: 0,
            hscroll: 0,
            filter: LevelFilter::All,
            search: None,
            search_input: None,
            match_cursor: None,
            focus_list: false,
        }
    }
}

pub struct App {
    pub servers: Vec<ServerState>,
    pub selected: usize,
    pub screen: Screen,
    pub tab: Tab,
    pub focus: Focus,
    pub input: TextInput,
    pub history: Vec<String>,
    pub hist_pos: Option<usize>,
    pub popup: Option<Popup>,
    pub toast: Option<(String, ToastKind, Instant)>,
    pub quitting: bool,
    pub should_quit: bool,
    pub players_sel: usize,
    pub settings_sel: usize,
    pub backups: Vec<BackupInfo>,
    pub backups_sel: usize,
    pub logs: LogsView,
    /// Высота области лога из последней отрисовки — для прыжков к совпадениям.
    pub logs_height: Cell<u16>,
    pub area: Rect,
    pub setup: Option<Setup>,
    pub config_path: PathBuf,
    /// Новая версия на GitHub, если есть.
    pub update_available: Option<String>,
    next_id: usize,
    rcon_seq: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
    sys: Option<sysinfo::System>,
}

/// Хоткеи работают и в русской раскладке: ЙЦУКЕН → QWERTY.
pub fn latin(c: char) -> char {
    const RU: &str = "йцукенгшщзхъфывапролджэячсмитьбю";
    const EN: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,.";
    let lower = c.to_lowercase().next().unwrap_or(c);
    match RU.chars().position(|r| r == lower) {
        Some(i) => {
            let e = EN.chars().nth(i).unwrap();
            if c.is_uppercase() { e.to_ascii_uppercase() } else { e }
        }
        None => c,
    }
}

impl App {
    pub fn new(cfg: crate::config::Config, config_path: PathBuf, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        let servers: Vec<ServerState> =
            cfg.servers.into_iter().enumerate().map(|(i, c)| ServerState::new(c, i + 1)).collect();
        let first_run = servers.is_empty();
        let mut app = Self {
            next_id: servers.len() + 1,
            rcon_seq: 0,
            setup: first_run.then(|| Setup::new(true)),
            config_path,
            update_available: None,
            servers,
            selected: 0,
            screen: if first_run { Screen::Setup } else { Screen::Main },
            tab: Tab::Status,
            focus: Focus::Sidebar,
            input: TextInput::default(),
            history: vec![],
            hist_pos: None,
            popup: None,
            toast: None,
            quitting: false,
            should_quit: false,
            players_sel: 0,
            settings_sel: 0,
            backups: vec![],
            backups_sel: 0,
            logs: LogsView::new(),
            logs_height: Cell::new(20),
            area: Rect::default(),
            tx,
            sys: sysinfo::System::new().ok(),
        };
        for i in 0..app.servers.len() {
            if app.servers[i].cfg.kind == Kind::Rcon {
                app.connect_rcon(i);
            }
        }
        app
    }

    pub fn sel(&self) -> &ServerState {
        &self.servers[self.selected]
    }

    fn sel_mut(&mut self) -> &mut ServerState {
        &mut self.servers[self.selected]
    }

    pub fn toast(&mut self, msg: impl Into<String>, kind: ToastKind) {
        self.toast = Some((msg.into(), kind, Instant::now()));
    }

    // ───────────────────────────── серверные операции ─────────────────────────────

    fn connect_rcon(&mut self, idx: usize) {
        let s = &mut self.servers[idx];
        let (host, port, password) = match s.cfg.kind {
            Kind::Rcon => match s.cfg.rcon_password() {
                Some(p) => (s.cfg.host.clone(), s.cfg.port, p),
                None => {
                    s.status = Status::Unreachable;
                    s.rcon_state = RconState::Error("не задан пароль (password / password_env)".into());
                    return;
                }
            },
            Kind::Local => {
                s.reload_props();
                if s.prop("enable-rcon") != Some("true") {
                    return;
                }
                let pass = s.prop("rcon.password").unwrap_or("").to_string();
                if pass.is_empty() {
                    s.rcon_state = RconState::Error("rcon.password пустой".into());
                    return;
                }
                let port = s.prop("rcon.port").and_then(|p| p.parse().ok()).unwrap_or(25575);
                ("127.0.0.1".to_string(), port, pass)
            }
        };
        self.rcon_seq += 1;
        s.rcon_state = RconState::Connecting;
        if s.cfg.kind == Kind::Rcon {
            s.status = Status::Connecting;
        }
        s.rcon = Some(rcon_task::spawn(s.id, self.rcon_seq, host, port, password, self.tx.clone()));
    }

    pub fn start(&mut self, idx: usize) {
        let s = &mut self.servers[idx];
        if s.cfg.kind == Kind::Rcon {
            self.connect_rcon(idx);
            self.toast("переподключение RCON…", ToastKind::Info);
            return;
        }
        if s.proc.is_some() {
            let msg = format!("{} уже запущен", s.cfg.name);
            self.toast(msg, ToastKind::Info);
            return;
        }
        match LocalProc::spawn(s.id, &s.cfg, self.tx.clone()) {
            Ok(p) => {
                s.log.push(&format!("──────── mctui: запуск {} (pid {}) ────────", s.cfg.command.join(" "), p.pid));
                s.proc = Some(p);
                s.status = Status::Starting;
                s.started_at = Some(Instant::now());
                s.user_stop = false;
                s.restart_after_stop = false;
                s.restart_at = None;
                s.needs_restart = false;
                s.players.clear();
                s.reload_props();
                let name = s.cfg.name.clone();
                self.toast(format!("{name}: запуск"), ToastKind::Info);
            }
            Err(e) => {
                s.status = Status::Crashed;
                let msg = format!("{e:#}");
                s.log.push(&format!("mctui: {msg}"));
                self.toast(msg, ToastKind::Error);
            }
        }
    }

    pub fn stop(&mut self, idx: usize, restart: bool) {
        let s = &mut self.servers[idx];
        if s.cfg.kind == Kind::Rcon {
            if restart {
                self.toast("перезапуск удалённого сервера по RCON не поддерживается", ToastKind::Error);
            } else {
                self.popup = Some(Popup::Confirm {
                    title: "Остановка".into(),
                    text: format!("Отправить stop на удалённый сервер «{}»?", s.cfg.name),
                    action: Action::RemoteStop(idx),
                });
            }
            return;
        }
        if s.status == Status::Restarting {
            s.restart_at = None;
            s.status = Status::Offline;
            self.toast("авто-рестарт отменён", ToastKind::Info);
            return;
        }
        let Some(p) = &s.proc else {
            if restart {
                self.start(idx);
            } else {
                self.toast("сервер не запущен", ToastKind::Info);
            }
            return;
        };
        if s.status == Status::Stopping {
            self.popup = Some(Popup::Confirm {
                title: "Принудительно".into(),
                text: format!(
                    "«{}» уже останавливается. Убить процесс (kill -9)? Несохранённые данные пропадут.",
                    s.cfg.name
                ),
                action: Action::ForceKill(idx),
            });
            return;
        }
        p.send("stop");
        s.user_stop = true;
        s.restart_after_stop = restart;
        s.status = Status::Stopping;
        s.stop_deadline = Some(Instant::now() + Duration::from_secs(s.cfg.stop_timeout_secs));
        let name = s.cfg.name.clone();
        self.toast(format!("{name}: {}", if restart { "перезапуск" } else { "остановка" }), ToastKind::Info);
    }

    pub fn backup(&mut self, idx: usize) {
        let s = &mut self.servers[idx];
        if s.cfg.kind == Kind::Rcon {
            self.toast("бэкап доступен только для локальных серверов", ToastKind::Error);
            return;
        }
        if s.backup_progress.is_some() {
            self.toast("бэкап уже выполняется", ToastKind::Info);
            return;
        }
        if s.status == Status::Starting || s.status == Status::Stopping {
            self.toast("дождитесь окончания запуска/остановки", ToastKind::Info);
            return;
        }
        let job = BackupJob {
            id: s.id,
            name: s.cfg.name.clone(),
            root: s.cfg.dir().to_path_buf(),
            paths: s.cfg.backup_paths.clone(),
            backup_dir: s.cfg.backup_dir(),
            keep: s.cfg.backup_keep,
            live: s.proc.as_ref().map(|p| (p.stdin(), p.lines.subscribe())),
        };
        s.backup_progress = Some("подготовка…".into());
        tokio::spawn(backup::run(job, self.tx.clone()));
    }

    /// Отправка команды серверу. Локальный — через stdin, RCON — через подключение.
    fn send_command(&mut self, idx: usize, cmd: &str, kind: ReqKind) -> Result<(), String> {
        let cmd = cmd.trim().trim_start_matches('/').to_string();
        let s = &mut self.servers[idx];
        match s.cfg.kind {
            Kind::Local => {
                let Some(p) = &s.proc else {
                    return Err("сервер не запущен".into());
                };
                if !p.send(&cmd) {
                    return Err("stdin процесса закрыт".into());
                }
                if cmd == "stop" || cmd == "end" {
                    s.user_stop = true;
                    s.status = Status::Stopping;
                    s.stop_deadline = Some(Instant::now() + Duration::from_secs(s.cfg.stop_timeout_secs));
                }
                if kind == ReqKind::User {
                    s.response = vec![format!("> {cmd}")];
                    s.response_until = Some(Instant::now() + RESPONSE_WINDOW);
                }
                Ok(())
            }
            Kind::Rcon => {
                let Some(r) = &s.rcon else {
                    return Err("нет подключения RCON".into());
                };
                if kind == ReqKind::User {
                    s.response = vec![format!("> {cmd}"), "…".into()];
                }
                if r.send(kind, &cmd) { Ok(()) } else { Err("задача RCON завершилась".into()) }
            }
        }
    }

    fn submit_input(&mut self) {
        let cmd = self.input.take();
        if cmd.trim().is_empty() {
            return;
        }
        if self.history.last() != Some(&cmd) {
            self.history.push(cmd.clone());
            if self.history.len() > 500 {
                self.history.remove(0);
            }
        }
        self.hist_pos = None;
        if let Err(e) = self.send_command(self.selected, &cmd, ReqKind::User) {
            self.sel_mut().response = vec![format!("> {}", cmd.trim()), format!("ошибка: {e}")];
        }
    }

    fn request_quit(&mut self) {
        let running = self.servers.iter().filter(|s| s.proc.is_some()).count();
        if self.quitting {
            for s in &mut self.servers {
                if let Some(p) = &mut s.proc {
                    p.kill();
                }
            }
            self.should_quit = true;
        } else if running == 0 {
            self.should_quit = true;
        } else {
            self.popup = Some(Popup::Confirm {
                title: "Выход".into(),
                text: format!(
                    "Запущено локальных серверов: {running}. Они будут штатно остановлены (stop), затем mctui закроется."
                ),
                action: Action::QuitStopAll,
            });
        }
    }

    pub fn quit_stop_all(&mut self) {
        self.quitting = true;
        for i in 0..self.servers.len() {
            let s = &mut self.servers[i];
            s.restart_at = None;
            if s.proc.is_some() && s.status != Status::Stopping {
                self.stop(i, false);
            }
        }
        self.toast("остановка серверов… Ctrl+Q ещё раз — убить принудительно", ToastKind::Info);
        self.check_quit();
    }

    fn check_quit(&mut self) {
        if self.quitting && self.servers.iter().all(|s| s.proc.is_none()) {
            self.should_quit = true;
        }
    }

    fn run_action(&mut self, a: Action) {
        match a {
            Action::QuitStopAll => self.quit_stop_all(),
            Action::ForceKill(i) => {
                if let Some(p) = &mut self.servers[i].proc {
                    p.kill();
                }
            }
            Action::RemoteStop(i) => {
                if let Err(e) = self.send_command(i, "stop", ReqKind::Action) {
                    self.toast(e, ToastKind::Error);
                }
            }
            Action::Command { idx, cmd } => match self.send_command(idx, &cmd, ReqKind::Action) {
                Ok(()) => self.toast(format!("отправлено: {cmd}"), ToastKind::Ok),
                Err(e) => self.toast(e, ToastKind::Error),
            },
            Action::DeleteBackup(p) => match std::fs::remove_file(&p) {
                Ok(()) => {
                    self.toast("бэкап удалён", ToastKind::Ok);
                    self.refresh_backups();
                }
                Err(e) => self.toast(format!("не удалось удалить: {e}"), ToastKind::Error),
            },
            Action::EnableRcon(i) => self.enable_rcon(i),
            Action::RemoveServer(i) => self.remove_server(i),
        }
    }

    fn enable_rcon(&mut self, idx: usize) {
        let s = &mut self.servers[idx];
        let Some(path) = s.cfg.properties_path() else {
            return;
        };
        let Some(props) = &mut s.props else {
            self.toast("server.properties не загружен", ToastKind::Error);
            return;
        };
        props.set("enable-rcon", "true");
        if props.get("rcon.password").unwrap_or("").is_empty() {
            props.set("rcon.password", &random_password());
        }
        if props.get("rcon.port").unwrap_or("").is_empty() {
            props.set("rcon.port", "25575");
        }
        // Не дублировать опросы list/tps операторам в чат.
        props.set("broadcast-rcon-to-ops", "false");
        match props.save(&path) {
            Ok(()) => {
                if s.proc.is_some() {
                    s.needs_restart = true;
                }
                s.reload_props();
                self.toast(
                    "RCON включён. Перезапустите сервер (Ctrl+R), чтобы появились TPS и список игроков",
                    ToastKind::Ok,
                );
            }
            Err(e) => self.toast(format!("ошибка записи: {e}"), ToastKind::Error),
        }
    }

    fn set_property(&mut self, key: &str, value: &str) {
        let s = self.sel_mut();
        let Some(path) = s.cfg.properties_path() else {
            return;
        };
        let Some(props) = &mut s.props else { return };
        props.set(key, value);
        let res = props.save(&path);
        let running = s.proc.is_some();
        s.reload_props();
        match res {
            Ok(()) => {
                if running {
                    self.sel_mut().needs_restart = true;
                    self.toast(format!("{key} = {value} — сохранено, нужен перезапуск"), ToastKind::Ok);
                } else {
                    self.toast(format!("{key} = {value} — сохранено"), ToastKind::Ok);
                }
            }
            Err(e) => self.toast(format!("ошибка записи: {e}"), ToastKind::Error),
        }
    }

    pub fn refresh_backups(&mut self) {
        let s = self.sel();
        self.backups =
            if s.is_local() { backup::list(&s.cfg.backup_dir(), &backup::file_prefix(&s.cfg.name)) } else { vec![] };
        self.backups_sel = self.backups_sel.min(self.backups.len().saturating_sub(1));
    }

    fn on_select_changed(&mut self) {
        self.players_sel = 0;
        self.settings_sel = 0;
        self.backups_sel = 0;
        self.sel_mut().reload_props();
        self.refresh_backups();
    }

    fn set_tab(&mut self, t: Tab) {
        self.tab = t;
        match t {
            Tab::Settings => self.sel_mut().reload_props(),
            Tab::Backups => self.refresh_backups(),
            _ => {}
        }
    }

    // ───────────────────────────── фоновые события ─────────────────────────────

    pub fn on_event(&mut self, ev: AppEvent) {
        let pos = |app: &Self, id: usize| app.servers.iter().position(|s| s.id == id);
        match ev {
            AppEvent::Line { id, line } => {
                if let Some(i) = pos(self, id) {
                    self.on_line(i, &line)
                }
            }
            AppEvent::Exited { id, pid, status } => {
                if let Some(i) = pos(self, id) {
                    self.on_exit(i, pid, status)
                }
            }
            AppEvent::Rcon { id, generation, ev } => {
                if let Some(i) = pos(self, id) {
                    self.on_rcon(i, generation, ev)
                }
            }
            AppEvent::Backup { id, ev } => {
                if let Some(i) = pos(self, id) {
                    self.on_backup(i, ev)
                }
            }
            AppEvent::UpdateAvailable(v) => {
                self.toast(format!("Доступна версия {v} — выполните «mctui update»"), ToastKind::Info);
                self.update_available = Some(v);
            }
        }
    }

    // ───────────────────────────── список серверов ─────────────────────────────

    fn save_config(&mut self) {
        let cfg = crate::config::Config { servers: self.servers.iter().map(|s| s.cfg.clone()).collect() };
        if let Err(e) = cfg.save(&self.config_path) {
            self.toast(format!("не удалось сохранить {}: {e:#}", self.config_path.display()), ToastKind::Error);
        }
    }

    pub fn open_setup(&mut self) {
        self.setup = Some(Setup::new(self.servers.is_empty()));
        self.screen = Screen::Setup;
    }

    fn add_server(&mut self, mut cfg: crate::config::ServerConfig) {
        let base = cfg.name.clone();
        let mut n = 2;
        while self.servers.iter().any(|s| s.cfg.name == cfg.name) {
            cfg.name = format!("{base} ({n})");
            n += 1;
        }
        let local_dir = cfg.dir.clone().filter(|_| cfg.kind == Kind::Local);
        if let Some(d) = local_dir
            && let Some(other) =
                self.servers.iter().find(|s| s.cfg.kind == Kind::Local && s.cfg.dir.as_ref() == Some(&d))
        {
            let name = other.cfg.name.clone();
            if let Some(s) = &mut self.setup {
                s.error = Some(format!("Эта папка уже добавлена как «{name}»"));
            }
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let name = cfg.name.clone();
        let is_rcon = cfg.kind == Kind::Rcon;
        self.servers.push(ServerState::new(cfg, id));
        self.selected = self.servers.len() - 1;
        if is_rcon {
            self.connect_rcon(self.selected);
        }
        self.save_config();
        self.setup = None;
        self.screen = Screen::Main;
        self.focus = Focus::Sidebar;
        self.on_select_changed();
        let hint = if is_rcon { "" } else { " — Ctrl+S, чтобы запустить" };
        self.toast(format!("Сервер «{name}» добавлен{hint}"), ToastKind::Ok);
    }

    fn ask_remove_server(&mut self) {
        let s = self.sel();
        if s.proc.is_some() || s.backup_progress.is_some() {
            self.toast("сначала остановите сервер и дождитесь окончания бэкапа", ToastKind::Error);
            return;
        }
        self.popup = Some(Popup::Confirm {
            title: "Убрать из списка".into(),
            text: format!("Убрать «{}» из mctui? Файлы сервера и бэкапы останутся на диске.", s.cfg.name),
            action: Action::RemoveServer(self.selected),
        });
    }

    fn remove_server(&mut self, idx: usize) {
        if idx >= self.servers.len() || self.servers[idx].proc.is_some() {
            return;
        }
        let s = self.servers.remove(idx);
        self.save_config();
        self.toast(format!("«{}» убран из списка", s.cfg.name), ToastKind::Info);
        if self.servers.is_empty() {
            self.selected = 0;
            self.open_setup();
        } else {
            self.selected = self.selected.min(self.servers.len() - 1);
            self.on_select_changed();
        }
    }

    fn setup_key(&mut self, k: KeyEvent) {
        let Some(setup) = &mut self.setup else { return };
        match setup.handle_key(k) {
            SetupOutcome::None => {}
            SetupOutcome::Cancel => {
                if self.servers.is_empty() {
                    self.should_quit = true;
                } else {
                    self.setup = None;
                    self.screen = Screen::Main;
                }
            }
            SetupOutcome::Save(cfg) => self.add_server(*cfg),
        }
    }

    fn on_line(&mut self, idx: usize, raw: &str) {
        let following = self.screen == Screen::Logs
            && idx == self.selected
            && matches!(self.logs.sources.get(self.logs.active), Some((_, LogSource::Live)));
        let s = &mut self.servers[idx];
        let line = s.log.push(raw).clone();
        if following && self.logs.scroll > 0 && self.logs.filter.pass(line.level) {
            self.logs.scroll += 1; // держим вид на месте, пока пользователь читает старое
        }
        let text = &line.text;
        if s.status == Status::Starting && stats::is_done(text) {
            s.status = Status::Running;
            let name = s.cfg.name.clone();
            self.connect_rcon(idx);
            self.toast(format!("{name} запущен"), ToastKind::Ok);
        }
        let s = &mut self.servers[idx];
        if stats::is_stopping(text) && matches!(s.status, Status::Running | Status::Starting) {
            s.status = Status::Stopping;
            s.stop_deadline.get_or_insert(Instant::now() + Duration::from_secs(s.cfg.stop_timeout_secs));
        }
        if let Some(v) = stats::version(text) {
            s.version = Some(v);
        }
        match stats::player_event(text) {
            Some(PlayerEvent::Joined(n)) => {
                s.players.insert(n);
            }
            Some(PlayerEvent::Left(n)) => {
                s.players.remove(&n);
            }
            None => {}
        }
        if let Some(until) = s.response_until {
            if Instant::now() < until {
                s.response.push(text.clone());
                if s.response.len() > 50 {
                    s.response.remove(1);
                }
            } else {
                s.response_until = None;
            }
        }
    }

    fn on_exit(&mut self, idx: usize, pid: u32, status: String) {
        let s = &mut self.servers[idx];
        if s.proc.as_ref().map(|p| p.pid) != Some(pid) {
            return;
        }
        s.clear_runtime();
        s.last_exit = Some(format!("{} — {status}", chrono::Local::now().format("%H:%M:%S")));
        s.log.push(&format!("──────── mctui: процесс завершился ({status}) ────────"));
        let name = s.cfg.name.clone();
        if s.user_stop {
            s.status = Status::Offline;
            if s.restart_after_stop && !self.quitting {
                self.start(idx);
            } else {
                self.toast(format!("{name} остановлен"), ToastKind::Info);
            }
        } else {
            s.status = Status::Crashed;
            if s.cfg.auto_restart && !self.quitting {
                match s.crash.on_crash(Instant::now()) {
                    crate::server::supervisor::Decision::Restart => {
                        let d = s.cfg.restart_delay_secs;
                        s.status = Status::Restarting;
                        s.restart_at = Some(Instant::now() + Duration::from_secs(d));
                        self.toast(format!("{name} упал ({status}), перезапуск через {d} с"), ToastKind::Error);
                    }
                    crate::server::supervisor::Decision::GiveUp => {
                        s.log.push("mctui: 3 падения за 5 минут — авто-рестарт приостановлен");
                        self.toast(format!("{name} падает слишком часто — авто-рестарт остановлен"), ToastKind::Error);
                    }
                }
            } else {
                self.toast(format!("{name} завершился: {status}"), ToastKind::Error);
            }
        }
        self.check_quit();
    }

    fn on_rcon(&mut self, idx: usize, generation: u64, ev: RconEvent) {
        let s = &mut self.servers[idx];
        if s.rcon.as_ref().map(|r| r.generation) != Some(generation) {
            return;
        }
        let remote = s.cfg.kind == Kind::Rcon;
        match ev {
            RconEvent::Connected => {
                s.rcon_state = RconState::Connected;
                if remote {
                    s.status = Status::Online;
                    s.log.push(&format!("── RCON подключён к {}:{} ──", s.cfg.host, s.cfg.port));
                }
                s.last_poll = None; // опросить сразу
            }
            RconEvent::Disconnected(e) => {
                if remote && s.rcon_state != RconState::Error(e.clone()) {
                    s.log.push(&format!("── RCON: {e} ──"));
                }
                s.rcon_state = RconState::Error(e);
                if remote {
                    s.status = Status::Unreachable;
                    s.players.clear();
                    s.tps = None;
                }
            }
            RconEvent::Response { kind, cmd, result } => match kind {
                ReqKind::User => {
                    let mut out = vec![format!("> {cmd}")];
                    match &result {
                        Ok(body) if body.trim().is_empty() => out.push("(пустой ответ)".into()),
                        Ok(body) => out.extend(body.lines().map(crate::log::strip_codes)),
                        Err(e) => out.push(format!("ошибка: {e}")),
                    }
                    for l in &out {
                        s.log.push(l);
                    }
                    s.response = out;
                }
                ReqKind::Action => {
                    let msg = match result {
                        Ok(b) if !b.trim().is_empty() => crate::log::strip_codes(b.lines().next().unwrap_or("")),
                        Ok(_) => format!("{cmd}: ок"),
                        Err(e) => format!("{cmd}: {e}"),
                    };
                    s.log.push(&format!("> {cmd}: {msg}"));
                    self.toast(msg, ToastKind::Info);
                }
                ReqKind::PollList => {
                    if let Some(info) = result.ok().as_deref().and_then(stats::parse_list) {
                        s.players = info.players.into_iter().collect();
                        s.max_players = Some(info.max);
                        s.players_source = PlayersSource::Rcon;
                    }
                }
                ReqKind::PollTps => {
                    if let Some(t) = result.ok().as_deref().and_then(stats::parse_tps) {
                        s.tps = Some(t);
                    }
                }
            },
        }
    }

    fn on_backup(&mut self, idx: usize, ev: BackupEvent) {
        let s = &mut self.servers[idx];
        match ev {
            BackupEvent::Progress(p) => s.backup_progress = Some(p),
            BackupEvent::Done { path, bytes, warning } => {
                s.backup_progress = None;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                s.last_backup =
                    Some(format!("{} — {name} ({})", chrono::Local::now().format("%H:%M:%S"), human_bytes(bytes)));
                match warning {
                    Some(w) => self.toast(format!("бэкап готов, но: {w}"), ToastKind::Error),
                    None => self.toast(format!("бэкап готов: {name} ({})", human_bytes(bytes)), ToastKind::Ok),
                }
                if idx == self.selected {
                    self.refresh_backups();
                }
            }
            BackupEvent::Failed(e) => {
                s.backup_progress = None;
                self.toast(format!("бэкап не удался: {e}"), ToastKind::Error);
            }
        }
    }

    /// Раз в секунду: таймауты остановки, отложенные рестарты, ресурсы, опрос RCON.
    pub fn on_tick(&mut self) {
        let now = Instant::now();
        if self.toast.as_ref().is_some_and(|t| now.duration_since(t.2) > TOAST_FOR) {
            self.toast = None;
        }
        for i in 0..self.servers.len() {
            let s = &mut self.servers[i];
            if let (Some(d), Some(p)) = (s.stop_deadline, s.proc.as_mut())
                && now >= d
            {
                s.log.push("mctui: сервер не остановился за отведённое время — kill");
                p.kill();
                s.stop_deadline = None;
            }
            if s.restart_at.is_some_and(|t| now >= t) {
                s.restart_at = None;
                self.start(i);
            }
            let s = &mut self.servers[i];
            if s.rcon_state == RconState::Connected && s.last_poll.is_none_or(|t| now.duration_since(t) >= POLL_EVERY) {
                s.last_poll = Some(now);
                if let Some(r) = &s.rcon {
                    r.send(ReqKind::PollList, "list");
                    r.send(ReqKind::PollTps, "tps");
                }
            }
        }
        self.refresh_resources();
    }

    /// CPU/RAM считаются по всему дереву процессов: скрипт запуска (sh, cmd) + java.
    fn refresh_resources(&mut self) {
        let Some(sys) = &mut self.sys else { return };
        if self.servers.iter().all(|s| s.proc.is_none()) {
            return;
        }
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let mut children: std::collections::HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> = Default::default();
        for (pid, p) in sys.processes() {
            if let Some(parent) = p.parent() {
                children.entry(parent).or_default().push(*pid);
            }
        }
        for s in &mut self.servers {
            let Some(p) = &s.proc else { continue };
            let root = sysinfo::Pid::from_u32(p.pid);
            if sys.process(root).is_none() {
                continue;
            }
            let (mut cpu, mut mem) = (0f32, 0u64);
            let mut stack = vec![root];
            while let Some(pid) = stack.pop() {
                if let Some(pr) = sys.process(pid) {
                    cpu += pr.cpu_usage();
                    mem += pr.memory();
                }
                if let Some(c) = children.get(&pid) {
                    stack.extend(c);
                }
            }
            s.cpu = Some(cpu);
            s.mem = Some(mem);
            s.cpu_hist.push_back(cpu.round() as u64);
            if s.cpu_hist.len() > 120 {
                s.cpu_hist.pop_front();
            }
        }
    }

    // ───────────────────────────── клавиатура ─────────────────────────────

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if let Some(p) = self.popup.take() {
            self.popup_key(p, k);
            return;
        }
        if self.screen == Screen::Setup {
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl && matches!(k.code, KeyCode::Char(c) if matches!(latin(c), 'q' | 'c')) {
                return self.request_quit();
            }
            return self.setup_key(k);
        }
        if k.modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = k.code
        {
            let i = self.selected;
            match latin(c) {
                'q' | 'c' => return self.request_quit(),
                's' => return self.start(i),
                'x' => return self.stop(i, false),
                'r' => return self.stop(i, true),
                'b' => return self.backup(i),
                'l' => return self.toggle_logs(),
                _ => {}
            }
        }
        match self.screen {
            Screen::Main => self.main_key(k),
            Screen::Logs => self.logs_key(k),
            Screen::Setup => {}
        }
    }

    fn popup_key(&mut self, p: Popup, k: KeyEvent) {
        match p {
            Popup::Confirm { title, text, action } => match k.code {
                KeyCode::Enter => self.run_action(action),
                KeyCode::Char(c) if matches!(latin(c), 'y' | 'Y') || c == 'д' || c == 'Д' => self.run_action(action),
                KeyCode::Esc => {}
                KeyCode::Char(c) if matches!(latin(c), 'n' | 'N') => {}
                _ => self.popup = Some(Popup::Confirm { title, text, action }),
            },
            Popup::Edit { title, mut input, key } => match k.code {
                KeyCode::Enter => self.set_property(&key, input.value.trim()),
                KeyCode::Esc => {}
                _ => {
                    input.handle(&k);
                    self.popup = Some(Popup::Edit { title, input, key });
                }
            },
        }
    }

    fn main_key(&mut self, k: KeyEvent) {
        if self.focus == Focus::Input {
            match k.code {
                KeyCode::Enter => self.submit_input(),
                KeyCode::Esc => self.focus = Focus::Sidebar,
                KeyCode::Tab => self.focus = Focus::Sidebar,
                KeyCode::BackTab => self.focus = Focus::Content,
                KeyCode::Up => self.history_step(-1),
                KeyCode::Down => self.history_step(1),
                _ => {
                    self.input.handle(&k);
                }
            }
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
                    '1'..='4' => return self.set_tab(TABS[c as usize - '1' as usize].0),
                    ':' | 'i' | '/' => {
                        self.focus = Focus::Input;
                        return;
                    }
                    'q' => return self.request_quit(),
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

    fn cycle_tab(&mut self, d: isize) {
        let i = TABS.iter().position(|t| t.0 == self.tab).unwrap_or(0) as isize;
        let n = TABS.len() as isize;
        self.set_tab(TABS[((i + d).rem_euclid(n)) as usize].0);
    }

    fn move_selection(&mut self, delta: isize) {
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
                Tab::Settings => {
                    let n = self.sel().props.as_ref().map(|p| p.pairs().len()).unwrap_or(0);
                    self.settings_sel = step(self.settings_sel, n)
                }
                Tab::Backups => self.backups_sel = step(self.backups_sel, self.backups.len()),
                Tab::Status => {}
            },
            Focus::Input => {}
        }
    }

    fn history_step(&mut self, d: isize) {
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

    fn selected_player(&self) -> Option<String> {
        self.sel().players.iter().nth(self.players_sel).cloned()
    }

    fn content_char(&mut self, c: char) {
        let idx = self.selected;
        match self.tab {
            Tab::Players => {
                let Some(p) = self.selected_player() else {
                    return;
                };
                let (cmd, what) = match c {
                    'k' => (format!("kick {p}"), "Кикнуть"),
                    'b' => (format!("ban {p}"), "Забанить"),
                    'o' => (format!("op {p}"), "Выдать оператора"),
                    'd' => (format!("deop {p}"), "Снять оператора"),
                    _ => return,
                };
                self.popup = Some(Popup::Confirm {
                    title: what.into(),
                    text: format!("{what} {p}? Будет выполнено: /{cmd}"),
                    action: Action::Command { idx, cmd },
                });
            }
            Tab::Settings => match c {
                ' ' => {
                    if let Some((k, v)) = self.selected_property() {
                        match v.as_str() {
                            "true" => self.set_property(&k, "false"),
                            "false" => self.set_property(&k, "true"),
                            _ => self
                                .toast("пробел переключает только true/false; Enter — редактировать", ToastKind::Info),
                        }
                    }
                }
                'r' => {
                    if self.sel().is_local() {
                        self.popup = Some(Popup::Confirm {
                            title: "RCON".into(),
                            text: "Включить RCON на 127.0.0.1 (enable-rcon=true, случайный пароль)? Нужен для TPS и точного списка игроков.".into(),
                            action: Action::EnableRcon(idx),
                        });
                    }
                }
                'u' => {
                    self.sel_mut().reload_props();
                    self.toast("перечитано с диска", ToastKind::Info);
                }
                _ => {}
            },
            Tab::Backups => match c {
                'n' => self.backup(idx),
                'd' => {
                    if let Some(b) = self.backups.get(self.backups_sel) {
                        let name = b.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        self.popup = Some(Popup::Confirm {
                            title: "Удалить бэкап".into(),
                            text: format!("Удалить {name} безвозвратно?"),
                            action: Action::DeleteBackup(b.path.clone()),
                        });
                    }
                }
                'u' => self.refresh_backups(),
                _ => {}
            },
            Tab::Status => {}
        }
    }

    fn selected_property(&self) -> Option<(String, String)> {
        self.sel().props.as_ref()?.pairs().into_iter().nth(self.settings_sel)
    }

    fn content_enter(&mut self) {
        if self.tab == Tab::Settings
            && let Some((k, v)) = self.selected_property()
        {
            self.popup = Some(Popup::Edit { title: k.clone(), input: TextInput::with(&v), key: k });
        }
    }

    // ───────────────────────────── экран логов ─────────────────────────────

    pub fn toggle_logs(&mut self) {
        if self.screen == Screen::Logs {
            self.screen = Screen::Main;
            return;
        }
        self.screen = Screen::Logs;
        self.build_log_sources();
        let s = self.sel();
        // По умолчанию: живой вывод, если сервер работает (или это RCON), иначе latest.log.
        let prefer_live = s.proc.is_some()
            || s.cfg.kind == Kind::Rcon && s.cfg.log_path.is_none()
            || s.log.len() > 0 && s.status == Status::Crashed;
        let pick = if prefer_live {
            self.logs.sources.iter().position(|(_, src)| matches!(src, LogSource::Live))
        } else {
            self.logs.sources.iter().position(|(_, src)| matches!(src, LogSource::File(_)))
        };
        self.open_log_source(pick.unwrap_or(0));
    }

    fn build_log_sources(&mut self) {
        let s = self.sel();
        let mut v: Vec<(String, LogSource)> = vec![];
        match s.cfg.kind {
            Kind::Local => {
                let live = if s.proc.is_some() {
                    "● Живой вывод"
                } else {
                    "Вывод последнего запуска"
                };
                v.push((live.into(), LogSource::Live));
                if let Some(dir) = s.cfg.logs_dir() {
                    let latest = dir.join("latest.log");
                    if latest.is_file() {
                        v.push(("latest.log".into(), LogSource::File(latest)));
                    }
                    let mut archives: Vec<PathBuf> = std::fs::read_dir(&dir)
                        .map(|rd| {
                            rd.flatten()
                                .map(|e| e.path())
                                .filter(|p| p.to_string_lossy().ends_with(".log.gz"))
                                .collect()
                        })
                        .unwrap_or_default();
                    archives.sort();
                    archives.reverse();
                    for a in archives.into_iter().take(100) {
                        let name = a.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        v.push((name, LogSource::File(a)));
                    }
                }
            }
            Kind::Rcon => {
                v.push(("Сессия RCON".into(), LogSource::Live));
                if let Some(p) = &s.cfg.log_path {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    v.push((name, LogSource::File(p.clone())));
                }
            }
        }
        self.logs.sources = v;
    }

    fn open_log_source(&mut self, i: usize) {
        let Some((_, src)) = self.logs.sources.get(i).cloned() else {
            return;
        };
        self.logs.active = i;
        self.logs.source_sel = i;
        self.logs.scroll = 0;
        self.logs.hscroll = 0;
        self.logs.match_cursor = None;
        self.logs.file_lines.clear();
        self.logs.file_error = None;
        if let LogSource::File(p) = src {
            match read_log_file(&p) {
                Ok(lines) => self.logs.file_lines = lines,
                Err(e) => self.logs.file_error = Some(format!("{}: {e}", p.display())),
            }
        }
    }

    /// Строки активного источника после фильтра уровня.
    pub fn visible_log_lines(&self) -> Vec<&LogLine> {
        let f = self.logs.filter;
        match self.logs.sources.get(self.logs.active) {
            Some((_, LogSource::Live)) => self.sel().log.iter().filter(|l| f.pass(l.level)).collect(),
            Some((_, LogSource::File(_))) => self.logs.file_lines.iter().filter(|l| f.pass(l.level)).collect(),
            None => vec![],
        }
    }

    pub fn log_matches(&self, lines: &[&LogLine]) -> Vec<usize> {
        let Some(q) = self.logs.search.as_ref().filter(|q| !q.is_empty()) else {
            return vec![];
        };
        let q = q.to_lowercase();
        lines.iter().enumerate().filter(|(_, l)| l.text.to_lowercase().contains(&q)).map(|(i, _)| i).collect()
    }

    fn jump_to_match(&mut self, d: isize) {
        let lines = self.visible_log_lines();
        let matches = self.log_matches(&lines);
        let total = lines.len();
        drop(lines);
        if matches.is_empty() {
            self.logs.match_cursor = None;
            if self.logs.search.is_some() {
                self.toast("совпадений нет", ToastKind::Info);
            }
            return;
        }
        let cur = match self.logs.match_cursor {
            None => matches.len() - 1,
            Some(c) => (c as isize + d).clamp(0, matches.len() as isize - 1) as usize,
        };
        self.logs.match_cursor = Some(cur);
        let h = self.logs_height.get().max(1) as usize;
        let line = matches[cur];
        // Совпадение — примерно в середине экрана.
        let end = (line + h / 2 + 1).min(total);
        self.logs.scroll = total - end.max(h.min(total));
    }

    fn scroll_logs(&mut self, d: isize) {
        let total = self.visible_log_lines().len();
        let h = self.logs_height.get() as usize;
        let max = total.saturating_sub(h);
        self.logs.scroll = (self.logs.scroll as isize + d).clamp(0, max as isize) as usize;
    }

    fn logs_key(&mut self, k: KeyEvent) {
        if let Some(mut inp) = self.logs.search_input.take() {
            match k.code {
                KeyCode::Enter => {
                    let q = inp.value.trim().to_string();
                    self.logs.search = (!q.is_empty()).then_some(q);
                    self.logs.match_cursor = None;
                    self.jump_to_match(0);
                }
                KeyCode::Esc => {}
                _ => {
                    inp.handle(&k);
                    self.logs.search_input = Some(inp);
                }
            }
            return;
        }
        let h = self.logs_height.get().max(1) as isize;
        match k.code {
            KeyCode::Esc => {
                if self.logs.search.is_some() {
                    self.logs.search = None;
                    self.logs.match_cursor = None;
                } else {
                    self.screen = Screen::Main;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => self.logs.focus_list = !self.logs.focus_list,
            KeyCode::Up if self.logs.focus_list => self.logs.source_sel = self.logs.source_sel.saturating_sub(1),
            KeyCode::Down if self.logs.focus_list => {
                self.logs.source_sel = (self.logs.source_sel + 1).min(self.logs.sources.len().saturating_sub(1))
            }
            KeyCode::Enter if self.logs.focus_list => {
                self.open_log_source(self.logs.source_sel);
                self.logs.focus_list = false;
            }
            KeyCode::Up => self.scroll_logs(1),
            KeyCode::Down => self.scroll_logs(-1),
            KeyCode::PageUp => self.scroll_logs(h - 1),
            KeyCode::PageDown => self.scroll_logs(-(h - 1)),
            KeyCode::Home => self.scroll_logs(isize::MAX / 2),
            KeyCode::End => self.logs.scroll = 0,
            KeyCode::Left => self.logs.hscroll = self.logs.hscroll.saturating_sub(10),
            KeyCode::Right => self.logs.hscroll = self.logs.hscroll.saturating_add(10),
            KeyCode::Char(c) => match latin(c) {
                'l' | 'L' | 'q' => self.screen = Screen::Main,
                '/' | '.' => self.logs.search_input = Some(TextInput::with(self.logs.search.as_deref().unwrap_or(""))),
                'n' => self.jump_to_match(-1),
                'N' => self.jump_to_match(1),
                'f' => {
                    self.logs.filter = match self.logs.filter {
                        LevelFilter::All => LevelFilter::WarnUp,
                        LevelFilter::WarnUp => LevelFilter::ErrorOnly,
                        LevelFilter::ErrorOnly => LevelFilter::All,
                    };
                    self.logs.scroll = 0;
                    self.logs.match_cursor = None;
                }
                'r' => {
                    self.build_log_sources();
                    let a = self.logs.active.min(self.logs.sources.len().saturating_sub(1));
                    self.open_log_source(a);
                }
                'g' => self.scroll_logs(isize::MAX / 2),
                'G' => self.logs.scroll = 0,
                _ => {}
            },
            _ => {}
        }
    }

    // ───────────────────────────── мышь ─────────────────────────────

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
                let l = layout::main(self.area);
                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if l.logs_btn.contains(pos) {
                            self.toggle_logs();
                        } else if l.tabs.contains(pos) {
                            if let Some(t) = layout::tab_at(l.tabs, m.column) {
                                self.set_tab(TABS[t].0);
                                self.focus = Focus::Content;
                            }
                        } else if l.sidebar.contains(pos) {
                            self.focus = Focus::Sidebar;
                            let row = m.row.saturating_sub(l.sidebar.y + 1) as usize;
                            if row < self.servers.len() && row != self.selected {
                                self.selected = row;
                                self.on_select_changed();
                            }
                        } else if l.input.contains(pos) {
                            self.focus = Focus::Input;
                        } else if l.content.contains(pos) {
                            self.focus = Focus::Content;
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if l.sidebar.contains(pos) {
                            self.focus = Focus::Sidebar
                        } else if l.content.contains(pos) {
                            self.focus = Focus::Content
                        }
                        self.move_selection(-1)
                    }
                    MouseEventKind::ScrollDown => {
                        if l.sidebar.contains(pos) {
                            self.focus = Focus::Sidebar
                        } else if l.content.contains(pos) {
                            self.focus = Focus::Content
                        }
                        self.move_selection(1)
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
        if self.screen == Screen::Setup && self.popup.is_none() {
            if let Some(setup) = &mut self.setup {
                setup.paste(s);
            }
        } else if let Some(Popup::Edit { input, .. }) = &mut self.popup {
            input.insert_str(s);
        } else if let Some(inp) = &mut self.logs.search_input {
            inp.insert_str(s);
        } else if self.screen == Screen::Main {
            self.focus = Focus::Input;
            self.input.insert_str(s);
        }
    }
}

fn read_log_file(p: &std::path::Path) -> anyhow::Result<Vec<LogLine>> {
    use std::io::Read;
    const CAP: usize = 50_000;
    let mut bytes = Vec::new();
    let f = std::fs::File::open(p)?;
    if p.to_string_lossy().ends_with(".gz") {
        flate2::read::GzDecoder::new(f).take(256 << 20).read_to_end(&mut bytes)?;
    } else {
        f.take(256 << 20).read_to_end(&mut bytes)?;
    }
    Ok(crate::log::parse_text(&String::from_utf8_lossy(&bytes), CAP))
}

fn random_password() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok();
    if !ok {
        use std::hash::{BuildHasher, Hasher};
        for chunk in buf.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(Instant::now().elapsed().as_nanos());
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["Б", "КиБ", "МиБ", "ГиБ", "ТиБ"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} {}", U[0]) } else { format!("{v:.1} {}", U[i]) }
}

pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, s / 60 % 60, s % 60);
    if h >= 24 {
        format!("{}д {}ч {}м", h / 24, h % 24, m)
    } else if h > 0 {
        format!("{h}ч {m}м")
    } else {
        format!("{m}м {sec}с")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_layout_hotkeys() {
        assert_eq!(latin('д'), 'l');
        assert_eq!(latin('Д'), 'L');
        assert_eq!(latin('л'), 'k');
        assert_eq!(latin('l'), 'l');
        assert_eq!(latin('1'), '1');
    }

    #[test]
    fn formatting() {
        assert_eq!(human_bytes(512), "512 Б");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 МиБ");
        assert_eq!(human_duration(Duration::from_secs(3725)), "1ч 2м");
        assert_eq!(random_password().len(), 32);
    }
}
