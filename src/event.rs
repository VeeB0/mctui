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

#[derive(Debug)]
pub enum RestoreEvent {
    Progress(String),
    Done { stash: PathBuf },
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
    Restore {
        id: usize,
        ev: RestoreEvent,
    },
    /// Вышла новая версия pechka.
    UpdateAvailable(String),
    /// Сообщение от фоновой службы (в окне TUI).
    Daemon(Box<crate::proto::Msg>),
    /// Связь со службой потеряна / не установлена.
    DaemonLost(String),
    /// Результат фоновой работы окна (список версий, создание сервера, проверка плагинов…).
    Ui(UiTask),
}

/// Действие над окном из фонового потока.
pub struct UiTask(pub Box<dyn FnOnce(&mut crate::app::App) + Send>);

impl std::fmt::Debug for UiTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UiTask")
    }
}
