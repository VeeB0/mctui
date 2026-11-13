//! Подключение окна TUI к фоновой службе: запуск службы при необходимости и переподключение.

use crate::event::AppEvent;
use crate::proto::{self, Endpoint, Msg, Request};
use crate::tr;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Отправка запросов службе. Пока связи нет, запросы отбрасываются.
#[derive(Clone)]
pub struct Client {
    tx: mpsc::UnboundedSender<Request>,
}

impl Client {
    pub fn send(&self, r: Request) {
        let _ = self.tx.send(r);
    }
}

pub fn start(config_path: PathBuf, default_config: bool, app: mpsc::UnboundedSender<AppEvent>) -> Client {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(supervise(config_path, default_config, app, rx));
    Client { tx }
}

async fn try_connect(config_path: &std::path::Path) -> Result<(TcpStream, Msg)> {
    let text = std::fs::read_to_string(proto::endpoint_path(config_path)).context("daemon.json")?;
    let ep: Endpoint = serde_json::from_str(&text)?;
    let mut stream = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(("127.0.0.1", ep.port))).await??;
    let _ = stream.set_nodelay(true);
    let hello = Request::Hello { token: ep.token, version: crate::update::VERSION.into(), protocol: proto::PROTOCOL };
    stream.write_all(proto::encode(&hello).as_bytes()).await?;
    // Первая строка — Welcome (или Denied).
    let mut first = Vec::new();
    let mut byte = [0u8; 1];
    let read = async {
        loop {
            use tokio::io::AsyncReadExt;
            let n = stream.read(&mut byte).await?;
            if n == 0 {
                bail!("closed");
            }
            if byte[0] == b'\n' {
                break;
            }
            first.push(byte[0]);
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(5), read).await??;
    let msg: Msg = serde_json::from_slice(&first)?;
    if let Msg::Denied { reason } = &msg {
        bail!("{reason}");
    }
    Ok((stream, msg))
}

async fn supervise(
    config_path: PathBuf,
    default_config: bool,
    app: mpsc::UnboundedSender<AppEvent>,
    mut requests: mpsc::UnboundedReceiver<Request>,
) {
    let mut spawned_at: Option<Instant> = None;
    loop {
        let (stream, welcome) = match try_connect(&config_path).await {
            Ok(v) => v,
            Err(e) => {
                // Служба не отвечает — запускаем (не чаще раза в 15 с) и ждём.
                if spawned_at.is_none_or(|t| t.elapsed() > Duration::from_secs(15)) {
                    spawned_at = Some(Instant::now());
                    if let Err(e) = crate::service::start(&config_path, default_config) {
                        let _ = app.send(AppEvent::DaemonLost(format!("{e:#}")));
                    }
                } else if spawned_at.is_some_and(|t| t.elapsed() > Duration::from_secs(10)) {
                    let _ = app.send(AppEvent::DaemonLost(tr!(
                        format!("служба не отвечает: {e:#}"),
                        format!("service is not responding: {e:#}")
                    )));
                }
                // Запросы, пришедшие без связи, теряются.
                while requests.try_recv().is_ok() {}
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        if app.send(AppEvent::Daemon(Box::new(welcome))).is_err() {
            return;
        }
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => {
                        if let Ok(m) = serde_json::from_str::<Msg>(&l)
                            && app.send(AppEvent::Daemon(Box::new(m))).is_err()
                        {
                            return;
                        }
                    }
                    _ => break,
                },
                req = requests.recv() => match req {
                    Some(r) => {
                        if w.write_all(proto::encode(&r).as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    None => return,
                },
            }
        }
        let _ =
            app.send(AppEvent::DaemonLost(tr!("связь со службой потеряна", "lost connection to the service").into()));
        spawned_at = None;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Разовый запрос из командной строки (`mctui daemon stop` и т. п.). None — служба не запущена.
pub async fn oneshot(config_path: &std::path::Path, req: Request) -> Result<Option<Msg>> {
    let Ok((mut stream, welcome)) = try_connect(config_path).await else { return Ok(None) };
    stream.write_all(proto::encode(&req).as_bytes()).await?;
    stream.flush().await?;
    Ok(Some(welcome))
}

/// Состояние службы для `mctui daemon status`.
pub async fn status(config_path: &std::path::Path) -> Result<Option<(Msg, Vec<proto::ServerView>)>> {
    let Ok((stream, welcome)) = try_connect(config_path).await else { return Ok(None) };
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Ok(Some(l))) = tokio::time::timeout(Duration::from_secs(3), lines.next_line()).await {
        if let Ok(Msg::State { servers }) = serde_json::from_str::<Msg>(&l) {
            return Ok(Some((welcome, servers)));
        }
    }
    Ok(Some((welcome, vec![])))
}
