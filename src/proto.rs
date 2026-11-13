//! Протокол между фоновой службой (`mctui daemon`) и окном TUI.
//!
//! Транспорт — TCP на 127.0.0.1, по одному JSON-объекту на строку. Порт и случайный токен служба
//! пишет в `daemon.json` рядом с servers.toml (доступен только владельцу); первым сообщением
//! клиент отправляет `Hello` с этим токеном.

use crate::config::ServerConfig;
use crate::server::Status;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PROTOCOL: u32 = 1;

/// Клиент → служба.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Request {
    Hello {
        token: String,
        version: String,
        protocol: u32,
    },
    Start {
        id: usize,
    },
    Stop {
        id: usize,
        restart: bool,
    },
    Kill {
        id: usize,
    },
    /// `user` — команда из строки ввода (ответ в панели «Ответ»), иначе действие из UI (ответ всплывашкой).
    Command {
        id: usize,
        cmd: String,
        user: bool,
    },
    Backup {
        id: usize,
    },
    Restore {
        id: usize,
        path: PathBuf,
    },
    /// server.properties изменён окном — перечитать (и пометить «нужен перезапуск», если сервер работает).
    PropsChanged {
        id: usize,
    },
    AddServer {
        cfg: Box<ServerConfig>,
    },
    UpdateServer {
        id: usize,
        cfg: Box<ServerConfig>,
    },
    RemoveServer {
        id: usize,
    },
    SetLanguage {
        language: Option<String>,
    },
    /// Штатно остановить все серверы (служба продолжает работать).
    StopAll,
    /// Остановить все серверы и завершить службу.
    Shutdown,
    /// Завершить службу, оставив серверы работать (с 0.4: их держат хранители процессов).
    Detach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeKind {
    Info,
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RconView {
    Off,
    Connecting,
    Connected,
    Error(String),
}

/// Точка графика (раз в минуту).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Sample {
    /// Unix-время, секунды.
    pub t: i64,
    pub players: u16,
    pub tps: Option<f32>,
    pub mem_mb: Option<u32>,
    pub cpu: Option<f32>,
}

/// Снимок состояния сервера для окна.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerView {
    pub id: usize,
    pub cfg: ServerConfig,
    pub status: Status,
    pub pid: Option<u32>,
    pub rcon: RconView,
    pub players: Vec<String>,
    pub players_from_rcon: bool,
    /// Ники операторов в нижнем регистре.
    pub ops: Vec<String>,
    pub max_players: Option<u32>,
    pub tps: Option<[f64; 3]>,
    pub cpu: Option<f32>,
    pub mem: Option<u64>,
    pub cpu_hist: Vec<u64>,
    pub started_unix: Option<i64>,
    pub version: Option<String>,
    pub last_exit: Option<String>,
    pub backup_progress: Option<String>,
    pub last_backup: Option<String>,
    pub needs_restart: bool,
    pub response: Vec<String>,
    /// Секунд до авто-рестарта после падения.
    pub restart_in: Option<u64>,
    pub next_backup_unix: Option<i64>,
    pub next_restart_unix: Option<i64>,
    pub crash_gave_up: bool,
    /// Активные предупреждения (низкий TPS, память, диск…).
    #[serde(default)]
    pub alerts: Vec<String>,
}

impl ServerView {
    pub fn running(&self) -> bool {
        self.pid.is_some()
    }

    pub fn is_local(&self) -> bool {
        self.cfg.kind == crate::config::Kind::Local
    }

    pub fn is_op(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        self.ops.contains(&n)
    }
}

/// Служба → клиент.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Msg {
    Welcome {
        version: String,
        protocol: u32,
        pid: u32,
        config_path: PathBuf,
        language: Option<String>,
        /// Служба умеет уйти, не останавливая серверы ([`Request::Detach`]); у служб до 0.4 — false.
        #[serde(default)]
        can_detach: bool,
    },
    Denied {
        reason: String,
    },
    State {
        servers: Vec<ServerView>,
    },
    /// Новые строки вывода. `reset` — заменить буфер целиком (при подключении).
    Log {
        id: usize,
        lines: Vec<String>,
        reset: bool,
    },
    History {
        id: usize,
        samples: Vec<Sample>,
    },
    Sample {
        id: usize,
        sample: Sample,
    },
    Notice {
        text: String,
        kind: NoticeKind,
    },
}

/// Содержимое daemon.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
}

pub fn endpoint_path(config_path: &Path) -> PathBuf {
    crate::config::state_dir(config_path).join("daemon.json")
}

pub fn encode<T: Serialize>(v: &T) -> String {
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let r = Request::Command { id: 3, cmd: "say hi".into(), user: true };
        let s = encode(&r);
        assert!(s.ends_with('\n'));
        match serde_json::from_str::<Request>(s.trim()).unwrap() {
            Request::Command { id, cmd, user } => assert_eq!((id, cmd.as_str(), user), (3, "say hi", true)),
            _ => panic!(),
        }
        let m = Msg::Notice { text: "ok".into(), kind: NoticeKind::Ok };
        assert!(matches!(serde_json::from_str::<Msg>(encode(&m).trim()).unwrap(), Msg::Notice { .. }));
    }
}
