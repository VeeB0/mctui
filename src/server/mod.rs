pub mod local;
pub mod rcon;
pub mod rcon_task;
pub mod stats;
pub mod supervisor;

use crate::config::{Kind, ServerConfig};
use crate::log::LogBuffer;
use crate::properties::Properties;
use local::LocalProc;
use rcon_task::RconHandle;
use std::collections::{BTreeSet, VecDeque};
use std::time::Instant;
use supervisor::CrashPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Offline,
    Starting,
    Running,
    Stopping,
    Crashed,
    Restarting,
    // RCON-серверы
    Connecting,
    Online,
    Unreachable,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Offline => "остановлен",
            Status::Starting => "запускается",
            Status::Running => "работает",
            Status::Stopping => "останавливается",
            Status::Crashed => "упал",
            Status::Restarting => "перезапуск…",
            Status::Connecting => "подключение…",
            Status::Online => "онлайн",
            Status::Unreachable => "недоступен",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RconState {
    Off,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayersSource {
    Log,
    Rcon,
}

pub struct ServerState {
    pub id: usize,
    pub cfg: ServerConfig,
    pub status: Status,
    pub log: LogBuffer,
    pub proc: Option<LocalProc>,
    pub rcon: Option<RconHandle>,
    pub rcon_state: RconState,
    pub players: BTreeSet<String>,
    pub players_source: PlayersSource,
    pub max_players: Option<u32>,
    pub tps: Option<[f64; 3]>,
    pub cpu: Option<f32>,
    pub mem: Option<u64>,
    pub cpu_hist: VecDeque<u64>,
    pub started_at: Option<Instant>,
    pub version: Option<String>,
    pub user_stop: bool,
    pub restart_after_stop: bool,
    pub stop_deadline: Option<Instant>,
    pub restart_at: Option<Instant>,
    pub crash: CrashPolicy,
    pub last_exit: Option<String>,
    pub backup_progress: Option<String>,
    pub last_backup: Option<String>,
    pub needs_restart: bool,
    pub props: Option<Properties>,
    pub props_error: Option<String>,
    pub response: Vec<String>,
    pub response_until: Option<Instant>,
    pub last_poll: Option<Instant>,
}

impl ServerState {
    pub fn new(cfg: ServerConfig, id: usize) -> Self {
        let status = if cfg.kind == Kind::Rcon { Status::Connecting } else { Status::Offline };
        let mut s = Self {
            id,
            cfg,
            status,
            log: LogBuffer::new(10_000),
            proc: None,
            rcon: None,
            rcon_state: RconState::Off,
            players: BTreeSet::new(),
            players_source: PlayersSource::Log,
            max_players: None,
            tps: None,
            cpu: None,
            mem: None,
            cpu_hist: VecDeque::new(),
            started_at: None,
            version: None,
            user_stop: false,
            restart_after_stop: false,
            stop_deadline: None,
            restart_at: None,
            crash: CrashPolicy::default(),
            last_exit: None,
            backup_progress: None,
            last_backup: None,
            needs_restart: false,
            props: None,
            props_error: None,
            response: vec![],
            response_until: None,
            last_poll: None,
        };
        s.reload_props();
        s
    }

    pub fn is_local(&self) -> bool {
        self.cfg.kind == Kind::Local
    }

    pub fn reload_props(&mut self) {
        let Some(path) = self.cfg.properties_path() else {
            return;
        };
        match Properties::load(&path) {
            Ok(p) => {
                self.max_players = self.max_players.or_else(|| p.get("max-players").and_then(|v| v.parse().ok()));
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

    /// Сброс runtime-данных при выходе процесса.
    pub fn clear_runtime(&mut self) {
        self.proc = None;
        self.rcon = None;
        self.rcon_state = RconState::Off;
        self.players.clear();
        self.players_source = PlayersSource::Log;
        self.tps = None;
        self.cpu = None;
        self.mem = None;
        self.cpu_hist.clear();
        self.started_at = None;
        self.stop_deadline = None;
    }
}
