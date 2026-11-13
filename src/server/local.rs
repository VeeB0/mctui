//! Локальный сервер как дочерний процесс.

use crate::config::ServerConfig;
use crate::event::AppEvent;
use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot};

pub struct LocalProc {
    pub pid: u32,
    stdin: mpsc::UnboundedSender<String>,
    kill: Option<oneshot::Sender<()>>,
    /// Копия строк вывода — для задач, которым нужно дождаться ответа (бэкап).
    pub lines: broadcast::Sender<String>,
}

impl LocalProc {
    pub fn spawn(id: usize, cfg: &ServerConfig, tx: mpsc::UnboundedSender<AppEvent>) -> Result<Self> {
        let dir = cfg.dir();
        let mut program = std::path::PathBuf::from(&cfg.command[0]);
        if program.is_relative() && cfg.command[0].contains(['/', '\\']) {
            program = dir.join(program);
        }
        let mut cmd = Command::new(&program);
        cmd.args(&cfg.command[1..])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Свой group: Ctrl+C в терминале не долетает до сервера, а «убить» можно всё дерево.
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
        let mut child = cmd.spawn().with_context(|| {
            crate::tr!(
                format!("не удалось запустить {}", program.display()),
                format!("could not start {}", program.display())
            )
        })?;
        let pid = child.id().unwrap_or(0);

        let (lines_tx, _) = broadcast::channel(1024);
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump(id, out, tx.clone(), lines_tx.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump(id, err, tx.clone(), lines_tx.clone()));
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

        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                _ = kill_rx => {
                    kill_tree(pid);
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let status = match status {
                Ok(s) => describe_status(s),
                Err(e) => {
                    crate::tr!(format!("ошибка ожидания процесса: {e}"), format!("error waiting for the process: {e}"))
                }
            };
            let _ = tx.send(AppEvent::Exited { id, pid, status });
        });

        Ok(Self { pid, stdin: stdin_tx, kill: Some(kill_tx), lines: lines_tx })
    }

    pub fn send(&self, line: &str) -> bool {
        self.stdin.send(line.to_string()).is_ok()
    }

    pub fn stdin(&self) -> mpsc::UnboundedSender<String> {
        self.stdin.clone()
    }

    pub fn kill(&mut self) {
        if let Some(k) = self.kill.take() {
            let _ = k.send(());
        }
    }
}

/// Принудительно завершает процесс вместе с потомками (скрипт запуска → java).
fn kill_tree(pid: u32) {
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

fn describe_status(s: std::process::ExitStatus) -> String {
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

async fn pump<R: AsyncRead + Unpin>(
    id: usize,
    r: R,
    tx: mpsc::UnboundedSender<AppEvent>,
    lines: broadcast::Sender<String>,
) {
    let mut br = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match br.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
                let _ = lines.send(line.clone());
                if tx.send(AppEvent::Line { id, line }).is_err() {
                    break;
                }
            }
        }
    }
}
