//! Ядро фоновой службы: серверы, процессы, RCON, бэкапы, расписания, история для графиков.
//! Окно TUI управляет им через [`crate::proto::Request`] и получает снимки состояния.

use crate::backup::{self, BackupJob};
use crate::config::{Config, Kind, ServerConfig};
use crate::event::{AppEvent, BackupEvent, RconEvent, ReqKind, RestoreEvent};
use crate::proto::{NoticeKind, Request, Sample};
use crate::server::local::{Found, LocalProc};
use crate::server::stats::{self, PlayerEvent};
use crate::server::{PlayersSource, RconState, ServerState, Status, rcon_task};
use crate::tr;
use chrono::{Local, TimeZone};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const POLL_EVERY: Duration = Duration::from_secs(10);
const RESPONSE_WINDOW: Duration = Duration::from_millis(1500);
const SAMPLE_EVERY: Duration = Duration::from_secs(60);
const HISTORY_CAP: usize = 24 * 60;
const HISTORY_SAVE_EVERY: Duration = Duration::from_secs(600);
/// За сколько секунд до рестарта по расписанию предупреждать игроков.
const RESTART_WARNINGS: [u64; 10] = [900, 300, 60, 30, 10, 5, 4, 3, 2, 1];

/// Что накопилось для рассылки клиентам.
#[derive(Default)]
pub struct Outbox {
    pub notices: Vec<(String, NoticeKind)>,
    pub lines: Vec<(usize, String)>,
    pub samples: Vec<(usize, Sample)>,
    /// Состояние изменилось — разослать новый снимок.
    pub dirty: bool,
}

pub struct Manager {
    pub servers: Vec<ServerState>,
    pub config_path: PathBuf,
    pub language: Option<String>,
    /// Идёт остановка всех серверов перед выходом службы.
    pub quitting: bool,
    /// Служба уходит, оставляя серверы работать (перезапуск службы / обновление mctui).
    pub detaching: bool,
    pub should_exit: bool,
    pub out: Outbox,
    next_id: usize,
    rcon_seq: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
    sys: Option<sysinfo::System>,
    last_history_save: Instant,
    last_disk_check: Option<Instant>,
}

impl Manager {
    pub fn new(cfg: Config, config_path: PathBuf, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        let servers: Vec<ServerState> =
            cfg.servers.into_iter().enumerate().map(|(i, c)| ServerState::new(c, i + 1)).collect();
        let mut m = Self {
            next_id: servers.len() + 1,
            servers,
            config_path,
            language: cfg.language,
            quitting: false,
            detaching: false,
            should_exit: false,
            out: Outbox { dirty: true, ..Default::default() },
            rcon_seq: 0,
            tx,
            sys: sysinfo::System::new().ok(),
            last_history_save: Instant::now(),
            last_disk_check: None,
        };
        for i in 0..m.servers.len() {
            m.load_history(i);
            m.load_sessions(i);
            if m.servers[i].cfg.kind == Kind::Rcon {
                m.connect_rcon(i);
            } else if !m.attach_running(i) && m.servers[i].cfg.autostart {
                m.start(i);
            }
        }
        m
    }

    fn idx(&self, id: usize) -> Option<usize> {
        self.servers.iter().position(|s| s.id == id)
    }

    fn notice(&mut self, text: impl Into<String>, kind: NoticeKind) {
        self.out.notices.push((text.into(), kind));
    }

    /// Строка в лог сервера (и клиентам).
    fn log(&mut self, i: usize, line: &str) {
        let s = &mut self.servers[i];
        s.log.push(line);
        self.out.lines.push((s.id, line.to_string()));
    }

    // ───────────────────────────── запросы окна ─────────────────────────────

    pub fn handle(&mut self, req: Request) {
        self.out.dirty = true;
        let by_id = |m: &Self, id: usize| m.idx(id);
        match req {
            Request::Hello { .. } => {}
            Request::Start { id } => {
                if let Some(i) = by_id(self, id) {
                    self.servers[i].crash.reset();
                    self.servers[i].crash_gave_up = false;
                    self.start(i)
                }
            }
            Request::Stop { id, restart } => {
                if let Some(i) = by_id(self, id) {
                    self.stop(i, restart)
                }
            }
            Request::Kill { id } => {
                if let Some(i) = by_id(self, id)
                    && let Some(p) = &mut self.servers[i].proc
                {
                    p.kill();
                }
            }
            Request::Command { id, cmd, user } => {
                let Some(i) = by_id(self, id) else { return };
                let kind = if user { ReqKind::User } else { ReqKind::Action };
                match self.send_command(i, &cmd, kind) {
                    Ok(()) if !user => {
                        self.notice(tr!(format!("отправлено: {cmd}"), format!("sent: {cmd}")), NoticeKind::Ok)
                    }
                    Ok(()) => {}
                    Err(e) if user => {
                        self.servers[i].response =
                            vec![format!("> {}", cmd.trim()), tr!(format!("ошибка: {e}"), format!("error: {e}"))]
                    }
                    Err(e) => self.notice(e, NoticeKind::Error),
                }
            }
            Request::Backup { id } => {
                if let Some(i) = by_id(self, id) {
                    self.backup(i, false)
                }
            }
            Request::Restore { id, path } => {
                if let Some(i) = by_id(self, id) {
                    self.restore(i, path)
                }
            }
            Request::PropsChanged { id } => {
                if let Some(i) = by_id(self, id) {
                    let s = &mut self.servers[i];
                    s.reload_props();
                    if s.proc.is_some() {
                        s.needs_restart = true;
                    }
                }
            }
            Request::AddServer { cfg } => self.add_server(*cfg),
            Request::UpdateServer { id, cfg } => {
                if let Some(i) = by_id(self, id) {
                    self.update_server(i, *cfg)
                }
            }
            Request::RemoveServer { id } => {
                if let Some(i) = by_id(self, id) {
                    self.remove_server(i)
                }
            }
            Request::SetLanguage { language } => {
                crate::i18n::init(language.as_deref());
                self.language = language;
                self.save_config();
            }
            Request::StopAll => {
                for i in 0..self.servers.len() {
                    self.servers[i].restart_at = None;
                    if self.servers[i].proc.is_some() && self.servers[i].status != Status::Stopping {
                        self.stop(i, false);
                    }
                }
            }
            Request::Shutdown => self.quit_stop_all(),
            Request::Detach => self.detach(),
        }
    }

    /// Каталог файлов состояния хранителей процессов.
    fn run_dir(&self) -> PathBuf {
        crate::server::local::run_dir(&self.config_path)
    }

    /// Подключиться к серверу, который продолжает работать после прошлой службы.
    fn attach_running(&mut self, i: usize) -> bool {
        let run_dir = self.run_dir();
        let s = &mut self.servers[i];
        match LocalProc::attach(s.id, &s.cfg, &run_dir, self.tx.clone()) {
            Found::Nothing => false,
            Found::Ended(status) => {
                s.last_exit = Some(status.clone());
                self.log(
                    i,
                    &tr!(
                        format!("mctui: сервер завершился, пока служба не работала ({status})"),
                        format!("mctui: the server exited while the service was down ({status})")
                    ),
                );
                false
            }
            Found::Running(a) => {
                let a = *a;
                for l in &a.tail {
                    let line = s.log.push(l).clone();
                    if let Some(v) = stats::version(&line.text) {
                        s.version = Some(v);
                    }
                    s.note_op_text(&line.text);
                    match stats::player_event(&line.text) {
                        Some(PlayerEvent::Joined(n)) => {
                            s.players.insert(n);
                        }
                        Some(PlayerEvent::Left(n)) => {
                            s.players.remove(&n);
                        }
                        None => {}
                    }
                }
                // Сессии, открытые прошлой службой, закрыты по её последнему пульсу — открываем заново.
                if let Some(log) = &mut s.sessions {
                    for n in &s.players {
                        log.record_join(n, Local::now());
                    }
                }
                let ago = (Local::now().timestamp() - a.started).max(0) as u64;
                let pid = a.proc.pid;
                s.proc = Some(a.proc);
                s.status = if a.ready { Status::Running } else { Status::Starting };
                s.started_unix = Some(a.started);
                s.started_at = Instant::now().checked_sub(Duration::from_secs(ago)).or(Some(Instant::now()));
                s.user_stop = false;
                s.reload_props();
                self.log(
                    i,
                    &tr!(
                        format!("──────── mctui: служба подключилась к работающему серверу (pid {pid}) ────────"),
                        format!("──────── mctui: the service reattached to the running server (pid {pid}) ────────")
                    ),
                );
                if a.ready {
                    self.connect_rcon(i);
                    self.update_schedule(i);
                }
                true
            }
        }
    }

    /// Завершить службу, не останавливая серверы. Дожидается конца бэкапов и восстановлений.
    pub fn detach(&mut self) {
        if self.quitting {
            return;
        }
        self.detaching = true;
        self.notice(
            tr!(
                "служба перезапускается, серверы продолжают работать",
                "the service is restarting; servers keep running"
            ),
            NoticeKind::Info,
        );
        self.check_detach();
    }

    fn check_detach(&mut self) {
        // Остановка и авто-рестарт идут в этой службе — доводим их до конца, иначе новая служба примет
        // штатную остановку за падение.
        if self.detaching
            && self.servers.iter().all(|s| {
                s.backup_progress.is_none()
                    && !s.restoring
                    && !matches!(s.status, Status::Stopping | Status::Restarting)
            })
        {
            self.save_history();
            self.should_exit = true;
        }
    }

    // ───────────────────────────── серверные операции ─────────────────────────────

    fn connect_rcon(&mut self, idx: usize) {
        let s = &mut self.servers[idx];
        let (host, port, password) = match s.cfg.kind {
            Kind::Rcon => match s.cfg.rcon_password() {
                Some(p) => (s.cfg.host.clone(), s.cfg.port, p),
                None => {
                    s.status = Status::Unreachable;
                    s.rcon_state = RconState::Error(
                        tr!("не задан пароль (password / password_env)", "no password (password / password_env)")
                            .into(),
                    );
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
                    s.rcon_state = RconState::Error(tr!("rcon.password пустой", "rcon.password is empty").into());
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
            self.notice(tr!("переподключение RCON…", "reconnecting RCON…"), NoticeKind::Info);
            return;
        }
        if s.proc.is_some() {
            let msg = tr!(format!("{} уже запущен", s.cfg.name), format!("{} is already running", s.cfg.name));
            self.notice(msg, NoticeKind::Info);
            return;
        }
        if s.restoring {
            self.notice(tr!("идёт восстановление из бэкапа", "restore in progress"), NoticeKind::Info);
            return;
        }
        let run_dir = crate::server::local::run_dir(&self.config_path);
        match LocalProc::spawn(s.id, &s.cfg, &run_dir, self.tx.clone()) {
            Ok(p) => {
                let line = tr!(
                    format!("──────── mctui: запуск {} (pid {}) ────────", s.cfg.command.join(" "), p.pid),
                    format!("──────── mctui: starting {} (pid {}) ────────", s.cfg.command.join(" "), p.pid)
                );
                s.proc = Some(p);
                s.status = Status::Starting;
                s.started_at = Some(Instant::now());
                s.started_unix = Some(Local::now().timestamp());
                s.user_stop = false;
                s.restart_after_stop = false;
                s.restart_at = None;
                s.needs_restart = false;
                s.played_since_backup = false;
                s.players.clear();
                s.reload_props();
                let name = s.cfg.name.clone();
                self.log(idx, &line);
                self.notice(tr!(format!("{name}: запуск"), format!("{name}: starting")), NoticeKind::Info);
            }
            Err(e) => {
                s.status = Status::Crashed;
                let msg = format!("{e:#}");
                self.log(idx, &format!("mctui: {msg}"));
                self.notice(msg, NoticeKind::Error);
            }
        }
    }

    pub fn stop(&mut self, idx: usize, restart: bool) {
        let s = &mut self.servers[idx];
        if s.cfg.kind == Kind::Rcon {
            if restart {
                self.notice(
                    tr!(
                        "перезапуск удалённого сервера по RCON не поддерживается",
                        "restarting a remote server over RCON is not supported"
                    ),
                    NoticeKind::Error,
                );
            } else if let Err(e) = self.send_command(idx, "stop", ReqKind::Action) {
                self.notice(e, NoticeKind::Error);
            }
            return;
        }
        if s.status == Status::Restarting {
            s.restart_at = None;
            s.status = Status::Offline;
            self.notice(tr!("авто-рестарт отменён", "auto-restart cancelled"), NoticeKind::Info);
            return;
        }
        let Some(p) = &s.proc else {
            if restart {
                self.start(idx);
            } else {
                self.notice(tr!("сервер не запущен", "server is not running"), NoticeKind::Info);
            }
            return;
        };
        if s.status == Status::Stopping {
            self.notice(tr!("сервер уже останавливается", "server is already stopping"), NoticeKind::Info);
            return;
        }
        p.send("stop");
        s.user_stop = true;
        s.restart_after_stop = restart;
        s.status = Status::Stopping;
        s.stop_deadline = Some(Instant::now() + Duration::from_secs(s.cfg.stop_timeout_secs));
        let name = s.cfg.name.clone();
        let what =
            if restart { tr!("перезапуск", "restarting") } else { tr!("остановка", "stopping") };
        self.notice(format!("{name}: {what}"), NoticeKind::Info);
    }

    fn backup(&mut self, idx: usize, auto: bool) {
        let s = &mut self.servers[idx];
        if s.cfg.kind == Kind::Rcon {
            self.notice(
                tr!("бэкап доступен только для локальных серверов", "backups are only available for local servers"),
                NoticeKind::Error,
            );
            return;
        }
        if s.backup_progress.is_some() || s.restoring {
            if !auto {
                self.notice(tr!("бэкап уже выполняется", "a backup is already running"), NoticeKind::Info);
            }
            return;
        }
        if s.status == Status::Starting || s.status == Status::Stopping {
            if !auto {
                self.notice(
                    tr!("дождитесь окончания запуска/остановки", "wait until the server has started/stopped"),
                    NoticeKind::Info,
                );
            }
            return;
        }
        let job = BackupJob {
            id: s.id,
            name: s.cfg.name.clone(),
            root: s.cfg.dir().to_path_buf(),
            paths: s.cfg.backup_paths.clone(),
            backup_dir: s.cfg.backup_dir(),
            keep: s.cfg.backup_keep,
            mode: s.cfg.backup_mode,
            copy_to: s.cfg.backup_copy_to.clone(),
            live: s.proc.as_ref().map(|p| (p.stdin(), p.lines.subscribe())),
        };
        s.backup_progress = Some(
            if auto {
                tr!("авто-бэкап…", "auto backup…")
            } else {
                tr!("подготовка…", "preparing…")
            }
            .into(),
        );
        s.played_since_backup = false;
        tokio::spawn(backup::run(job, self.tx.clone()));
    }

    fn restore(&mut self, idx: usize, path: PathBuf) {
        let s = &mut self.servers[idx];
        if !s.is_local() {
            return;
        }
        if s.backup_progress.is_some() || s.restoring {
            self.notice(tr!("дождитесь окончания бэкапа", "wait for the backup to finish"), NoticeKind::Error);
            return;
        }
        if s.proc.is_some() {
            s.pending_restore = Some((path, true));
            s.restart_at = None;
            let name = s.cfg.name.clone();
            self.log(idx, tr!("mctui: остановка для восстановления из бэкапа", "mctui: stopping to restore a backup"));
            self.stop(idx, false);
            self.notice(
                tr!(
                    format!("{name}: остановка, затем восстановление из бэкапа"),
                    format!("{name}: stopping, then restoring the backup")
                ),
                NoticeKind::Info,
            );
        } else {
            self.run_restore(idx, path, false);
        }
    }

    fn run_restore(&mut self, idx: usize, path: PathBuf, start_after: bool) {
        let s = &mut self.servers[idx];
        s.restoring = true;
        s.backup_progress = Some(tr!("восстановление…", "restoring…").into());
        s.pending_restore = None;
        let (id, root, paths, tx) = (s.id, s.cfg.dir().to_path_buf(), s.cfg.backup_paths.clone(), self.tx.clone());
        s.restart_after_stop = start_after; // запомним, запускать ли после
        tokio::task::spawn_blocking(move || {
            let tx2 = tx.clone();
            let ev = match backup::restore(&root, &path, &paths, |p| {
                let _ = tx2.send(AppEvent::Restore { id, ev: RestoreEvent::Progress(p) });
            }) {
                Ok(stash) => RestoreEvent::Done { stash },
                Err(e) => RestoreEvent::Failed(format!("{e:#}")),
            };
            let _ = tx.send(AppEvent::Restore { id, ev });
        });
    }

    /// Отправка команды серверу. Локальный — через stdin, RCON — через подключение.
    fn send_command(&mut self, idx: usize, cmd: &str, kind: ReqKind) -> Result<(), String> {
        let cmd = cmd.trim().trim_start_matches('/').to_string();
        let s = &mut self.servers[idx];
        match s.cfg.kind {
            Kind::Local => {
                let Some(p) = &s.proc else {
                    return Err(tr!("сервер не запущен", "server is not running").into());
                };
                if !p.send(&cmd) {
                    return Err(tr!("stdin процесса закрыт", "process stdin is closed").into());
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
                if kind == ReqKind::User || kind == ReqKind::Action {
                    self.out.lines.push((s.id, format!("> {cmd}")));
                    s.log.push(&format!("> {cmd}"));
                }
                Ok(())
            }
            Kind::Rcon => {
                let Some(r) = &s.rcon else {
                    return Err(tr!("нет подключения RCON", "no RCON connection").into());
                };
                if kind == ReqKind::User {
                    s.response = vec![format!("> {cmd}"), "…".into()];
                }
                if r.send(kind, &cmd) {
                    Ok(())
                } else {
                    Err(tr!("задача RCON завершилась", "RCON task has exited").into())
                }
            }
        }
    }

    pub fn quit_stop_all(&mut self) {
        self.quitting = true;
        for i in 0..self.servers.len() {
            self.servers[i].restart_at = None;
            if self.servers[i].proc.is_some() && self.servers[i].status != Status::Stopping {
                self.stop(i, false);
            }
        }
        self.check_quit();
    }

    fn check_quit(&mut self) {
        if self.quitting && self.servers.iter().all(|s| s.proc.is_none() && !s.restoring) {
            self.save_history();
            self.should_exit = true;
        }
    }

    // ───────────────────────────── список серверов ─────────────────────────────

    fn save_config(&mut self) {
        let cfg =
            Config { language: self.language.clone(), servers: self.servers.iter().map(|s| s.cfg.clone()).collect() };
        if let Err(e) = cfg.save(&self.config_path) {
            let p = self.config_path.display().to_string();
            self.notice(
                tr!(format!("не удалось сохранить {p}: {e:#}"), format!("could not save {p}: {e:#}")),
                NoticeKind::Error,
            );
        }
    }

    fn unique_name(&self, base: &str, except: Option<usize>) -> String {
        let mut name = base.to_string();
        let mut n = 2;
        while self.servers.iter().any(|s| Some(s.id) != except && s.cfg.name == name) {
            name = format!("{base} ({n})");
            n += 1;
        }
        name
    }

    fn add_server(&mut self, mut cfg: ServerConfig) {
        cfg.name = self.unique_name(&cfg.name, None);
        if cfg.kind == Kind::Local
            && let Some(other) =
                self.servers.iter().find(|s| s.cfg.kind == Kind::Local && s.cfg.dir.is_some() && s.cfg.dir == cfg.dir)
        {
            let name = other.cfg.name.clone();
            self.notice(
                tr!(
                    format!("Эта папка уже добавлена как «{name}»"),
                    format!("This folder is already added as “{name}”")
                ),
                NoticeKind::Error,
            );
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let name = cfg.name.clone();
        let is_rcon = cfg.kind == Kind::Rcon;
        self.servers.push(ServerState::new(cfg, id));
        let i = self.servers.len() - 1;
        self.load_history(i);
        self.load_sessions(i);
        if is_rcon {
            self.connect_rcon(i);
        }
        self.save_config();
        let hint =
            if is_rcon { "" } else { tr!(" — Ctrl+S, чтобы запустить", " — press Ctrl+S to start") };
        self.notice(
            tr!(format!("Сервер «{name}» добавлен{hint}"), format!("Server “{name}” added{hint}")),
            NoticeKind::Ok,
        );
    }

    fn update_server(&mut self, i: usize, mut cfg: ServerConfig) {
        let id = self.servers[i].id;
        cfg.name = self.unique_name(&cfg.name, Some(id));
        let s = &mut self.servers[i];
        let reconnect = s.cfg.kind == Kind::Rcon
            && (s.cfg.host != cfg.host || s.cfg.port != cfg.port || s.cfg.password != cfg.password);
        s.cfg = cfg;
        s.next_backup = None;
        s.next_restart = None;
        s.restart_warned.clear();
        if reconnect {
            self.connect_rcon(i);
        }
        self.update_schedule(i);
        self.save_config();
    }

    fn remove_server(&mut self, idx: usize) {
        if self.servers[idx].proc.is_some() || self.servers[idx].restoring {
            self.notice(tr!("сначала остановите сервер", "stop the server first"), NoticeKind::Error);
            return;
        }
        let s = self.servers.remove(idx);
        self.save_config();
        self.notice(
            tr!(format!("«{}» убран из списка", s.cfg.name), format!("“{}” removed from the list", s.cfg.name)),
            NoticeKind::Info,
        );
    }

    // ───────────────────────────── фоновые события ─────────────────────────────

    pub fn on_event(&mut self, ev: AppEvent) {
        self.out.dirty = true;
        match ev {
            AppEvent::Line { id, line } => {
                if let Some(i) = self.idx(id) {
                    self.on_line(i, &line)
                }
            }
            AppEvent::Exited { id, pid, status } => {
                if let Some(i) = self.idx(id) {
                    self.on_exit(i, pid, status)
                }
            }
            AppEvent::Rcon { id, generation, ev } => {
                if let Some(i) = self.idx(id) {
                    self.on_rcon(i, generation, ev)
                }
            }
            AppEvent::Backup { id, ev } => {
                if let Some(i) = self.idx(id) {
                    self.on_backup(i, ev)
                }
            }
            AppEvent::Restore { id, ev } => {
                if let Some(i) = self.idx(id) {
                    self.on_restore(i, ev)
                }
            }
            AppEvent::UpdateAvailable(_) | AppEvent::Daemon(_) | AppEvent::DaemonLost(_) | AppEvent::Ui(_) => {}
        }
    }

    fn on_line(&mut self, idx: usize, raw: &str) {
        let s = &mut self.servers[idx];
        let line = s.log.push(raw).clone();
        self.out.lines.push((s.id, raw.to_string()));
        let text = &line.text;
        if s.status == Status::Starting && stats::is_done(text) {
            s.status = Status::Running;
            let name = s.cfg.name.clone();
            self.connect_rcon(idx);
            self.update_schedule(idx);
            self.notice(tr!(format!("{name} запущен"), format!("{name} is up")), NoticeKind::Ok);
        }
        let s = &mut self.servers[idx];
        if stats::is_stopping(text) && matches!(s.status, Status::Running | Status::Starting) {
            s.status = Status::Stopping;
            s.stop_deadline.get_or_insert(Instant::now() + Duration::from_secs(s.cfg.stop_timeout_secs));
        }
        if let Some(v) = stats::version(text) {
            s.version = Some(v);
        }
        s.note_op_text(text);
        if crate::alerts::is_oom_line(text)
            && let Some(c) = s.alerts.note_oom(Instant::now())
        {
            self.apply_alert(idx, c);
        }
        let s = &mut self.servers[idx];
        match stats::player_event(text) {
            Some(PlayerEvent::Joined(n)) => {
                if let Some(log) = &mut s.sessions {
                    log.record_join(&n, Local::now());
                }
                s.players.insert(n);
                s.played_since_backup = true;
                self.save_sessions(idx, false);
            }
            Some(PlayerEvent::Left(n)) => {
                if let Some(log) = &mut s.sessions {
                    log.record_leave(&n, Local::now());
                }
                s.players.remove(&n);
                self.save_sessions(idx, false);
            }
            None => {}
        }
        let s = &mut self.servers[idx];
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
        if let Some(log) = &mut s.sessions {
            log.close_all(Local::now());
        }
        self.save_sessions(idx, false);
        let s = &mut self.servers[idx];
        s.next_backup = None;
        s.next_restart = None;
        s.restart_warned.clear();
        s.last_exit = Some(format!("{} — {status}", Local::now().format("%H:%M:%S")));
        let name = s.cfg.name.clone();
        let user_stop = s.user_stop;
        let restart_after = s.restart_after_stop;
        let pending = s.pending_restore.take();
        self.log(
            idx,
            &tr!(
                format!("──────── mctui: процесс завершился ({status}) ────────"),
                format!("──────── mctui: process exited ({status}) ────────")
            ),
        );
        if let Some((path, start_after)) = pending {
            self.servers[idx].status = Status::Offline;
            self.run_restore(idx, path, start_after && !self.quitting);
        } else if user_stop {
            self.servers[idx].status = Status::Offline;
            if restart_after && !self.quitting {
                self.start(idx);
            } else {
                self.notice(tr!(format!("{name} остановлен"), format!("{name} stopped")), NoticeKind::Info);
            }
        } else {
            let s = &mut self.servers[idx];
            s.status = Status::Crashed;
            if s.cfg.auto_restart && !self.quitting {
                match s.crash.on_crash(Instant::now()) {
                    crate::server::supervisor::Decision::Restart => {
                        let d = s.cfg.restart_delay_secs;
                        s.status = Status::Restarting;
                        s.restart_at = Some(Instant::now() + Duration::from_secs(d));
                        self.desktop(
                            idx,
                            &tr!(format!("упал ({status}), перезапуск"), format!("crashed ({status}), restarting")),
                        );
                        self.notice(
                            tr!(
                                format!("{name} упал ({status}), перезапуск через {d} с"),
                                format!("{name} crashed ({status}), restarting in {d}s")
                            ),
                            NoticeKind::Error,
                        );
                    }
                    crate::server::supervisor::Decision::GiveUp => {
                        s.crash_gave_up = true;
                        self.desktop(
                            idx,
                            tr!(
                                "падает слишком часто — авто-рестарт остановлен",
                                "keeps crashing — auto-restart stopped"
                            ),
                        );
                        self.log(
                            idx,
                            tr!(
                                "mctui: 3 падения за 5 минут — авто-рестарт приостановлен",
                                "mctui: 3 crashes in 5 minutes — auto-restart paused"
                            ),
                        );
                        self.notice(
                            tr!(
                                format!("{name} падает слишком часто — авто-рестарт остановлен"),
                                format!("{name} keeps crashing — auto-restart stopped")
                            ),
                            NoticeKind::Error,
                        );
                    }
                }
            } else {
                self.desktop(idx, &tr!(format!("завершился: {status}"), format!("exited: {status}")));
                self.notice(
                    tr!(format!("{name} завершился: {status}"), format!("{name} exited: {status}")),
                    NoticeKind::Error,
                );
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
                s.last_poll = None; // опросить сразу
                if remote {
                    s.status = Status::Online;
                    let line = tr!(
                        format!("── RCON подключён к {}:{} ──", s.cfg.host, s.cfg.port),
                        format!("── RCON connected to {}:{} ──", s.cfg.host, s.cfg.port)
                    );
                    self.log(idx, &line);
                }
            }
            RconEvent::Disconnected(e) => {
                let changed = s.rcon_state != RconState::Error(e.clone());
                s.rcon_state = RconState::Error(e.clone());
                if remote {
                    s.status = Status::Unreachable;
                    s.players.clear();
                    s.tps = None;
                    if changed {
                        self.log(idx, &format!("── RCON: {e} ──"));
                    }
                }
            }
            RconEvent::Response { kind, cmd, result } => match kind {
                ReqKind::User => {
                    if let Ok(body) = &result {
                        s.note_op_text(&crate::log::strip_codes(body));
                    }
                    let mut out = vec![format!("> {cmd}")];
                    match &result {
                        Ok(body) if body.trim().is_empty() => {
                            out.push(tr!("(пустой ответ)", "(empty response)").into())
                        }
                        Ok(body) => out.extend(body.lines().map(crate::log::strip_codes)),
                        Err(e) => out.push(tr!(format!("ошибка: {e}"), format!("error: {e}"))),
                    }
                    s.response = out.clone();
                    for l in &out {
                        self.log(idx, l);
                    }
                }
                ReqKind::Action => {
                    if let Ok(body) = &result {
                        s.note_op_text(&crate::log::strip_codes(body));
                    }
                    let msg = match result {
                        Ok(b) if !b.trim().is_empty() => crate::log::strip_codes(b.lines().next().unwrap_or("")),
                        Ok(_) => format!("{cmd}: ok"),
                        Err(e) => format!("{cmd}: {e}"),
                    };
                    self.log(idx, &format!("> {cmd}: {msg}"));
                    self.notice(msg, NoticeKind::Info);
                }
                ReqKind::PollList => {
                    if let Some(info) = result.ok().as_deref().and_then(stats::parse_list) {
                        if !info.players.is_empty() {
                            s.played_since_backup = true;
                        }
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
                let size = human_bytes(bytes);
                s.last_backup = Some(format!("{} — {name} ({size})", Local::now().format("%H:%M:%S")));
                if s.cfg.backup_every_hours > 0 && s.proc.is_some() {
                    s.next_backup = Some(Local::now() + chrono::Duration::hours(s.cfg.backup_every_hours as i64));
                }
                match warning {
                    Some(w) => self.notice(
                        tr!(format!("бэкап готов, но: {w}"), format!("backup done, but: {w}")),
                        NoticeKind::Error,
                    ),
                    None => self.notice(
                        tr!(format!("бэкап готов: {name} ({size})"), format!("backup done: {name} ({size})")),
                        NoticeKind::Ok,
                    ),
                }
            }
            BackupEvent::Failed(e) => {
                s.backup_progress = None;
                self.notice(tr!(format!("бэкап не удался: {e}"), format!("backup failed: {e}")), NoticeKind::Error);
            }
        }
    }

    fn on_restore(&mut self, idx: usize, ev: RestoreEvent) {
        let s = &mut self.servers[idx];
        match ev {
            RestoreEvent::Progress(p) => s.backup_progress = Some(p),
            RestoreEvent::Done { stash } => {
                s.restoring = false;
                s.backup_progress = None;
                let start_after = std::mem::take(&mut s.restart_after_stop);
                let stash = stash.display().to_string();
                self.log(
                    idx,
                    &tr!(
                        format!("mctui: мир восстановлен из бэкапа; прежний сохранён в {stash}"),
                        format!("mctui: world restored from backup; the previous one is kept in {stash}")
                    ),
                );
                self.notice(
                    tr!(
                        format!("восстановлено; прежний мир — в {stash}"),
                        format!("restored; previous world kept in {stash}")
                    ),
                    NoticeKind::Ok,
                );
                if start_after {
                    self.start(idx);
                }
            }
            RestoreEvent::Failed(e) => {
                s.restoring = false;
                s.backup_progress = None;
                s.restart_after_stop = false;
                self.log(idx, &format!("mctui: restore: {e}"));
                self.notice(
                    tr!(format!("восстановление не удалось: {e}"), format!("restore failed: {e}")),
                    NoticeKind::Error,
                );
            }
        }
        self.check_quit();
    }

    // ───────────────────────────── расписания ─────────────────────────────

    /// Пересчитать время следующего авто-бэкапа и рестарта (после запуска или правки настроек).
    fn update_schedule(&mut self, idx: usize) {
        let s = &mut self.servers[idx];
        let running = s.proc.is_some() && s.status == Status::Running;
        let now = Local::now();
        s.next_backup = (running && s.cfg.backup_every_hours > 0)
            .then(|| s.next_backup.unwrap_or(now + chrono::Duration::hours(s.cfg.backup_every_hours as i64)));
        s.next_restart = if running { next_time(now, &s.cfg.restart_hm()) } else { None };
        s.restart_warned.clear();
    }

    fn run_schedules(&mut self, idx: usize) {
        let now = Local::now();
        let s = &mut self.servers[idx];
        if s.proc.is_none() || s.status != Status::Running {
            return;
        }
        if !s.players.is_empty() {
            s.played_since_backup = true;
        }
        if let Some(t) = s.next_backup
            && now >= t
        {
            s.next_backup = Some(now + chrono::Duration::hours(s.cfg.backup_every_hours.max(1) as i64));
            if s.played_since_backup || s.last_backup.is_none() {
                self.log(idx, tr!("mctui: авто-бэкап по расписанию", "mctui: scheduled backup"));
                self.backup(idx, true);
            } else {
                self.log(
                    idx,
                    tr!(
                        "mctui: авто-бэкап пропущен — с прошлого бэкапа никто не играл",
                        "mctui: scheduled backup skipped — nobody played since the last one"
                    ),
                );
            }
        }
        let s = &mut self.servers[idx];
        let Some(t) = s.next_restart else { return };
        let left = (t - now).num_seconds();
        if left <= 0 {
            s.next_restart = None;
            s.restart_warned.clear();
            self.log(idx, tr!("mctui: рестарт по расписанию", "mctui: scheduled restart"));
            self.stop(idx, true);
            return;
        }
        let left = left as u64;
        let mut say = None;
        for w in RESTART_WARNINGS {
            if left <= w && !s.restart_warned.contains(&w) {
                s.restart_warned.insert(w);
                // Предупреждаем только о ближайшем пороге (не засыпаем чат пропущенными).
                if left + 2 > w {
                    say = Some(w);
                }
            }
        }
        if let Some(w) = say
            && let Some(p) = &s.proc
        {
            p.send(&format!("say {}", restart_warning(w)));
        }
    }

    // ───────────────────────────── таймер ─────────────────────────────

    /// Раз в секунду: таймауты остановки, отложенные рестарты, расписания, ресурсы, опрос RCON, графики.
    pub fn on_tick(&mut self) {
        self.check_detach();
        let now = Instant::now();
        self.out.dirty = true;
        for i in 0..self.servers.len() {
            let s = &mut self.servers[i];
            if let (Some(d), Some(p)) = (s.stop_deadline, s.proc.as_mut())
                && now >= d
            {
                p.kill();
                s.stop_deadline = None;
                self.log(
                    i,
                    tr!(
                        "mctui: сервер не остановился за отведённое время — kill",
                        "mctui: server did not stop in time — killing"
                    ),
                );
            }
            let s = &mut self.servers[i];
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
            self.servers[i].reload_ops();
            self.run_schedules(i);
        }
        self.refresh_resources();
        self.check_alerts(now);
        self.take_samples(now);
        if now.duration_since(self.last_history_save) >= HISTORY_SAVE_EVERY {
            self.last_history_save = now;
            self.save_history();
        }
    }

    /// Предупреждения о нагрузке: TPS, свободная память компьютера, место на диске, OOM.
    fn check_alerts(&mut self, now: Instant) {
        use crate::alerts::{self, Kind};
        let watched = |s: &ServerState| {
            (s.proc.is_some() && s.status == Status::Running)
                || (s.cfg.kind == crate::config::Kind::Rcon && s.status == Status::Online)
        };
        if !self.servers.iter().any(watched) {
            return;
        }
        let (avail, total) = match &mut self.sys {
            Some(sys) => {
                sys.refresh_memory();
                (sys.available_memory(), sys.total_memory())
            }
            None => (0, 0),
        };
        let disks = if self.last_disk_check.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(30)) {
            self.last_disk_check = Some(now);
            sysinfo::Disks::new_with_refreshed_list().ok()
        } else {
            None
        };
        for i in 0..self.servers.len() {
            let s = &mut self.servers[i];
            if !watched(s) {
                continue;
            }
            let mut changes = vec![];
            if let Some(t) = s.tps {
                let bad = alerts::tps_bad(t[0], s.alerts.is_active(Kind::LowTps));
                changes.extend(s.alerts.observe(Kind::LowTps, bad, now));
            }
            if s.proc.is_some() {
                changes.extend(s.alerts.observe(Kind::LowMemory, alerts::memory_bad(avail, total), now));
                if let (Some(d), Some(dir)) = (&disks, &s.cfg.dir)
                    && let Some(free) = disk_free(d, dir)
                {
                    changes.extend(s.alerts.observe(Kind::LowDisk, free < alerts::DISK_LOW_BYTES, now));
                }
                changes.extend(s.alerts.expire_oom(now));
            }
            for c in changes {
                self.apply_alert(i, c);
            }
        }
    }

    fn apply_alert(&mut self, i: usize, change: crate::alerts::Change) {
        use crate::alerts::Change;
        let name = self.servers[i].cfg.name.clone();
        match change {
            Change::Raised(k) => {
                let text = k.text();
                self.log(i, &format!("mctui: ⚠ {text}"));
                self.notice(format!("{name}: {text}"), NoticeKind::Error);
                if self.servers[i].alerts.should_notify(k, Instant::now()) {
                    self.desktop(i, &text);
                }
            }
            Change::Cleared(k) => {
                let text = k.text();
                self.log(i, &tr!(format!("mctui: ✓ прошло: {text}"), format!("mctui: ✓ cleared: {text}")));
            }
        }
    }

    /// Уведомление рабочего стола от имени сервера (если `notify` включён).
    fn desktop(&self, i: usize, body: &str) {
        let s = &self.servers[i];
        if s.cfg.notify && std::env::var_os("MCTUI_NO_NOTIFY").is_none() {
            crate::alerts::desktop(&format!("mctui · {}", s.cfg.name), body);
        }
    }

    fn take_samples(&mut self, now: Instant) {
        for s in &mut self.servers {
            let alive = s.proc.is_some() || s.status == Status::Online;
            if !alive || s.last_sample.is_some_and(|t| now.duration_since(t) < SAMPLE_EVERY) {
                continue;
            }
            s.last_sample = Some(now);
            let sample = Sample {
                t: Local::now().timestamp(),
                players: s.players.len() as u16,
                tps: s.tps.map(|t| t[0] as f32),
                mem_mb: s.mem.map(|m| (m / (1024 * 1024)) as u32),
                cpu: s.cpu,
            };
            s.history.push_back(sample);
            while s.history.len() > HISTORY_CAP {
                s.history.pop_front();
            }
            self.out.samples.push((s.id, sample));
        }
    }

    fn history_path(&self, i: usize) -> PathBuf {
        let s = &self.servers[i];
        crate::config::state_dir(&self.config_path)
            .join("history")
            .join(format!("{}.json", backup::file_prefix(&s.cfg.name)))
    }

    fn load_history(&mut self, i: usize) {
        let path = self.history_path(i);
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let Ok(v) = serde_json::from_str::<Vec<Sample>>(&text) else { return };
        let since = Local::now().timestamp() - 24 * 3600;
        self.servers[i].history = v.into_iter().filter(|s| s.t >= since).collect();
    }

    fn load_sessions(&mut self, i: usize) {
        if !self.servers[i].is_local() {
            return;
        }
        let path = crate::playerstats::session_log_path(&self.config_path, &self.servers[i].cfg.name);
        let mut log = crate::playerstats::SessionLog::load(path);
        log.recover();
        self.servers[i].sessions = Some(log);
    }

    fn save_sessions(&mut self, i: usize, heartbeat: bool) {
        if let Some(log) = &mut self.servers[i].sessions {
            if heartbeat {
                log.heartbeat(Local::now());
            }
            if let Err(e) = log.save() {
                eprintln!("mctui: sessions: {e:#}");
            }
        }
    }

    pub fn save_history(&mut self) {
        for i in 0..self.servers.len() {
            if self.servers[i].proc.is_some() {
                self.save_sessions(i, true);
            }
        }
        for i in 0..self.servers.len() {
            let path = self.history_path(i);
            if self.servers[i].history.is_empty() {
                continue;
            }
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_string(&self.servers[i].history) {
                let _ = std::fs::write(path, json);
            }
        }
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
}

/// Свободное место на диске, где лежит `dir` (самая длинная подходящая точка монтирования).
pub(crate) fn disk_free(disks: &sysinfo::Disks, dir: &std::path::Path) -> Option<u64> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    disks
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// Ближайшее будущее время из списка (час, минута) — сегодня или завтра.
pub fn next_time(now: chrono::DateTime<Local>, times: &[(u32, u32)]) -> Option<chrono::DateTime<Local>> {
    times
        .iter()
        .filter_map(|&(h, m)| {
            let today = now.date_naive().and_hms_opt(h, m, 0)?;
            let mut t = Local.from_local_datetime(&today).earliest()?;
            if t <= now {
                let tomorrow = (now.date_naive() + chrono::Duration::days(1)).and_hms_opt(h, m, 0)?;
                t = Local.from_local_datetime(&tomorrow).earliest()?;
            }
            Some(t)
        })
        .min()
}

fn restart_warning(secs: u64) -> String {
    if secs >= 60 {
        let m = secs / 60;
        tr!(format!("Сервер перезапустится через {m} мин"), format!("Server restarts in {m} min"))
    } else {
        tr!(format!("Сервер перезапустится через {secs} с"), format!("Server restarts in {secs} s"))
    }
}

pub fn human_bytes(b: u64) -> String {
    let units: [&str; 5] = if crate::i18n::is_en() {
        ["B", "KiB", "MiB", "GiB", "TiB"]
    } else {
        ["Б", "КиБ", "МиБ", "ГиБ", "ТиБ"]
    };
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} {}", units[0]) } else { format!("{v:.1} {}", units[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_restart_time() {
        let now = Local.with_ymd_and_hms(2026, 11, 13, 4, 30, 0).unwrap();
        let t = next_time(now, &[(5, 0)]).unwrap();
        assert_eq!(t, Local.with_ymd_and_hms(2026, 11, 13, 5, 0, 0).unwrap());
        let t = next_time(now, &[(3, 0), (17, 0)]).unwrap();
        assert_eq!(t, Local.with_ymd_and_hms(2026, 11, 13, 17, 0, 0).unwrap());
        let t = next_time(now, &[(4, 0)]).unwrap();
        assert_eq!(t, Local.with_ymd_and_hms(2026, 11, 14, 4, 0, 0).unwrap());
        assert!(next_time(now, &[]).is_none());
    }

    #[test]
    fn warnings_text() {
        crate::i18n::set(crate::i18n::Lang::Ru);
        assert_eq!(restart_warning(300), "Сервер перезапустится через 5 мин");
        assert_eq!(restart_warning(10), "Сервер перезапустится через 10 с");
    }
}
