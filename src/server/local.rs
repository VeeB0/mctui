//! Локальный сервер со стороны службы: сам процесс держит хранитель (`keeper.rs`), служба подключается к
//! нему по TCP. Если службу перезапустить, сервер продолжает работать, а новая служба подключается заново.

use super::keeper::{self, FromKeeper, State, ToKeeper};
use crate::config::ServerConfig;
use crate::event::AppEvent;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

pub struct LocalProc {
    pub pid: u32,
    stdin: mpsc::UnboundedSender<String>,
    kill: mpsc::UnboundedSender<()>,
    /// Копия строк вывода — для задач, которым нужно дождаться ответа (бэкап).
    pub lines: broadcast::Sender<String>,
}

/// Сервер, запущенный ещё прошлой службой.
pub struct Attached {
    pub proc: LocalProc,
    /// Unix-время запуска.
    pub started: i64,
    /// Сервер уже дошёл до «Done (…)!».
    pub ready: bool,
    /// Последние строки вывода.
    pub tail: Vec<String>,
}

pub enum Found {
    Nothing,
    /// Сервер завершился, пока службы не было.
    Ended(String),
    Running(Box<Attached>),
}

pub fn run_dir(config_path: &Path) -> PathBuf {
    crate::config::state_dir(config_path).join("run")
}

fn state_path(run_dir: &Path, cfg: &ServerConfig) -> PathBuf {
    run_dir.join(format!("{}.json", keeper::run_key(cfg.dir())))
}

/// Отметка «бэкап выключил автосохранение» этого сервера (см. [`crate::backup::take_save_off_mark`]).
pub fn save_off_mark(run_dir: &Path, cfg: &ServerConfig) -> PathBuf {
    run_dir.join(format!("{}.save-off", keeper::run_key(cfg.dir())))
}

impl LocalProc {
    /// Запускает сервер через хранителя и подключается к нему.
    pub fn spawn(id: usize, cfg: &ServerConfig, run_dir: &Path, tx: mpsc::UnboundedSender<AppEvent>) -> Result<Self> {
        let dir = cfg.dir();
        let mut program = PathBuf::from(&cfg.command[0]);
        if program.is_relative() && cfg.command[0].contains(['/', '\\']) {
            program = dir.join(program);
        }
        std::fs::create_dir_all(run_dir).with_context(|| run_dir.display().to_string())?;
        let state = state_path(run_dir, cfg);
        if let Some(old) = keeper::read_state(&state)
            && old.exited.is_none()
            && old.error.is_none()
            && std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], old.port).into(), Duration::from_millis(300))
                .is_ok()
        {
            bail!(crate::tr!(
                format!("сервер в {} уже запущен (pid {})", dir.display(), old.pid),
                format!("a server in {} is already running (pid {})", dir.display(), old.pid)
            ));
        }
        let _ = std::fs::remove_file(&state);
        let spec_path = state.with_extension("spec.json");
        let spec = keeper::Spec {
            dir: dir.to_path_buf(),
            program: program.clone(),
            args: cfg.command[1..].to_vec(),
            state: state.clone(),
        };
        std::fs::write(&spec_path, serde_json::to_string(&spec)?)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state.with_extension("log"))
            .context("keeper log")?;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("__keeper").arg(&spec_path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Своя группа процессов: ни Ctrl+C терминала, ни остановка службы (launchd/systemd) его не задевают.
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        let mut child = cmd
            .spawn()
            .context(crate::tr!("не удалось запустить хранитель процесса", "could not start the process keeper"))?;
        // Ждём файл состояния: хранитель запустил сервер и слушает порт.
        let deadline = Instant::now() + Duration::from_secs(10);
        let st = loop {
            if let Some(st) = keeper::read_state(&state) {
                break st;
            }
            if let Ok(Some(status)) = child.try_wait() {
                let _ = std::fs::remove_file(&spec_path);
                bail!(crate::tr!(
                    format!("хранитель процесса завершился сразу ({status})"),
                    format!("the process keeper exited immediately ({status})")
                ));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!(crate::tr!("хранитель процесса не ответил", "the process keeper did not respond"));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Хранитель живёт сам по себе; zombie не остаётся — ждём его в фоне.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        if let Some(e) = st.error {
            let _ = std::fs::remove_file(&state);
            bail!(e);
        }
        let (proc, _) = connect(id, &st, state, tx)?;
        Ok(proc)
    }

    /// Подключиться к серверу, который запустила прошлая служба.
    pub fn attach(id: usize, cfg: &ServerConfig, run_dir: &Path, tx: mpsc::UnboundedSender<AppEvent>) -> Found {
        let state = state_path(run_dir, cfg);
        let Some(st) = keeper::read_state(&state) else { return Found::Nothing };
        if let Some(e) = st.exited.clone().or(st.error.clone()) {
            let _ = std::fs::remove_file(&state);
            return Found::Ended(e);
        }
        match connect(id, &st, state.clone(), tx) {
            Ok((proc, hello)) => {
                let FromKeeper::Hello { started, ready, tail, .. } = hello else { return Found::Nothing };
                Found::Running(Box::new(Attached { proc, started, ready, tail }))
            }
            Err(_) => {
                // Хранителя нет (компьютер перезагружался или его убили) — файл устарел.
                // Если он успел записать итог, отдаём его.
                match keeper::read_state(&state).and_then(|s| s.exited) {
                    Some(e) => {
                        let _ = std::fs::remove_file(&state);
                        Found::Ended(e)
                    }
                    None => {
                        let _ = std::fs::remove_file(&state);
                        Found::Nothing
                    }
                }
            }
        }
    }

    pub fn send(&self, line: &str) -> bool {
        self.stdin.send(line.to_string()).is_ok()
    }

    pub fn stdin(&self) -> mpsc::UnboundedSender<String> {
        self.stdin.clone()
    }

    pub fn kill(&mut self) {
        let _ = self.kill.send(());
    }
}

/// Рукопожатие (синхронно, коротко), затем чтение/запись в фоне.
fn connect(
    id: usize,
    st: &State,
    state_path: PathBuf,
    tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<(LocalProc, FromKeeper)> {
    let mut stream = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], st.port).into(), Duration::from_secs(2))
        .context("keeper connect")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(keeper::encode(&ToKeeper::Hello { token: st.token.clone() }).as_bytes())?;
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let hello: FromKeeper = serde_json::from_str(line.trim()).context("keeper hello")?;
    let FromKeeper::Hello { pid, .. } = &hello else { bail!("keeper hello") };
    let pid = *pid;
    // До `ready` хранитель больше ничего не присылает, поэтому в буфере reader ничего не осталось.
    drop(reader);
    stream.write_all(keeper::encode(&ToKeeper::Ready).as_bytes())?;
    stream.set_read_timeout(None)?;
    stream.set_nonblocking(true)?;
    let stream = tokio::net::TcpStream::from_std(stream)?;
    let (r, mut w) = stream.into_split();

    let (lines_tx, _) = broadcast::channel(1024);
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                l = stdin_rx.recv() => match l { Some(l) => ToKeeper::In { l }, None => break },
                k = kill_rx.recv() => match k { Some(()) => ToKeeper::Kill, None => break },
            };
            if w.write_all(keeper::encode(&msg).as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let lines = lines_tx.clone();
    tokio::spawn(async move {
        let mut rd = BufReader::new(r).lines();
        let mut status = None;
        while let Ok(Some(l)) = rd.next_line().await {
            match serde_json::from_str::<FromKeeper>(&l) {
                Ok(FromKeeper::Out { l }) => {
                    let _ = lines.send(l.clone());
                    if tx.send(AppEvent::Line { id, line: l }).is_err() {
                        return;
                    }
                }
                Ok(FromKeeper::Exit { status: s }) => {
                    status = Some(s);
                    break;
                }
                _ => {}
            }
        }
        let status = status.or_else(|| keeper::read_state(&state_path).and_then(|s| s.exited)).unwrap_or_else(|| {
            crate::tr!("связь с хранителем процесса потеряна", "lost the connection to the process keeper").into()
        });
        let _ = std::fs::remove_file(&state_path);
        let _ = tx.send(AppEvent::Exited { id, pid, status });
    });
    Ok((LocalProc { pid, stdin: stdin_tx, kill: kill_tx, lines: lines_tx }, hello))
}
