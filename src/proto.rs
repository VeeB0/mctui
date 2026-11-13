//! Протокол между фоновой службой (`pechka daemon`) и окном TUI.
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

    /// Проводной формат: окно одной версии говорит со службой другой (обновление, откат), а `Detach`
    /// и `Hello` старые службы и окна понимают по этим именам. Переименование тега или поля ломает их —
    /// тогда этот тест должен упасть, и менять формат можно только вместе с [`PROTOCOL`].
    #[test]
    fn wire_format_is_stable() {
        assert_eq!(PROTOCOL, 1);
        let hello = Request::Hello { token: "t".into(), version: "0.4.0".into(), protocol: 1 };
        assert_eq!(encode(&hello), "{\"t\":\"Hello\",\"token\":\"t\",\"version\":\"0.4.0\",\"protocol\":1}\n");
        assert_eq!(encode(&Request::Detach), "{\"t\":\"Detach\"}\n");
        assert_eq!(encode(&Request::Shutdown), "{\"t\":\"Shutdown\"}\n");
        assert_eq!(encode(&Request::StopAll), "{\"t\":\"StopAll\"}\n");
        assert_eq!(encode(&Request::Stop { id: 2, restart: true }), "{\"t\":\"Stop\",\"id\":2,\"restart\":true}\n");
        assert_eq!(
            encode(&Request::Command { id: 1, cmd: "list".into(), user: false }),
            "{\"t\":\"Command\",\"id\":1,\"cmd\":\"list\",\"user\":false}\n"
        );
        assert_eq!(encode(&Msg::Denied { reason: "x".into() }), "{\"t\":\"Denied\",\"reason\":\"x\"}\n");
        assert_eq!(
            encode(&Msg::Log { id: 0, lines: vec!["a".into()], reset: true }),
            "{\"t\":\"Log\",\"id\":0,\"lines\":[\"a\"],\"reset\":true}\n"
        );
        let welcome = Msg::Welcome {
            version: "0.4.0".into(),
            protocol: 1,
            pid: 7,
            config_path: "/c/servers.toml".into(),
            language: Some("ru".into()),
            can_detach: true,
        };
        assert_eq!(
            encode(&welcome),
            "{\"t\":\"Welcome\",\"version\":\"0.4.0\",\"protocol\":1,\"pid\":7,\"config_path\":\"/c/servers.toml\",\
             \"language\":\"ru\",\"can_detach\":true}\n"
        );
    }

    /// Служба 0.3 не знает `can_detach`: окно должно считать, что отпустить серверы она не умеет.
    #[test]
    fn welcome_from_0_3_daemon() {
        let m: Msg = serde_json::from_str(
            r#"{"t":"Welcome","version":"0.3.0","protocol":1,"pid":42,"config_path":"/x/servers.toml","language":null}"#,
        )
        .unwrap();
        match m {
            Msg::Welcome { version, can_detach, language, .. } => {
                assert_eq!(version, "0.3.0");
                assert!(!can_detach);
                assert_eq!(language, None);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Снимок сервера от службы 0.2 (без `alerts`) читается новым окном.
    #[test]
    fn server_view_without_alerts() {
        let cfg = ServerConfig::new_rcon("A", "example.org", 25575, "p");
        let v = crate::server::ServerState::new(cfg, 3).view();
        let mut j = serde_json::to_value(&v).unwrap();
        assert!(j.as_object_mut().unwrap().remove("alerts").is_some());
        let back: ServerView = serde_json::from_value(j).unwrap();
        assert_eq!((back.id, back.cfg.name.as_str()), (3, "A"));
        assert!(back.alerts.is_empty());
    }

    /// Поля, добавленные более новой версией, старая пропускает; незнакомый тип сообщения не
    /// разбирается — окно и служба такие строки молча пропускают, а не рвут связь.
    #[test]
    fn newer_peer_is_tolerated() {
        let m: Msg = serde_json::from_str(r#"{"t":"Notice","text":"hi","kind":"Ok","future":1}"#).unwrap();
        assert!(matches!(m, Msg::Notice { kind: NoticeKind::Ok, .. }));
        let r: Request = serde_json::from_str(r#"{"t":"Start","id":1,"future":true}"#).unwrap();
        assert!(matches!(r, Request::Start { id: 1 }));
        assert!(serde_json::from_str::<Msg>(r#"{"t":"FromTheFuture","x":1}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"t":"FromTheFuture"}"#).is_err());
    }

    #[test]
    fn endpoint_is_next_to_config() {
        let p = endpoint_path(Path::new("/home/u/.config/pechka/servers.toml"));
        assert_eq!(p, Path::new("/home/u/.config/pechka/daemon.json"));
    }
}
