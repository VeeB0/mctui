//! Окно TUI: состояние интерфейса и обработка ввода (Update в терминах TEA).
//!
//! Серверами управляет фоновая служба ([`crate::core`]); окно получает от неё снимки состояния,
//! строки логов и историю графиков, а действия отправляет запросами [`Request`].
//! Файлы сервера (server.properties, списки доступа, плагины, бэкапы) окно читает и правит само.

use crate::access::{self, List};
use crate::backup::{self, BackupInfo};
use crate::client::Client;
use crate::config::{Kind, ServerConfig};
use crate::datapacks::{self, DatapackInfo};
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

mod access_tab;
mod addons_tab;
mod backup_health;
mod datapacks_tab;
mod events;
mod help;
mod keys;
mod logs_screen;
mod maintain;
mod mouse;
mod panel;
mod plugins_tab;
mod settings_tab;
mod setup_flow;
mod sub_tabs;
mod tools;
mod world_tab;
pub use backup_health::{BackupHealth, ago};
pub use logs_screen::{LevelFilter, LogSource, LogsView};
#[cfg(test)]
pub(crate) use panel::PanelKind;
pub use panel::{Panel, Tone};
pub use world_tab::{WorldRow, WorldView};

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
    Addons,
    Settings,
    Backups,
    World,
}

pub const TABS: [Tab; 7] =
    [Tab::Status, Tab::Players, Tab::Access, Tab::Addons, Tab::Settings, Tab::Backups, Tab::World];

impl Tab {
    pub fn title(self) -> &'static str {
        match self {
            Tab::Status => tr!("Статус", "Status"),
            Tab::Players => tr!("Игроки", "Players"),
            Tab::Access => tr!("Доступ", "Access"),
            Tab::Addons => tr!("Дополнения", "Add-ons"),
            Tab::Settings => tr!("Настройки", "Settings"),
            Tab::Backups => tr!("Бэкапы", "Backups"),
            Tab::World => tr!("Мир", "World"),
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
    Stop {
        id: usize,
        restart: bool,
    },
    Kill(usize),
    RemoteStop(usize),
    Command {
        id: usize,
        cmd: String,
    },
    DeleteBackup(PathBuf),
    Restore {
        id: usize,
        path: PathBuf,
    },
    EnableRcon,
    RemoveServer(usize),
    AccessRemove {
        list: List,
        name: String,
    },
    ApplyUpdates(Vec<PathBuf>),
    ReloadDatapacks(usize),
    // `datapack enable "file/<имя>"` на сервере `id`: пак выключен в игре командой `/datapack`.
    EnableDatapackInGame(usize, Box<DatapackInfo>),
    DatapackToggle(PathBuf),
    ApplyDatapackUpdates(Vec<PathBuf>),
    InstallPlugin(Box<crate::plugin_search::InstallPlan>),
    ResetDimension(crate::world::Dimension),
    /// Второй шаг сброса верхнего мира: спросить имя мира.
    AskOverworldName,
    SetLevel(String),
    ApplyCore(Box<maintain::CoreTarget>),
    SelfUpdate(String),
    CloneServer(Box<crate::clone::ClonePlan>),
    JvmApply(crate::jvm::Change),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    Autostart,
    AutoRestart,
    Notify,
    BackupEvery,
    BackupKeep,
    BackupMode,
    BackupCopyTo,
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
            Opt::BackupMode => tr!("Вид бэкапа", "Backup type"),
            Opt::BackupCopyTo => tr!("Копировать бэкапы в (папки, rclone:…)", "Copy backups to (folders, rclone:…)"),
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
    LogSearch,
    PregenRadius(crate::world::Dimension),
    /// Подтверждение сброса верхнего мира: нужно ввести имя мира (`level`).
    ResetOverworld {
        level: String,
    },
    JvmMemory {
        xms_equal: bool,
    },
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
    Panel(Box<Panel>),
    /// Справка по клавишам (`?` / `F1`); `scroll` — сдвиг в строках.
    Help {
        scroll: u16,
    },
}

/// Сервер глазами окна: снимок от службы + то, что окно держит само.
pub struct ServerUi {
    pub view: ServerView,
    pub log: LogBuffer,
    pub history: VecDeque<Sample>,
    pub props: Option<Properties>,
    pub props_error: Option<String>,
    /// Время самого свежего бэкапа в `backup_dir`: `None` — ещё не считали, `Some(None)` — бэкапов нет.
    pub newest_backup: Option<Option<SystemTime>>,
    /// Подсчёт `newest_backup` уже идёт в фоне.
    backup_scan: bool,
    /// Растёт при каждом сбросе `newest_backup`: ответ устаревшего подсчёта отбрасывается.
    backup_gen: u32,
}

impl std::ops::Deref for ServerUi {
    type Target = ServerView;
    fn deref(&self) -> &ServerView {
        &self.view
    }
}

impl ServerUi {
    fn new(view: ServerView) -> Self {
        let mut s = Self {
            view,
            log: LogBuffer::new(10_000),
            history: VecDeque::new(),
            props: None,
            props_error: None,
            newest_backup: None,
            backup_scan: false,
            backup_gen: 0,
        };
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

/// Где стрелки на вкладках с подвкладками («Доступ», «Дополнения»): ↓ спускает уровнем ниже, ↑ поднимает.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubLevel {
    /// ←→ листают вкладки окна, как на остальных вкладках.
    #[default]
    Tabs,
    /// ←→ листают подвкладки («Доступ»: белый список, операторы, баны, IP; «Дополнения»: плагины, датапаки).
    Lists,
    /// ↑↓ выбирают запись, ←→ по-прежнему листают подвкладки.
    Entries,
}

pub struct AccessView {
    pub list: List,
    pub level: SubLevel,
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
    /// Что умеет ядро сервера; определяется только при перечитывании списка.
    pub core: Option<AddonsCore>,
    loaded_for: Option<usize>,
}

/// Что ядро сервера принимает во вкладке «Дополнения».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonsCore {
    Plugins,
    Mods,
    Vanilla,
}

/// Пояснение для ванильного ядра: плагины и моды ему недоступны.
pub fn vanilla_addons_text() -> &'static str {
    tr!(
        "Ванильное ядро не поддерживает плагины и моды — только датапаки",
        "The vanilla core supports no plugins or mods — only datapacks"
    )
}

/// Подвкладка вкладки «Дополнения».
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AddonsSub {
    #[default]
    Plugins,
    Datapacks,
}

/// Где стрелки на вкладке «Дополнения».
pub struct AddonsNav {
    pub sub: AddonsSub,
    pub level: SubLevel,
}

pub struct DatapacksView {
    pub items: Vec<DatapackInfo>,
    pub sel: usize,
    pub updates: HashMap<PathBuf, UpdateInfo>,
    /// Версия на Modrinth по пути файла (известна после проверки обновлений).
    pub versions: HashMap<PathBuf, String>,
    pub checking: bool,
    pub checked: bool,
    pub busy: Option<String>,
    /// (id сервера, папка мира), для которых загружен список.
    loaded_for: Option<(usize, PathBuf)>,
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
    /// Команды, о которых окно уже сообщило своим уведомлением: ответ службы «отправлено: …» на них не показываем.
    announced: Vec<(String, Instant)>,
    pub should_quit: bool,
    pub exit_message: Option<String>,
    pub players_sel: usize,
    pub settings_sel: usize,
    pub backups: Vec<BackupInfo>,
    pub backups_sel: usize,
    backups_at: Option<Instant>,
    pub access: AccessView,
    pub plugins: PluginsView,
    /// Номер последнего запроса поиска дополнений (см. `PanelKind::PluginSearch::req`).
    pub(crate) search_gen: u64,
    pub addons: AddonsNav,
    pub datapacks: DatapacksView,
    pub world: WorldView,
    pub logs: LogsView,
    /// Высота области лога из последней отрисовки — для прыжков к совпадениям.
    pub logs_height: Cell<u16>,
    pub area: Rect,
    pub setup: Option<Setup>,
    pub config_path: PathBuf,
    /// Новая версия на GitHub, если есть.
    pub update_available: Option<String>,
    /// Где нарисован значок обновления (для клика).
    pub update_badge: Cell<Rect>,
    /// Идёт обновление pechka из окна.
    pub updating: bool,
    /// После выхода запустить окно заново из этого файла (обновлённая версия).
    pub reexec: Option<PathBuf>,
    pub language: Option<String>,
    /// Связь со службой установлена и пришёл первый снимок.
    pub connected: bool,
    pub got_state: bool,
    pub daemon_error: Option<String>,
    pub daemon_version: Option<String>,
    /// Служба умеет перезапуститься, не останавливая серверы (с 0.4).
    pub daemon_can_detach: bool,
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
            announced: vec![],
            should_quit: false,
            exit_message: None,
            players_sel: 0,
            settings_sel: 0,
            backups: vec![],
            backups_sel: 0,
            backups_at: None,
            access: AccessView {
                list: List::Whitelist,
                level: SubLevel::Tabs,
                entries: vec![],
                sel: 0,
                error: None,
                loaded: None,
                busy: false,
            },
            world: WorldView::default(),
            search_gen: 0,
            plugins: PluginsView {
                items: vec![],
                sel: 0,
                updates: HashMap::new(),
                checking: false,
                checked: false,
                busy: None,
                core: None,
                loaded_for: None,
            },
            addons: AddonsNav { sub: AddonsSub::Plugins, level: SubLevel::Tabs },
            datapacks: DatapacksView {
                items: vec![],
                sel: 0,
                updates: HashMap::new(),
                versions: HashMap::new(),
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
            update_badge: Cell::new(Rect::default()),
            updating: false,
            reexec: None,
            language,
            connected: false,
            got_state: false,
            daemon_error: None,
            daemon_version: None,
            daemon_outdated: false,
            version_check_pending: false,
            daemon_can_detach: false,
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

    /// Консольная команда, о которой окно сообщает своим, более подробным уведомлением: общая отметка
    /// службы «отправлено: <cmd>» его не затирает (ошибки показываются как обычно).
    fn send_announced(&mut self, id: usize, cmd: String) {
        self.announced.retain(|(_, at)| at.elapsed() < ANNOUNCED_FOR);
        self.announced.push((cmd.clone(), Instant::now()));
        self.send(Request::Command { id, cmd, user: false });
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
        let online = players_note(&s.players).map(|n| format!("{n}\n")).unwrap_or_default();
        if s.cfg.kind == Kind::Rcon && !restart {
            self.popup = Some(Popup::Confirm {
                title: tr!("Остановка", "Stop").into(),
                text: tr!(
                    format!("{online}Отправить stop на удалённый сервер «{}»?", s.cfg.name),
                    format!("{online}Send stop to the remote server “{}”?", s.cfg.name)
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
                        "{online}«{}» уже останавливается. Убить процесс (kill -9)? Несохранённые данные пропадут.",
                        s.cfg.name
                    ),
                    format!(
                        "{online}“{}” is already stopping. Kill the process (kill -9)? Unsaved data will be lost.",
                        s.cfg.name
                    )
                ),
                action: Action::Kill(id),
            });
            return;
        }
        // Игроки онлайн — их отключит: переспросить. Пустой сервер останавливается сразу.
        if s.running() && !s.players.is_empty() && s.status != Status::Stopping {
            let name = &s.cfg.name;
            let (title, question) = if restart {
                (
                    tr!("Перезапуск", "Restart"),
                    tr!(
                        format!("Перезапустить «{name}»? Игроков отключит, через минуту-две они смогут зайти снова."),
                        format!("Restart “{name}”? Players are disconnected and can rejoin in a minute or two.")
                    ),
                )
            } else {
                (
                    tr!("Остановка", "Stop"),
                    tr!(
                        format!("Остановить «{name}»? Игроков отключит."),
                        format!("Stop “{name}”? Players are disconnected.")
                    ),
                )
            };
            self.popup = Some(Popup::Confirm {
                title: title.into(),
                text: format!(
                    "{online}{question}{}",
                    tr!(
                        "\nПредупредить их можно в строке команды: say …",
                        "\nYou can warn them first from the command line: say …"
                    )
                ),
                action: Action::Stop { id, restart },
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

    /// Ctrl+U / клик по значку: предложить обновить pechka.
    pub fn self_update(&mut self) {
        if self.updating {
            return self.toast(tr!("обновление уже идёт…", "already updating…"), ToastKind::Info);
        }
        let Some(v) = self.update_available.clone() else {
            return self.toast(
                tr!(
                    format!("установлена последняя версия ({})", crate::update::VERSION),
                    format!("you have the latest version ({})", crate::update::VERSION)
                ),
                ToastKind::Info,
            );
        };
        let how = match crate::update::install_kind() {
            Ok(crate::update::Install::Homebrew) => {
                tr!("через Homebrew (brew upgrade)", "with Homebrew (brew upgrade)").to_string()
            }
            _ => tr!(
                "скачав сборку с GitHub и проверив её контрольную сумму",
                "by downloading the build from GitHub and checking its checksum"
            )
            .to_string(),
        };
        let servers = if self.daemon_can_detach {
            tr!(
                "Серверы продолжат работать: служба перезапустится на новой версии без их остановки.",
                "Servers keep running: the service restarts on the new version without stopping them."
            )
        } else {
            tr!(
                "Служба старой версии перезапустится, когда не будет запущенных серверов.",
                "The old service restarts once no servers are running."
            )
        };
        self.popup = Some(Popup::Confirm {
            title: tr!("Обновление pechka", "Update pechka").into(),
            text: tr!(
                format!(
                    "Обновить pechka {} → {v} {how}? Настройки сохранятся. {servers} Окно откроется заново.",
                    crate::update::VERSION
                ),
                format!(
                    "Update pechka {} → {v} {how}? Settings are kept. {servers} The window reopens.",
                    crate::update::VERSION
                )
            ),
            action: Action::SelfUpdate(v),
        });
    }

    fn run_self_update(&mut self, v: String) {
        self.updating = true;
        self.toast(tr!(format!("обновляю pechka до {v}…"), format!("updating pechka to {v}…")), ToastKind::Info);
        self.spawn_task(
            move || crate::update::update_to(&v).map(|p| (p, v)),
            |app, res| {
                app.updating = false;
                match res {
                    Ok((path, v)) => {
                        // Новое окно увидит службу старой версии и само перезапустит её (без остановки серверов).
                        app.reexec = Some(path);
                        app.exit_message =
                            Some(tr!(format!("pechka обновлена до {v}."), format!("pechka updated to {v}.")));
                        app.should_quit = true;
                    }
                    Err(e) => app.toast(
                        tr!(format!("обновление не удалось: {e:#}"), format!("update failed: {e:#}")),
                        ToastKind::Error,
                    ),
                }
            },
        );
    }

    fn run_action(&mut self, a: Action) {
        match a {
            Action::Stop { id, restart } => self.send(Request::Stop { id, restart }),
            Action::Kill(id) => self.send(Request::Kill { id }),
            Action::RemoteStop(id) => self.send(Request::Stop { id, restart: false }),
            Action::Command { id, cmd } => self.send(Request::Command { id, cmd, user: false }),
            Action::DeleteBackup(p) => match crate::backup::delete_path(&p) {
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
            Action::ReloadDatapacks(id) => self.reload_datapacks(id),
            Action::EnableDatapackInGame(id, d) => self.enable_datapack_in_game(id, &d),
            Action::DatapackToggle(path) => self.toggle_datapack_at(path),
            Action::ApplyDatapackUpdates(paths) => self.apply_datapack_updates(paths),
            Action::InstallPlugin(plan) => self.install_plugin(*plan),
            Action::ResetDimension(d) => self.reset_dimension(d),
            Action::AskOverworldName => self.ask_overworld_name(),
            Action::SetLevel(name) => self.set_level(name),
            Action::ApplyCore(t) => self.apply_core(*t),
            Action::SelfUpdate(v) => self.run_self_update(v),
            Action::CloneServer(p) => self.run_clone(*p),
            Action::JvmApply(c) => self.apply_jvm(c),
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
        if let Err(e) = crate::propdocs::validate(key, value) {
            return self.toast(format!("{key}: {e}"), ToastKind::Error);
        }
        let s = self.sel_mut();
        let Some(props) = &mut s.props else { return };
        props.set(key, value);
        self.save_props(tr!(format!("{key} = {value} — сохранено"), format!("{key} = {value} — saved")));
    }

    // ───────────────────────────── бэкапы ─────────────────────────────

    pub fn refresh_backups(&mut self) {
        self.backups_at = Some(Instant::now());
        if !self.has_server() {
            self.backups.clear();
            return;
        }
        let s = self.sel();
        let local = s.is_local();
        self.backups =
            if local { backup::list(&s.cfg.backup_dir(), &backup::file_prefix(&s.cfg.name)) } else { vec![] };
        self.backups_sel = self.backups_sel.min(self.backups.len().saturating_sub(1));
        if local {
            let newest = backup_health::newest_of(&self.backups);
            self.sel_mut().newest_backup = Some(newest);
        }
    }

    // ───────────────────────────── выбор сервера и вкладок ─────────────────────────────

    fn on_select_changed(&mut self) {
        self.players_sel = 0;
        self.settings_sel = 0;
        self.backups_sel = 0;
        self.access.sel = 0;
        self.access.level = SubLevel::Tabs;
        self.plugins.sel = 0;
        self.datapacks.sel = 0;
        self.addons = AddonsNav { sub: AddonsSub::Plugins, level: SubLevel::Tabs };
        self.world.sel = 0;
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
            Tab::Addons => {
                self.refresh_plugins(force);
                self.refresh_datapacks(force);
            }
            Tab::World => self.refresh_world(force),
            _ => {}
        }
    }

    /// Единственное место смены фокуса. Уходя с содержимого, уровни подвкладок сбрасываются: вернувшись,
    /// ←→ снова листают вкладки окна (правило стрелок), а не подвкладки.
    fn set_focus(&mut self, f: Focus) {
        if f != Focus::Content {
            self.access.level = SubLevel::Tabs;
            self.addons.level = SubLevel::Tabs;
        }
        self.focus = f;
    }

    fn set_tab(&mut self, t: Tab) {
        self.tab = t;
        self.access.level = SubLevel::Tabs;
        self.addons = AddonsNav { sub: AddonsSub::Plugins, level: SubLevel::Tabs };
        self.on_tab_shown(false);
    }
}

/// Сколько помнить команду, о которой окно уже сообщило своим уведомлением (см. [`App::send_announced`]).
const ANNOUNCED_FOR: Duration = Duration::from_secs(5);

/// Отметка службы «отправлено: <cmd>» / «sent: <cmd>» (core.rs, ответ на `Request::Command` с `user: false`)
/// о команде из `list`, отправленной меньше [`ANNOUNCED_FOR`] назад. Совпавшая запись снимается (одна отметка —
/// одна команда), устаревшие — тоже. Служба может говорить на другом языке, поэтому понимаем оба.
fn take_announced(list: &mut Vec<(String, Instant)>, text: &str, now: Instant) -> bool {
    list.retain(|(_, at)| now.duration_since(*at) < ANNOUNCED_FOR);
    let Some(cmd) = text.strip_prefix("sent: ").or_else(|| text.strip_prefix("отправлено: ")) else {
        return false;
    };
    match list.iter().position(|(c, _)| c == cmd) {
        Some(i) => {
            list.remove(i);
            true
        }
        None => false,
    }
}

#[cfg(all(test, not(windows)))]
impl App {
    /// Окно без службы (тесты): будто служба поздоровалась и прислала снимок `servers`.
    pub(crate) fn for_test(config: PathBuf, client: Client, servers: Vec<ServerView>) -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(config.clone(), client, tx);
        app.on_event(AppEvent::Daemon(Box::new(Msg::Welcome {
            version: crate::update::VERSION.into(),
            protocol: crate::proto::PROTOCOL,
            pid: 1,
            config_path: config,
            language: None,
            can_detach: true,
        })));
        app.on_event(AppEvent::Daemon(Box::new(Msg::State { servers })));
        app
    }
}

/// «Сейчас на сервере 2 игрока: Alex, Steve.» — для подтверждений; `None`, если никого.
pub fn players_note(players: &[String]) -> Option<String> {
    const SHOWN: usize = 5;
    let n = players.len();
    if n == 0 {
        return None;
    }
    let mut names = players.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if n > SHOWN {
        names += &tr!(format!(" и ещё {}", n - SHOWN), format!(" and {} more", n - SHOWN));
    }
    let word = match (n % 10, n % 100) {
        (1, r) if r != 11 => "игрок",
        (2..=4, r) if !(12..=14).contains(&r) => "игрока",
        _ => "игроков",
    };
    Some(tr!(
        format!("Сейчас на сервере {n} {word}: {names}."),
        if n == 1 { format!("1 player is online: {names}.") } else { format!("{n} players are online: {names}.") }
    ))
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
    fn players_note_text() {
        use crate::i18n::{Lang, with_lang};
        let p = |n: usize| (1..=n).map(|i| format!("P{i}")).collect::<Vec<_>>();
        assert_eq!(players_note(&[]), None);
        with_lang(Lang::Ru, || {
            assert_eq!(players_note(&p(1)).unwrap(), "Сейчас на сервере 1 игрок: P1.");
            assert_eq!(players_note(&p(3)).unwrap(), "Сейчас на сервере 3 игрока: P1, P2, P3.");
            assert_eq!(players_note(&p(7)).unwrap(), "Сейчас на сервере 7 игроков: P1, P2, P3, P4, P5 и ещё 2.");
            assert!(players_note(&p(11)).unwrap().contains("11 игроков"));
            assert!(players_note(&p(21)).unwrap().contains("21 игрок:"));
            assert!(players_note(&p(12)).unwrap().contains("12 игроков"));
        });
        with_lang(Lang::En, || {
            assert_eq!(players_note(&p(1)).unwrap(), "1 player is online: P1.");
            assert_eq!(players_note(&p(2)).unwrap(), "2 players are online: P1, P2.");
        });
    }

    #[test]
    fn announced_commands_match_exactly_and_expire() {
        let t0 = Instant::now();
        let mut list = vec![("minecraft:reload".to_string(), t0)];
        assert!(!take_announced(&mut list, "sent: reload", t0));
        assert!(!take_announced(&mut list, "sent: minecraft:reload now", t0));
        assert!(take_announced(&mut list, "sent: minecraft:reload", t0 + Duration::from_secs(1)));
        assert!(list.is_empty(), "одна отметка — одна команда");
        list.push(("minecraft:reload".into(), t0));
        assert!(!take_announced(&mut list, "sent: minecraft:reload", t0 + ANNOUNCED_FOR));
        assert!(list.is_empty(), "устаревшие записи снимаются");
    }

    #[test]
    fn formatting() {
        use crate::i18n::{Lang, with_lang};
        with_lang(Lang::Ru, || assert_eq!(human_duration(Duration::from_secs(3725)), "1ч 2м"));
        with_lang(Lang::En, || assert_eq!(human_duration(Duration::from_secs(3725)), "1h 2m"));
    }
}
