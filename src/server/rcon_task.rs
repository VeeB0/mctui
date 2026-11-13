//! Фоновая задача, владеющая RCON-подключением: переподключение с backoff, очередь команд.

use super::rcon::RconClient;
use crate::event::{AppEvent, RconEvent, ReqKind};
use std::time::Duration;
use tokio::sync::mpsc;

pub struct RconHandle {
    tx: mpsc::UnboundedSender<(ReqKind, String)>,
    pub generation: u64,
}

impl RconHandle {
    pub fn send(&self, kind: ReqKind, cmd: &str) -> bool {
        self.tx.send((kind, cmd.to_string())).is_ok()
    }
}

pub fn spawn(
    id: usize,
    generation: u64,
    host: String,
    port: u16,
    password: String,
    app: mpsc::UnboundedSender<AppEvent>,
) -> RconHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run(id, generation, host, port, password, rx, app));
    RconHandle { tx, generation }
}

async fn run(
    id: usize,
    generation: u64,
    host: String,
    port: u16,
    password: String,
    mut rx: mpsc::UnboundedReceiver<(ReqKind, String)>,
    app: mpsc::UnboundedSender<AppEvent>,
) {
    let emit = |ev: RconEvent| app.send(AppEvent::Rcon { id, generation, ev }).is_ok();
    let mut backoff = Duration::from_secs(2);
    loop {
        match RconClient::connect(&host, port, &password).await {
            Ok(mut client) => {
                backoff = Duration::from_secs(2);
                if !emit(RconEvent::Connected) {
                    return;
                }
                loop {
                    let Some((kind, cmd)) = rx.recv().await else {
                        return;
                    };
                    match client.command(&cmd).await {
                        Ok(body) => {
                            emit(RconEvent::Response { kind, cmd, result: Ok(body) });
                        }
                        Err(e) => {
                            emit(RconEvent::Response { kind, cmd, result: Err(e.to_string()) });
                            emit(RconEvent::Disconnected(e.to_string()));
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                if !emit(RconEvent::Disconnected(e.to_string())) {
                    return;
                }
                let sleep = tokio::time::sleep(backoff);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        m = rx.recv() => match m {
                            None => return,
                            Some((kind, cmd)) => {
                                emit(RconEvent::Response { kind, cmd, result: Err(crate::tr!("нет подключения RCON", "no RCON connection").into()) });
                            }
                        }
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}
