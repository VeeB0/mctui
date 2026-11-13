//! Окно TUI: состояние интерфейса и обработка ввода (Update в терминах TEA).
//!
//! Серверами управляет фоновая служба ([`crate::core`]); окно получает от неё снимки состояния,
//! строки логов и историю графиков, а действия отправляет запросами [`Request`].
//! Файлы сервера (server.properties, списки доступа, плагины, бэкапы) окно читает и правит само.

use crate::access::{self, List};
use crate::backup::{self, BackupInfo};
use crate::client::Client;
use crate::config::{Kind, ServerConfig};
use crate::event::{AppEvent, UiTask};
use crate::input::TextInput;
use crate::log::{Level, LogBuffer, LogLine};
use crate::plugins::{self, PluginInfo, UpdateInfo};
use crate::properties::Properties;
use crate::proto::{Msg, NoticeKind, Request, Sample, ServerView};
use crate::server::Status;
use crate::setup::{Outcome as SetupOutcome, Setup, Versions};
use crate::tr;
use crate::ui::layout;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;

pub use crate::core::human_bytes;

const TOAST_FOR: Duration = Duration::from_secs(5);
const HISTORY_CAP: usize = 24 * 60;

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
    Access,
    Plugins,
    Settings,
    Backups,
}

pub const TABS: [Tab; 6] = [Tab::Status, Tab::Players, Tab::Access, Tab::Plugins, Tab::Settings, Tab::Backups];

impl Tab {
    pub fn title(self) -> &'static str {
        match self {
            Tab::Status => tr!("Статус", "Status"),
            Tab::Players => tr!("Игроки", "Players"),
            Tab::Access => tr!("Доступ", "Access"),
            Tab::Plugins => tr!("Плагины", "Plugins"),
            Tab::Settings => tr!("Настройки", "Settings"),
            Tab::Backups => tr!("Бэкапы", "Backups"),
        }
    }
}

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
    Kill(usize),
    RemoteStop(usize),
    Command { id: usize, cmd: String },
    DeleteBackup(PathBuf),
    Restore { id: usize, path: PathBuf },
    EnableRcon,
    RemoveServer(usize),
    AccessRemove { list: List, name: String },
    ApplyUpdates(Vec<PathBuf>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    Autostart,
    AutoRestart,
    Notify,
    BackupEvery,
    BackupKeep,
    RestartTimes,
    Language,
}

impl Opt {
    pub fn label(self) -> &'static str {
        match self {
            Opt::Autostart => tr!("Запускать вместе со службой", "Start with the service"),
            Opt::AutoRestart => tr!("Авто-рестарт при падении", "Restart after a crash"),
            Opt::Notify => tr!("Уведомления рабочего стола", "Desktop notifications"),
            Opt::BackupEvery => tr!("Авто-бэкап, каждые N ч (0 — выкл)", "Auto backup every N h (0 = off)"),
            Opt::BackupKeep => tr!("Хранить бэкапов", "Backups to keep"),
            Opt::RestartTimes => tr!("Рестарт по расписанию (ЧЧ:ММ, …)", "Scheduled restarts (HH:MM, …)"),
            Opt::Language => tr!("Язык интерфейса", "Interface language"),
        }
    }
}

pub enum EditTarget {
    Property(String),
    Opt(Opt),
    AccessAdd(List),
    Chat,
}

pub enum Popup {
    Confirm {
        title: String,
        text: String,
        action: Action,
    },
    Edit {
        title: String,
        input: TextInput,
        target: EditTarget,
    },
    /// Выход при работающих серверах: закрыть окно или остановить всё.
    Quit {
        running: usize,
    },
}

#[derive(Debug, Clone)]
pub enum LogSource {
    /// Вывод процесса из службы (или сессия RCON).
    Live,
    /// Только сообщения чата из живого вывода.
    Chat,
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
            LevelFilter::All => tr!("все", "all"),
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

    pub fn active_source(&self) -> Option<&LogSource> {
        self.sources.get(self.active).map(|s| &s.1)
    }
}

/// Сервер глазами окна: снимок от службы + то, что окно держит само.
pub struct ServerUi {
    pub view: ServerView,
    pub log: LogBuffer,
    pub history: VecDeque<Sample>,
    pub props: Option<Properties>,
    pub props_error: Option<String>,
}

impl std::ops::Deref for ServerUi {
    type Target = ServerView;
    fn deref(&self) -> &ServerView {
        &self.view
    }
}

impl ServerUi {
    fn new(view: ServerView) -> Self {
        let mut s =
            Self { view, log: LogBuffer::new(10_000), history: VecDeque::new(), props: None, props_error: None };
        s.reload_props();
        s
    }

    pub fn reload_props(&mut self) {
        let Some(path) = self.view.cfg.properties_path() else {
            self.props = None;
            return;
        };
        match Properties::load(&path) {
            Ok(p) => {
                self.props = Some(p);
                self.props_error = None;
            }
            Err(e) => {
                self.props = None;
                self.props_error = Some(format!("{}: {e}", path.display()));
            }
        }
    }

    pub fn prop(&self, key: &str) -> Option<&str> {
        self.props.as_ref().and_then(|p| p.get(key))
    }

    /// Можно ли отправлять команды: процесс запущен или RCON подключён.
    pub fn accepts_commands(&self) -> bool {
        self.running() || self.rcon == crate::proto::RconView::Connected
    }
}

pub struct AccessView {
    pub list: List,
    pub entries: Vec<access::Entry>,
    pub sel: usize,
    pub error: Option<String>,
    /// Для какого сервера/списка и с каким mtime файла загружено.
    loaded: Option<(usize, List, Option<SystemTime>)>,
    pub busy: bool,
}

pub struct PluginsView {
    pub items: Vec<PluginInfo>,
    pub sel: usize,
    pub updates: HashMap<PathBuf, UpdateInfo>,
    pub checking: bool,
    pub checked: bool,
    pub busy: Option<String>,
    loaded_for: Option<usize>,
}

/// Состояние автодополнения по Tab (повторный Tab — следующий вариант).
pub struct Completion {
    base: String,
    items: Vec<String>,
    idx: usize,
    applied: String,
}

pub struct App {
    pub servers: Vec<ServerUi>,
    pub selected: usize,
    pub screen: Screen,
    pub tab: Tab,
    pub focus: Focus,
    pub input: TextInput,
    pub history: Vec<String>,
    pub hist_pos: Option<usize>,
    pub completion: Option<Completion>,
    /// Варианты автодополнения для строки подсказок.
    pub completion_hint: Option<String>,
    pub popup: Option<Popup>,
    pub toast: Option<(String, ToastKind, Instant)>,
    pub should_quit: bool,
    pub exit_message: Option<String>,
    pub players_sel: usize,
    pub settings_sel: usize,
    pub backups: Vec<BackupInfo>,
    pub backups_sel: usize,
    backups_at: Option<Instant>,
    pub access: AccessView,
    pub plugins: PluginsView,
    pub logs: LogsView,
    /// Высота области лога из последней отрисовки — для прыжков к совпадениям.
    pub logs_height: Cell<u16>,
    pub area: Rect,
    pub setup: Option<Setup>,
    pub config_path: PathBuf,
    /// Новая версия на GitHub, если есть.
    pub update_available: Option<String>,
    pub language: Option<String>,
    /// Связь со службой установлена и пришёл первый снимок.
    pub connected: bool,
    pub got_state: bool,
    pub daemon_error: Option<String>,
    pub daemon_version: Option<String>,
    /// Служба другой версии и занята серверами — показываем предупреждение.
    pub daemon_outdated: bool,
    version_check_pending: bool,
    /// После добавления сервера выбрать первый с id больше этого.
    select_new_after: Option<usize>,
    client: Client,
    tx: mpsc::UnboundedSender<AppEvent>,
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
    pub fn new(config_path: PathBuf, client: Client, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        let language = crate::config::Config::load(Some(config_path.clone())).ok().and_then(|(c, _)| c.language);
        Self {
            servers: vec![],
            selected: 0,
            screen: Screen::Main,
            tab: Tab::Status,
            focus: Focus::Sidebar,
            input: TextInput::default(),
            history: vec![],
            hist_pos: None,
            completion: None,
            completion_hint: None,
            popup: None,
            toast: None,
            should_quit: false,
            exit_message: None,
            players_sel: 0,
            settings_sel: 0,
            backups: vec![],
            backups_sel: 0,
            backups_at: None,
            access: AccessView {
                list: List::Whitelist,
                entries: vec![],
                sel: 0,
                error: None,
                loaded: None,
                busy: false,
            },
            plugins: PluginsView {
                items: vec![],
                sel: 0,
                updates: HashMap::new(),
                checking: false,
                checked: false,
                busy: None,
                loaded_for: None,
            },
            logs: LogsView::new(),
            logs_height: Cell::new(20),
            area: Rect::default(),
            setup: None,
            config_path,
            update_available: None,
            language,
            connected: false,
            got_state: false,
            daemon_error: None,
            daemon_version: None,
            daemon_outdated: false,
            version_check_pending: false,
            select_new_after: None,
            client,
            tx,
        }
    }

    pub fn has_server(&self) -> bool {
        !self.servers.is_empty()
    }

    pub fn sel(&self) -> &ServerUi {
        &self.servers[self.selected.min(self.servers.len().saturating_sub(1))]
    }

    fn sel_mut(&mut self) -> &mut ServerUi {
        let i = self.selected.min(self.servers.len().saturating_sub(1));
        &mut self.servers[i]
    }

    fn sel_id(&self) -> Option<usize> {
        self.servers.get(self.selected).map(|s| s.id)
    }

    pub fn toast(&mut self, msg: impl Into<String>, kind: ToastKind) {
        self.toast = Some((msg.into(), kind, Instant::now()));
    }

    fn send(&self, r: Request) {
        self.client.send(r);
    }

    /// Запуск блокирующей работы в фоне; результат применяется к окну.
    fn spawn_task<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
        apply: impl FnOnce(&mut App, T) + Send + 'static,
    ) {
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let v = work();
            let _ = tx.send(AppEvent::Ui(UiTask(Box::new(move |app: &mut App| apply(app, v)))));
        });
    }

    // ───────────────────────────── серверные операции ─────────────────────────────

    pub fn start(&mut self) {
        if let Some(id) = self.sel_id() {
            self.send(Request::Start { id });
        }
    }

    pub fn stop(&mut self, restart: bool) {
        if !self.has_server() {
            return;
        }
        let s = self.sel();
        let id = s.id;
        if s.cfg.kind == Kind::Rcon && !restart {
            self.popup = Some(Popup::Confirm {
                title: tr!("Остановка", "Stop").into(),
                text: tr!(
                    format!("Отправить stop на удалённый сервер «{}»?", s.cfg.name),
                    format!("Send stop to the remote server “{}”?", s.cfg.name)
                ),
                action: Action::RemoteStop(id),
            });
            return;
        }
        if s.status == Status::Stopping && s.running() {
            self.popup = Some(Popup::Confirm {
                title: tr!("Принудительно", "Force").into(),
                text: tr!(
                    format!(
                        "«{}» уже останавливается. Убить процесс (kill -9)? Несохранённые данные пропадут.",
                        s.cfg.name
                    ),
                    format!(
                        "“{}” is already stopping. Kill the process (kill -9)? Unsaved data will be lost.",
                        s.cfg.name
                    )
                ),
                action: Action::Kill(id),
            });
            return;
        }
        self.send(Request::Stop { id, restart });
    }

    pub fn backup(&mut self) {
        if let Some(id) = self.sel_id() {
            self.send(Request::Backup { id });
        }
    }

    fn submit_input(&mut self) {
        let cmd = self.input.take();
        self.completion = None;
        self.completion_hint = None;
        if cmd.trim().is_empty() || !self.has_server() {
            return;
        }
        if self.history.last() != Some(&cmd) {
            self.history.push(cmd.clone());
            if self.history.len() > 500 {
                self.history.remove(0);
            }
        }
        self.hist_pos = None;
        let id = self.sel().id;
        self.send(Request::Command { id, cmd: cmd.trim().trim_start_matches('/').to_string(), user: true });
    }

    fn complete_input(&mut self) {
        if !self.has_server() {
            return;
        }
        let value = self.input.value.clone();
        if let Some(c) = &mut self.completion
            && c.applied == value
            && c.items.len() > 1
        {
            c.idx = (c.idx + 1) % c.items.len();
            let v = format!("{}{}", c.base, c.items[c.idx]);
            c.applied = v.clone();
            self.input.set(&v);
            return;
        }
        let players = self.sel().players.clone();
        let (start, items) = crate::complete::candidates(&value, &players);
        let base = value[..start].to_string();
        let word_len = value[start..].chars().count();
        self.completion = None;
        match items.len() {
            0 => self.completion_hint = Some(tr!("нет вариантов", "no matches").into()),
            1 => {
                self.input.set(&format!("{base}{} ", items[0]));
                self.completion_hint = None;
            }
            _ => {
                let prefix = crate::complete::common_prefix(&items);
                let shown: Vec<&str> = items.iter().take(12).map(String::as_str).collect();
                let more = if items.len() > 12 { format!(" … +{}", items.len() - 12) } else { String::new() };
                self.completion_hint = Some(format!("{}{more}", shown.join("  ")));
                let v = if prefix.chars().count() > word_len {
                    format!("{base}{prefix}")
                } else {
                    format!("{base}{}", items[0])
                };
                self.input.set(&v);
                self.completion = Some(Completion { base, items, idx: 0, applied: v });
            }
        }
    }

    fn request_quit(&mut self) {
        let running = self.servers.iter().filter(|s| s.running()).count();
        if running == 0 || !self.connected {
            self.should_quit = true;
        } else {
            self.popup = Some(Popup::Quit { running });
        }
    }

    fn run_action(&mut self, a: Action) {
        match a {
            Action::Kill(id) => self.send(Request::Kill { id }),
            Action::RemoteStop(id) => self.send(Request::Stop { id, restart: false }),
            Action::Command { id, cmd } => self.send(Request::Command { id, cmd, user: false }),
            Action::DeleteBackup(p) => match std::fs::remove_file(&p) {
                Ok(()) => {
                    self.toast(tr!("бэкап удалён", "backup deleted"), ToastKind::Ok);
                    self.refresh_backups();
                }
                Err(e) => self
                    .toast(tr!(format!("не удалось удалить: {e}"), format!("could not delete: {e}")), ToastKind::Error),
            },
            Action::Restore { id, path } => self.send(Request::Restore { id, path }),
            Action::EnableRcon => self.enable_rcon(),
            Action::RemoveServer(id) => self.send(Request::RemoveServer { id }),
            Action::AccessRemove { list, name } => self.access_remove(list, &name),
            Action::ApplyUpdates(paths) => self.apply_updates(paths),
        }
    }

    fn save_props(&mut self, msg_ok: String) {
        let s = self.sel_mut();
        let (Some(path), Some(props)) = (s.cfg.properties_path(), &s.props) else { return };
        let res = props.save(&path);
        let id = s.id;
        let running = s.running();
        s.reload_props();
        match res {
            Ok(()) => {
                self.send(Request::PropsChanged { id });
                let tail = if running {
                    tr!(" — нужен перезапуск (Ctrl+R)", " — restart needed (Ctrl+R)")
                } else {
                    ""
                };
                self.toast(format!("{msg_ok}{tail}"), ToastKind::Ok);
            }
            Err(e) => self.toast(tr!(format!("ошибка записи: {e}"), format!("write error: {e}")), ToastKind::Error),
        }
    }

    fn enable_rcon(&mut self) {
        let s = self.sel_mut();
        let Some(props) = &mut s.props else {
            self.toast(tr!("server.properties не загружен", "server.properties is not loaded"), ToastKind::Error);
            return;
        };
        props.set("enable-rcon", "true");
        if props.get("rcon.password").unwrap_or("").is_empty() {
            props.set("rcon.password", &crate::daemon::random_token()[..32]);
        }
        if props.get("rcon.port").unwrap_or("").is_empty() {
            props.set("rcon.port", "25575");
        }
        // Не дублировать опросы list/tps операторам в чат.
        props.set("broadcast-rcon-to-ops", "false");
        self.save_props(
            tr!(
                "RCON включён: после перезапуска появятся TPS и точный список игроков",
                "RCON enabled: TPS and the exact player list appear after a restart"
            )
            .into(),
        );
    }

    fn set_property(&mut self, key: &str, value: &str) {
        let s = self.sel_mut();
        let Some(props) = &mut s.props else { return };
        props.set(key, value);
        self.save_props(tr!(format!("{key} = {value} — сохранено"), format!("{key} = {value} — saved")));
    }

    // ───────────────────────────── настройки mctui ─────────────────────────────

    /// Строки настроек mctui над server.properties.
    pub fn opts(&self) -> Vec<Opt> {
        if self.has_server() && self.sel().is_local() {
            vec![
                Opt::Autostart,
                Opt::AutoRestart,
                Opt::Notify,
                Opt::BackupEvery,
                Opt::BackupKeep,
                Opt::RestartTimes,
                Opt::Language,
            ]
        } else {
            vec![Opt::Language]
        }
    }

    pub fn opt_value(&self, o: Opt) -> String {
        let c = &self.sel().cfg;
        let yes_no = |b: bool| if b { tr!("да", "yes") } else { tr!("нет", "no") }.to_string();
        match o {
            Opt::Autostart => yes_no(c.autostart),
            Opt::AutoRestart => yes_no(c.auto_restart),
            Opt::Notify => yes_no(c.notify),
            Opt::BackupEvery => c.backup_every_hours.to_string(),
            Opt::BackupKeep => c.backup_keep.to_string(),
            Opt::RestartTimes => {
                if c.restart_times.is_empty() {
                    tr!("выкл", "off").into()
                } else {
                    c.restart_times.join(", ")
                }
            }
            Opt::Language => match self.language.as_deref() {
                Some("ru") => "Русский".into(),
                Some("en") => "English".into(),
                _ => tr!("как в системе", "system").into(),
            },
        }
    }

    fn update_cfg(&mut self, f: impl FnOnce(&mut ServerConfig)) {
        let s = self.sel_mut();
        let mut cfg = s.view.cfg.clone();
        f(&mut cfg);
        s.view.cfg = cfg.clone(); // сразу видно в окне, служба пришлёт подтверждение
        let id = s.id;
        self.send(Request::UpdateServer { id, cfg: Box::new(cfg) });
    }

    fn toggle_opt(&mut self, o: Opt) {
        match o {
            Opt::Autostart => self.update_cfg(|c| c.autostart = !c.autostart),
            Opt::AutoRestart => self.update_cfg(|c| c.auto_restart = !c.auto_restart),
            Opt::Notify => self.update_cfg(|c| c.notify = !c.notify),
            Opt::Language => {
                let next = match self.language.as_deref() {
                    None => Some("ru"),
                    Some("ru") => Some("en"),
                    _ => None,
                };
                self.language = next.map(str::to_string);
                crate::i18n::init(next);
                self.send(Request::SetLanguage { language: self.language.clone() });
                if self.screen == Screen::Logs {
                    self.build_log_sources();
                }
            }
            _ => self.edit_opt(o),
        }
    }

    fn edit_opt(&mut self, o: Opt) {
        let c = &self.sel().cfg;
        let v = match o {
            Opt::BackupEvery => c.backup_every_hours.to_string(),
            Opt::BackupKeep => c.backup_keep.to_string(),
            Opt::RestartTimes => c.restart_times.join(", "),
            _ => return self.toggle_opt(o),
        };
        self.popup =
            Some(Popup::Edit { title: o.label().into(), input: TextInput::with(&v), target: EditTarget::Opt(o) });
    }

    fn apply_opt(&mut self, o: Opt, v: &str) {
        let bad = |app: &mut App| app.toast(tr!("неверное значение", "invalid value"), ToastKind::Error);
        match o {
            Opt::BackupEvery => match v.trim().parse::<u32>() {
                Ok(n) if n <= 24 * 30 => self.update_cfg(|c| c.backup_every_hours = n),
                _ => bad(self),
            },
            Opt::BackupKeep => match v.trim().parse::<usize>() {
                Ok(n) if n >= 1 => self.update_cfg(|c| c.backup_keep = n),
                _ => bad(self),
            },
            Opt::RestartTimes => {
                let parts: Vec<String> =
                    v.split([',', ' ', ';']).map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect();
                if parts.iter().all(|p| crate::config::parse_hm(p).is_some()) {
                    let norm: Vec<String> = parts
                        .iter()
                        .filter_map(|p| crate::config::parse_hm(p))
                        .map(|(h, m)| format!("{h:02}:{m:02}"))
                        .collect();
                    self.update_cfg(|c| c.restart_times = norm);
                } else {
                    self.toast(
                        tr!(
                            "время — ЧЧ:ММ через запятую, например 04:00, 16:00",
                            "times are HH:MM, comma-separated, e.g. 04:00, 16:00"
                        ),
                        ToastKind::Error,
                    );
                }
            }
            _ => {}
        }
    }

    // ───────────────────────────── бэкапы ─────────────────────────────

    pub fn refresh_backups(&mut self) {
        self.backups_at = Some(Instant::now());
        if !self.has_server() {
            self.backups.clear();
            return;
        }
        let s = self.sel();
        self.backups =
            if s.is_local() { backup::list(&s.cfg.backup_dir(), &backup::file_prefix(&s.cfg.name)) } else { vec![] };
        self.backups_sel = self.backups_sel.min(self.backups.len().saturating_sub(1));
    }

    // ───────────────────────────── доступ ─────────────────────────────

    fn access_mtime(&self) -> Option<SystemTime> {
        let dir = self.sel().cfg.dir.clone()?;
        std::fs::metadata(self.access.list.path(&dir)).and_then(|m| m.modified()).ok()
    }

    /// Перечитать список доступа, если сменился сервер/список или файл изменился.
    fn refresh_access(&mut self, force: bool) {
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
    }

    fn cycle_access_list(&mut self, d: isize) {
        let all = List::ALL;
        let i = all.iter().position(|l| *l == self.access.list).unwrap_or(0) as isize;
        self.access.list = all[(i + d).rem_euclid(all.len() as isize) as usize];
        self.access.sel = 0;
        self.refresh_access(true);
    }

    fn access_add(&mut self, list: List, value: &str) {
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

    fn access_remove(&mut self, list: List, name: &str) {
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

    fn toggle_whitelist(&mut self) {
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

    // ───────────────────────────── плагины ─────────────────────────────

    fn refresh_plugins(&mut self, force: bool) {
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

    fn toggle_plugin(&mut self) {
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

    fn check_plugin_updates(&mut self) {
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

    fn ask_apply_updates(&mut self, all: bool) {
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

    fn apply_updates(&mut self, paths: Vec<PathBuf>) {
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

    // ───────────────────────────── выбор сервера и вкладок ─────────────────────────────

    fn on_select_changed(&mut self) {
        self.players_sel = 0;
        self.settings_sel = 0;
        self.backups_sel = 0;
        self.access.sel = 0;
        self.plugins.sel = 0;
        self.completion = None;
        self.completion_hint = None;
        if self.has_server() {
            self.sel_mut().reload_props();
        }
        self.on_tab_shown(true);
    }

    fn on_tab_shown(&mut self, force: bool) {
        match self.tab {
            Tab::Settings | Tab::Access => {
                if self.has_server() {
                    self.sel_mut().reload_props();
                }
                if self.tab == Tab::Access {
                    self.refresh_access(force);
                }
            }
            Tab::Backups => self.refresh_backups(),
            Tab::Plugins => self.refresh_plugins(force),
            _ => {}
        }
    }

    fn set_tab(&mut self, t: Tab) {
        self.tab = t;
        self.on_tab_shown(false);
    }

    // ───────────────────────────── события ─────────────────────────────

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
                        format!("Доступна версия {v} — выполните «mctui update»"),
                        format!("Version {v} is available — run “mctui update”")
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

    fn on_daemon(&mut self, m: Msg) {
        match m {
            Msg::Welcome { version, .. } => {
                self.connected = true;
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

    fn on_state(&mut self, views: Vec<ServerView>) {
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
            if self.servers.iter().any(|s| s.running()) {
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

    // ───────────────────────────── список серверов ─────────────────────────────

    pub fn open_setup(&mut self) {
        let mut s = Setup::new(self.servers.is_empty());
        s.error = None;
        self.setup = Some(s);
        self.screen = Screen::Setup;
        self.check_setup_versions();
    }

    fn ask_remove_server(&mut self) {
        if !self.has_server() {
            return;
        }
        let s = self.sel();
        if s.running() || s.backup_progress.is_some() {
            self.toast(
                tr!(
                    "сначала остановите сервер и дождитесь окончания бэкапа",
                    "stop the server and wait for the backup to finish first"
                ),
                ToastKind::Error,
            );
            return;
        }
        self.popup = Some(Popup::Confirm {
            title: tr!("Убрать из списка", "Remove from list").into(),
            text: tr!(
                format!("Убрать «{}» из mctui? Файлы сервера и бэкапы останутся на диске.", s.cfg.name),
                format!("Remove “{}” from mctui? Server files and backups stay on disk.", s.cfg.name)
            ),
            action: Action::RemoveServer(s.id),
        });
    }

    fn close_setup(&mut self) {
        self.setup = None;
        self.screen = Screen::Main;
        self.focus = Focus::Sidebar;
    }

    /// Загрузить список версий ядра для формы «Создать новый», если ещё не загружен.
    fn check_setup_versions(&mut self) {
        let Some(setup) = &mut self.setup else { return };
        let Some(core) = setup.wants_versions() else { return };
        setup.set_versions(core, Versions::Loading);
        self.spawn_task(
            move || crate::create::list_versions(core).map_err(|e| format!("{e:#}")),
            move |app, res| {
                if let Some(s) = &mut app.setup {
                    s.set_versions(
                        core,
                        match res {
                            Ok(v) => Versions::Ready(v),
                            Err(e) => Versions::Failed(e),
                        },
                    );
                }
            },
        );
    }

    fn add_server(&mut self, cfg: ServerConfig) {
        self.select_new_after = Some(self.servers.iter().map(|s| s.id).max().unwrap_or(0));
        let name = cfg.name.clone();
        let local = cfg.kind == Kind::Local;
        self.send(Request::AddServer { cfg: Box::new(cfg) });
        self.close_setup();
        let hint = if local { tr!(" — Ctrl+S, чтобы запустить", " — Ctrl+S to start") } else { "" };
        self.toast(
            tr!(format!("Сервер «{name}» добавлен{hint}"), format!("Server “{name}” added{hint}")),
            ToastKind::Ok,
        );
    }

    fn create_server(&mut self, req: crate::setup::CreateRequest) {
        if let Some(s) = &mut self.setup {
            s.creating = Some(tr!("подготовка…", "preparing…").into());
            s.error = None;
        }
        let tx = self.tx.clone();
        let core = req.opts.core;
        self.spawn_task(
            move || {
                let mut progress = |p: crate::create::Progress| {
                    let text = match p {
                        crate::create::Progress::Step(s) => s,
                        crate::create::Progress::Download { what, done, total } => match total {
                            Some(t) if t > 0 => {
                                format!("{what}: {} / {} ({}%)", human_bytes(done), human_bytes(t), done * 100 / t)
                            }
                            _ => format!("{what}: {}", human_bytes(done)),
                        },
                    };
                    let _ = tx.send(AppEvent::Ui(UiTask(Box::new(move |app: &mut App| {
                        if let Some(s) = &mut app.setup {
                            s.creating = Some(text);
                        }
                    }))));
                };
                crate::create::create(&req.opts, &mut progress).map(|c| (c, req.name)).map_err(|e| format!("{e:#}"))
            },
            move |app, res| match res {
                Ok((created, name)) => {
                    // Paper и Purpur хранят Незер и Энд отдельными папками.
                    let paths: Vec<String> = match core {
                        crate::create::Core::Paper | crate::create::Core::Purpur => {
                            vec!["world".into(), "world_nether".into(), "world_the_end".into()]
                        }
                        _ => vec!["world".into()],
                    };
                    let cfg = ServerConfig::new_local(&name, created.dir, created.command, paths);
                    app.add_server(cfg);
                }
                Err(e) => {
                    if let Some(s) = &mut app.setup {
                        s.creating = None;
                        s.error = Some(e);
                    }
                }
            },
        );
    }

    fn setup_key(&mut self, k: KeyEvent) {
        let Some(setup) = &mut self.setup else { return };
        match setup.handle_key(k) {
            SetupOutcome::None => {}
            SetupOutcome::Cancel => {
                if self.servers.is_empty() {
                    self.should_quit = true;
                } else {
                    self.close_setup();
                }
            }
            SetupOutcome::Save(cfg) => {
                // Та же папка уже в списке?
                let dup = self
                    .servers
                    .iter()
                    .find(|s| s.cfg.kind == Kind::Local && cfg.kind == Kind::Local && s.cfg.dir == cfg.dir)
                    .map(|s| s.cfg.name.clone());
                match dup {
                    Some(n) => {
                        if let Some(s) = &mut self.setup {
                            s.error = Some(tr!(
                                format!("Эта папка уже добавлена как «{n}»"),
                                format!("This folder is already added as “{n}”")
                            ));
                        }
                    }
                    None => self.add_server(*cfg),
                }
            }
            SetupOutcome::Create(req) => self.create_server(*req),
        }
        self.check_setup_versions();
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
                _ => {}
            }
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

    fn popup_key(&mut self, p: Popup, k: KeyEvent) {
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
                            "Серверы продолжают работать в фоне ({running}). Вернуться: mctui · состояние: mctui daemon status"
                        ),
                        format!(
                            "Servers keep running in the background ({running}). Come back: mctui · status: mctui daemon status"
                        )
                    ));
                    self.should_quit = true;
                }
                KeyCode::Char(c) if latin(c) == 's' => {
                    self.send(Request::StopAll);
                    self.exit_message = Some(
                        tr!(
                            "Серверы штатно останавливаются в фоне (mctui daemon status).",
                            "Servers are shutting down cleanly in the background (mctui daemon status)."
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
            Popup::Edit { title, mut input, target } => match k.code {
                KeyCode::Enter => {
                    let v = input.value.clone();
                    match target {
                        EditTarget::Property(key) => self.set_property(&key, v.trim()),
                        EditTarget::Opt(o) => self.apply_opt(o, &v),
                        EditTarget::AccessAdd(list) => self.access_add(list, &v),
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

    fn main_key(&mut self, k: KeyEvent) {
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
                    '1'..='6' => {
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
            KeyCode::Left | KeyCode::Right if self.focus == Focus::Content && self.tab == Tab::Access => {
                self.cycle_access_list(if k.code == KeyCode::Left { -1 } else { 1 })
            }
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
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        let n = TABS.len() as isize;
        self.set_tab(TABS[((i + d).rem_euclid(n)) as usize]);
    }

    pub fn settings_len(&self) -> usize {
        self.opts().len() + self.sel().props.as_ref().map(|p| p.pairs().len()).unwrap_or(0)
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
                Tab::Settings => self.settings_sel = step(self.settings_sel, self.settings_len()),
                Tab::Backups => self.backups_sel = step(self.backups_sel, self.backups.len()),
                Tab::Access => self.access.sel = step(self.access.sel, self.access.entries.len()),
                Tab::Plugins => self.plugins.sel = step(self.plugins.sel, self.plugins.items.len()),
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
        self.sel().players.get(self.players_sel).cloned()
    }

    /// Выбранная строка вкладки «Настройки»: опция mctui или пара server.properties.
    fn selected_setting(&self) -> Option<Result<Opt, (String, String)>> {
        let opts = self.opts();
        if self.settings_sel < opts.len() {
            return Some(Ok(opts[self.settings_sel]));
        }
        let i = self.settings_sel - opts.len();
        self.sel().props.as_ref()?.pairs().into_iter().nth(i).map(Err)
    }

    fn content_char(&mut self, c: char) {
        let id = self.sel().id;
        match self.tab {
            Tab::Players => {
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
                    Some(Err((k, v))) => match v.as_str() {
                        "true" => self.set_property(&k, "false"),
                        "false" => self.set_property(&k, "true"),
                        _ => self.toast(
                            tr!(
                                "пробел переключает только true/false; Enter — редактировать",
                                "Space toggles only true/false; Enter edits"
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
                        self.popup = Some(Popup::Confirm {
                            title: tr!("Удалить бэкап", "Delete backup").into(),
                            text: tr!(format!("Удалить {name} безвозвратно?"), format!("Delete {name} permanently?")),
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
                            tr!(
                                " Сервер будет остановлен и запущен снова.",
                                " The server will be stopped and started again."
                            )
                        } else {
                            ""
                        };
                        self.popup = Some(Popup::Confirm {
                            title: tr!("Восстановить из бэкапа", "Restore from backup").into(),
                            text: tr!(
                                format!(
                                    "Восстановить {paths} из {name}?{running} Текущие файлы переедут в .mctui-before-restore/."
                                ),
                                format!(
                                    "Restore {paths} from {name}?{running} Current files move to .mctui-before-restore/."
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
            Tab::Plugins => match c {
                ' ' => self.toggle_plugin(),
                'c' => self.check_plugin_updates(),
                'U' => self.ask_apply_updates(true),
                'u' => self.refresh_plugins(true),
                _ => {}
            },
            Tab::Status => {}
        }
    }

    fn content_enter(&mut self) {
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
            Tab::Plugins => self.ask_apply_updates(false),
            Tab::Access => self.content_char('a'),
            _ => {}
        }
    }

    // ───────────────────────────── экран логов ─────────────────────────────

    pub fn toggle_logs(&mut self) {
        if self.screen == Screen::Logs {
            self.screen = Screen::Main;
            return;
        }
        if !self.has_server() {
            return;
        }
        self.screen = Screen::Logs;
        self.build_log_sources();
        let s = self.sel();
        // По умолчанию: живой вывод, если сервер работает (или это RCON), иначе latest.log.
        let prefer_live = s.running()
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
                let live = if s.running() {
                    tr!("● Живой вывод", "● Live output")
                } else {
                    tr!("Вывод последнего запуска", "Last run output")
                };
                v.push((live.into(), LogSource::Live));
                v.push((tr!("💬 Чат", "💬 Chat").into(), LogSource::Chat));
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
                v.push((tr!("Сессия RCON", "RCON session").into(), LogSource::Live));
                if let Some(p) = &s.cfg.log_path {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    v.push((tr!("💬 Чат", "💬 Chat").into(), LogSource::Chat));
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

    /// Строки активного источника после фильтра.
    pub fn visible_log_lines(&self) -> Vec<&LogLine> {
        let f = self.logs.filter;
        match self.logs.active_source() {
            Some(LogSource::Live) => self.sel().log.iter().filter(|l| f.pass(l.level)).collect(),
            Some(LogSource::Chat) => {
                let s = self.sel();
                // У RCON-сервера живого вывода нет — чат берётся из его файла лога.
                if s.is_local() || s.log.iter().any(|l| crate::server::stats::is_chat(&l.text)) {
                    s.log.iter().filter(|l| crate::server::stats::is_chat(&l.text)).collect()
                } else {
                    vec![]
                }
            }
            Some(LogSource::File(_)) => self.logs.file_lines.iter().filter(|l| f.pass(l.level)).collect(),
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
                self.toast(tr!("совпадений нет", "no matches"), ToastKind::Info);
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
        let chat = matches!(self.logs.active_source(), Some(LogSource::Chat));
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
            KeyCode::Enter if chat => self.open_chat_input(),
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
                'm' | 'i' => self.open_chat_input(),
                'c' => {
                    let i = self.logs.sources.iter().position(|(_, s)| matches!(s, LogSource::Chat));
                    if let Some(i) = i {
                        self.open_log_source(i);
                    }
                }
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

    fn open_chat_input(&mut self) {
        if !self.sel().accepts_commands() {
            return self.toast(tr!("сервер не запущен", "the server is not running"), ToastKind::Info);
        }
        self.popup = Some(Popup::Edit {
            title: tr!("Сообщение в чат (say)", "Chat message (say)").into(),
            input: TextInput::default(),
            target: EditTarget::Chat,
        });
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
                if !self.has_server() {
                    return;
                }
                let l = layout::main(self.area);
                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if l.logs_btn.contains(pos) {
                            self.toggle_logs();
                        } else if l.tabs.contains(pos) {
                            if let Some(t) = layout::tab_at(l.tabs, m.column) {
                                self.set_tab(TABS[t]);
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
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        if l.sidebar.contains(pos) {
                            self.focus = Focus::Sidebar
                        } else if l.content.contains(pos) {
                            self.focus = Focus::Content
                        }
                        self.move_selection(if m.kind == MouseEventKind::ScrollUp { -1 } else { 1 })
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
        if let Some(Popup::Edit { input, .. }) = &mut self.popup {
            input.insert_str(s.trim_end_matches(['\n', '\r']));
        } else if self.screen == Screen::Setup && self.popup.is_none() {
            if let Some(setup) = &mut self.setup {
                setup.paste(s);
            }
        } else if let Some(inp) = &mut self.logs.search_input {
            inp.insert_str(s);
        } else if self.screen == Screen::Main && self.has_server() {
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

pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, s / 60 % 60, s % 60);
    if h >= 24 {
        tr!(format!("{}д {}ч {}м", h / 24, h % 24, m), format!("{}d {}h {}m", h / 24, h % 24, m))
    } else if h > 0 {
        tr!(format!("{h}ч {m}м"), format!("{h}h {m}m"))
    } else {
        tr!(format!("{m}м {sec}с"), format!("{m}m {sec}s"))
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
        crate::i18n::set(crate::i18n::Lang::Ru);
        assert_eq!(human_duration(Duration::from_secs(3725)), "1ч 2м");
        crate::i18n::set(crate::i18n::Lang::En);
        assert_eq!(human_duration(Duration::from_secs(3725)), "1h 2m");
        crate::i18n::set(crate::i18n::Lang::Ru);
    }
}
