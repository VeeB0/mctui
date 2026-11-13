//! Хранитель процесса сервера (`mctui __keeper <spec>`): отдельный маленький процесс, который запускает
//! сервер и держит его stdin/stdout. Служба подключается к нему по TCP 127.0.0.1 с токеном, поэтому её
//! можно перезапустить (в том числе на новой версии mctui), не останавливая сервер: новая служба находит
//! файл состояния в `<state_dir>/run/` и подключается заново, получая хвост лога.
//!
//! Протокол — JSON-строки:
//! - служба → хранитель: `{"t":"hello","token":…}`, затем `{"t":"ready"}`, дальше `{"t":"in","l":…}`, `{"t":"kill"}`;
//! - хранитель → служба: `{"t":"hello","pid":…,"started":…,"ready":…,"tail":[…]}`, затем `{"t":"out","l":…}`,
//!   в конце `{"t":"exit","status":…}`.
//!
//! До `ready` хранитель ничего не пишет после приветствия — так служба читает приветствие буферизованно
//! и не теряет строк.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// Сколько последних строк вывода хранитель помнит для переподключившейся службы.
pub const TAIL: usize = 2000;

/// Что запускать (пишет служба, читает и удаляет хранитель).
#[derive(Debug, Serialize, Deserialize)]
pub struct Spec {
    pub dir: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub state: PathBuf,
}

/// Файл состояния `<state_dir>/run/<ключ>.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub keeper_pid: u32,
    /// pid процесса сервера (скрипт запуска или java).
    pub pid: u32,
    pub port: u16,
    pub token: String,
    /// Unix-время запуска сервера.
    pub started: i64,
    pub dir: PathBuf,
    pub version: String,
    /// Сервер завершился, пока службы не было (или не успели сообщить): как именно.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited: Option<String>,
    /// Не удалось запустить сервер.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToKeeper {
    Hello { token: String },
    Ready,
    In { l: String },
    Kill,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromKeeper {
    Hello { pid: u32, started: i64, ready: bool, tail: Vec<String> },
    Out { l: String },
    Exit { status: String },
}

pub fn encode<T: Serialize>(v: &T) -> String {
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    s
}

/// Ключ файла состояния — по папке сервера (имя сервера можно переименовать).
pub fn run_key(dir: &Path) -> String {
    use sha1::{Digest, Sha1};
    let d = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let hash = Sha1::digest(d.to_string_lossy().as_bytes());
    let name = d.file_name().map(|n| crate::backup::file_prefix(&n.to_string_lossy())).unwrap_or_default();
    let hex: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("{name}-{hex}")
}

pub fn read_state(path: &Path) -> Option<State> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn write_state(path: &Path, st: &State) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(st)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Принудительно завершает процесс вместе с потомками (скрипт запуска → java).
pub fn kill_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .args(["-9", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn describe_status(s: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return crate::tr!(format!("убит сигналом {sig}"), format!("killed by signal {sig}"));
        }
    }
    match s.code() {
        Some(0) => crate::tr!("код 0", "exit code 0").into(),
        Some(c) => crate::tr!(format!("код {c}"), format!("exit code {c}")),
        None => crate::tr!("неизвестный статус", "unknown status").into(),
    }
}

// ───────────────────────────── процесс-хранитель ─────────────────────────────

#[derive(Default)]
struct Shared {
    tail: VecDeque<String>,
    ready: bool,
    client: Option<mpsc::UnboundedSender<String>>,
    /// Пишет в подключение; `true` — всё записано.
    writer: Option<tokio::task::JoinHandle<bool>>,
}

/// Точка входа `mctui __keeper <spec>`.
pub async fn run(spec_path: &Path) -> i32 {
    let spec: Spec = match std::fs::read_to_string(spec_path)
        .map_err(anyhow::Error::from)
        .and_then(|t| serde_json::from_str(&t).map_err(anyhow::Error::from))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mctui keeper: {}: {e:#}", spec_path.display());
            return 2;
        }
    };
    let _ = std::fs::remove_file(spec_path);
    match keep(&spec).await {
        Ok(()) => 0,
        Err(e) => {
            let st = State {
                keeper_pid: std::process::id(),
                dir: spec.dir.clone(),
                version: crate::update::VERSION.into(),
                error: Some(format!("{e:#}")),
                ..Default::default()
            };
            let _ = write_state(&spec.state, &st);
            1
        }
    }
}

async fn keep(spec: &Spec) -> Result<()> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Свой group: «убить» можно всё дерево (скрипт + java).
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
    let mut child = cmd.spawn().with_context(|| {
        crate::tr!(
            format!("не удалось запустить {}", spec.program.display()),
            format!("could not start {}", spec.program.display())
        )
    })?;
    let pid = child.id().unwrap_or(0);
    let started = chrono::Local::now().timestamp();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("bind 127.0.0.1")?;
    let token = crate::daemon::random_token();
    let mut state = State {
        keeper_pid: std::process::id(),
        pid,
        port: listener.local_addr()?.port(),
        token: token.clone(),
        started,
        dir: spec.dir.clone(),
        version: crate::update::VERSION.into(),
        exited: None,
        error: None,
    };
    write_state(&spec.state, &state)?;

    let shared = Arc::new(Mutex::new(Shared::default()));
    let mut pumps = vec![];
    if let Some(out) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(out, shared.clone())));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(err, shared.clone())));
    }
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                if stdin.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });
    }
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<()>();
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (shared, token, stdin_tx, kill_tx) =
                    (shared.clone(), token.clone(), stdin_tx.clone(), kill_tx.clone());
                tokio::spawn(async move {
                    let _ = serve(stream, &token, pid, started, shared, stdin_tx, kill_tx).await;
                });
            }
        });
    }

    let status = tokio::select! {
        s = child.wait() => s,
        _ = kill_rx.recv() => {
            kill_tree(pid);
            let _ = child.start_kill();
            child.wait().await
        }
    };
    let status = match status {
        Ok(s) => describe_status(s),
        Err(e) => crate::tr!(format!("ошибка ожидания процесса: {e}"), format!("error waiting for the process: {e}")),
    };
    // Дочитать последние строки вывода.
    for p in pumps {
        let _ = tokio::time::timeout(Duration::from_secs(2), p).await;
    }
    state.exited = Some(status.clone());
    let _ = write_state(&spec.state, &state);
    let (client, writer) = {
        let mut sh = shared.lock().unwrap();
        (sh.client.take(), sh.writer.take())
    };
    if let Some(c) = client {
        let delivered = c.send(encode(&FromKeeper::Exit { status })).is_ok();
        drop(c);
        if let Some(w) = writer {
            let flushed = matches!(tokio::time::timeout(Duration::from_secs(3), w).await, Ok(Ok(true)));
            if delivered && flushed {
                // Служба узнала о выходе сама — файл больше не нужен.
                let _ = std::fs::remove_file(&spec.state);
            }
        }
    }
    Ok(())
}

async fn pump<R: AsyncRead + Unpin>(r: R, shared: Arc<Mutex<Shared>>) {
    let mut br = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match br.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
                let mut sh = shared.lock().unwrap();
                if !sh.ready && crate::server::stats::is_done(&crate::log::strip_codes(&line)) {
                    sh.ready = true;
                }
                if sh.tail.len() >= TAIL {
                    sh.tail.pop_front();
                }
                sh.tail.push_back(line.clone());
                if let Some(c) = &sh.client
                    && c.send(encode(&FromKeeper::Out { l: line })).is_err()
                {
                    sh.client = None;
                }
            }
        }
    }
}

/// Одно подключение службы. Новое подключение вытесняет старое.
async fn serve(
    stream: TcpStream,
    token: &str,
    pid: u32,
    started: i64,
    shared: Arc<Mutex<Shared>>,
    stdin_tx: mpsc::UnboundedSender<String>,
    kill_tx: mpsc::UnboundedSender<()>,
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let first = tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await??.unwrap_or_default();
    match serde_json::from_str::<ToKeeper>(&first) {
        Ok(ToKeeper::Hello { token: t }) if t == token => {}
        _ => bail!("bad hello"),
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let (go_tx, go_rx) = oneshot::channel::<()>();
    let hello = {
        let mut sh = shared.lock().unwrap();
        let hello = FromKeeper::Hello { pid, started, ready: sh.ready, tail: sh.tail.iter().cloned().collect() };
        // С этого момента новые строки копятся в очереди этого подключения.
        sh.client = Some(tx);
        hello
    };
    w.write_all(encode(&hello).as_bytes()).await?;
    let writer = tokio::spawn(async move {
        if go_rx.await.is_err() {
            return false;
        }
        while let Some(msg) = rx.recv().await {
            if w.write_all(msg.as_bytes()).await.is_err() {
                return false;
            }
        }
        w.shutdown().await.is_ok()
    });
    shared.lock().unwrap().writer = Some(writer);
    let mut go = Some(go_tx);
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<ToKeeper>(&line) {
            Ok(ToKeeper::Ready) => {
                if let Some(g) = go.take() {
                    let _ = g.send(());
                }
            }
            Ok(ToKeeper::In { l }) => {
                let _ = stdin_tx.send(l);
            }
            Ok(ToKeeper::Kill) => {
                let _ = kill_tx.send(());
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let m = encode(&ToKeeper::In { l: "say hi".into() });
        assert_eq!(m, "{\"t\":\"in\",\"l\":\"say hi\"}\n");
        let h: FromKeeper =
            serde_json::from_str(r#"{"t":"hello","pid":5,"started":1,"ready":true,"tail":["a"]}"#).unwrap();
        assert!(matches!(h, FromKeeper::Hello { pid: 5, ready: true, .. }));
    }

    #[test]
    fn run_key_is_stable_and_readable() {
        let a = run_key(Path::new("/srv/My Server"));
        assert_eq!(a, run_key(Path::new("/srv/My Server")));
        assert!(a.starts_with("My_Server-"));
        assert_ne!(a, run_key(Path::new("/other/My Server")));
    }
}
