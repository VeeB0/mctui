//! События от фоновых задач к UI.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReqKind {
    /// Команда, введённая пользователем — ответ показывается в панели «Ответ».
    User,
    /// Действие из UI (kick/ban/op) — ответ показывается всплывающим сообщением.
    Action,
    PollList,
    PollTps,
}

#[derive(Debug)]
pub enum RconEvent {
    Connected,
    Disconnected(String),
    Response { kind: ReqKind, cmd: String, result: Result<String, String> },
}

#[derive(Debug)]
pub enum BackupEvent {
    Progress(String),
    Done { path: PathBuf, bytes: u64, warning: Option<String> },
    Failed(String),
}

/// `id` — постоянный идентификатор сервера (индекс в списке меняется при удалении).
#[derive(Debug)]
pub enum AppEvent {
    /// Строка stdout/stderr локального сервера.
    Line {
        id: usize,
        line: String,
    },
    Exited {
        id: usize,
        pid: u32,
        status: String,
    },
    Rcon {
        id: usize,
        generation: u64,
        ev: RconEvent,
    },
    Backup {
        id: usize,
        ev: BackupEvent,
    },
    /// Вышла новая версия mctui.
    UpdateAvailable(String),
}
