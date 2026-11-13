//! Фоновая служба: держит серверы запущенными, пока окно TUI закрыто.
//!
//! `mctui daemon` слушает 127.0.0.1 на случайном порту; порт и токен — в `daemon.json`
//! (права 0600). Окна подключаются, получают снимки состояния и строки логов, шлют команды.

use crate::config::Config;
use crate::core::Manager;
use crate::event::AppEvent;
use crate::proto::{self, Endpoint, Msg, Request};
use crate::tr;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

enum ClientEvent {
    Hello { cid: u64, tx: mpsc::UnboundedSender<String> },
    Req { req: Request },
    Gone { cid: u64 },
}

pub fn random_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 24];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok();
    if !ok {
        use std::hash::{BuildHasher, Hasher};
        for (i, chunk) in buf.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_usize(i);
            h.write_u128(
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(),
            );
            h.write_u32(std::process::id());
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_endpoint(path: &Path, ep: &Endpoint) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(ep)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Уже работает другая служба с этим конфигом?
pub async fn alive(config_path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(proto::endpoint_path(config_path)) else { return false };
    let Ok(ep) = serde_json::from_str::<Endpoint>(&text) else { return false };
    tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(("127.0.0.1", ep.port)))
        .await
        .is_ok_and(|r| r.is_ok())
}

pub async fn run(config_path: PathBuf) -> Result<()> {
    if alive(&config_path).await {
        anyhow::bail!(tr!("служба mctui уже запущена", "the mctui service is already running"));
    }
    let (cfg, config_path) = Config::load(Some(config_path))?;
    crate::i18n::init(cfg.language.as_deref());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("bind 127.0.0.1")?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let ep_path = proto::endpoint_path(&config_path);
    write_endpoint(
        &ep_path,
        &Endpoint { port, token: token.clone(), pid: std::process::id(), version: crate::update::VERSION.into() },
    )?;
    eprintln!("mctui daemon {} pid {} port {port}", crate::update::VERSION, std::process::id());

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let mut mgr = Manager::new(cfg, config_path.clone(), tx);
    let (ctx, mut crx) = mpsc::unbounded_channel::<ClientEvent>();
    let mut clients: HashMap<u64, mpsc::UnboundedSender<String>> = HashMap::new();
    let mut next_cid = 1u64;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut flush = tokio::time::interval(Duration::from_millis(100));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            Some(ev) = rx.recv() => mgr.on_event(ev),
            _ = tick.tick() => mgr.on_tick(),
            Ok((stream, _)) = listener.accept() => {
                let cid = next_cid;
                next_cid += 1;
                tokio::spawn(client_task(cid, stream, token.clone(), ctx.clone()));
            }
            Some(ce) = crx.recv() => match ce {
                ClientEvent::Hello { cid, tx } => {
                    send_welcome(&mgr, &tx);
                    clients.insert(cid, tx);
                }
                ClientEvent::Req { req, .. } => mgr.handle(req),
                ClientEvent::Gone { cid } => { clients.remove(&cid); }
            },
            _ = flush.tick() => broadcast(&mut mgr, &mut clients),
            _ = &mut shutdown, if !mgr.quitting => mgr.quit_stop_all(),
        }
        if mgr.should_exit {
            broadcast(&mut mgr, &mut clients);
            break;
        }
    }
    // Удаляем daemon.json, только если он наш (могли запустить новую службу).
    if let Ok(text) = std::fs::read_to_string(&ep_path)
        && serde_json::from_str::<Endpoint>(&text).is_ok_and(|e| e.pid == std::process::id())
    {
        let _ = std::fs::remove_file(&ep_path);
    }
    Ok(())
}

fn send_welcome(mgr: &Manager, tx: &mpsc::UnboundedSender<String>) {
    let _ = tx.send(proto::encode(&Msg::Welcome {
        version: crate::update::VERSION.into(),
        protocol: proto::PROTOCOL,
        pid: std::process::id(),
        config_path: mgr.config_path.clone(),
        language: mgr.language.clone(),
        can_detach: true,
    }));
    let _ = tx.send(proto::encode(&Msg::State { servers: mgr.servers.iter().map(|s| s.view()).collect() }));
    for s in &mgr.servers {
        let lines: Vec<String> = s.log.iter().map(|l| l.text.clone()).collect();
        let _ = tx.send(proto::encode(&Msg::Log { id: s.id, lines, reset: true }));
        let _ = tx.send(proto::encode(&Msg::History { id: s.id, samples: s.history.iter().copied().collect() }));
    }
}

fn broadcast(mgr: &mut Manager, clients: &mut HashMap<u64, mpsc::UnboundedSender<String>>) {
    let out = std::mem::take(&mut mgr.out);
    if clients.is_empty() {
        return;
    }
    let mut msgs: Vec<String> = vec![];
    // Строки логов группируем по серверу, сохраняя порядок.
    let mut batch: Vec<(usize, Vec<String>)> = vec![];
    for (id, line) in out.lines {
        match batch.last_mut() {
            Some((bid, v)) if *bid == id => v.push(line),
            _ => batch.push((id, vec![line])),
        }
    }
    for (id, lines) in batch {
        msgs.push(proto::encode(&Msg::Log { id, lines, reset: false }));
    }
    for (id, sample) in out.samples {
        msgs.push(proto::encode(&Msg::Sample { id, sample }));
    }
    if out.dirty {
        msgs.push(proto::encode(&Msg::State { servers: mgr.servers.iter().map(|s| s.view()).collect() }));
    }
    for (text, kind) in out.notices {
        msgs.push(proto::encode(&Msg::Notice { text, kind }));
    }
    clients.retain(|_, tx| msgs.iter().all(|m| tx.send(m.clone()).is_ok()));
}

async fn client_task(cid: u64, stream: TcpStream, token: String, ctx: mpsc::UnboundedSender<ClientEvent>) {
    let _ = stream.set_nodelay(true);
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    // Первое сообщение — Hello с токеном, иначе отключаем.
    let hello = tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await;
    let ok = match hello {
        Ok(Ok(Some(l))) => match serde_json::from_str::<Request>(&l) {
            Ok(Request::Hello { token: t, protocol, .. }) if t == token => {
                if protocol != proto::PROTOCOL {
                    let msg = Msg::Denied { reason: format!("protocol {protocol} != {}", proto::PROTOCOL) };
                    let _ = w.write_all(proto::encode(&msg).as_bytes()).await;
                    false
                } else {
                    true
                }
            }
            _ => false,
        },
        _ => false,
    };
    if !ok {
        return;
    }
    let (tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    if ctx.send(ClientEvent::Hello { cid, tx }).is_err() {
        return;
    }
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if w.write_all(m.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    while let Ok(Some(l)) = lines.next_line().await {
        if let Ok(req) = serde_json::from_str::<Request>(&l)
            && ctx.send(ClientEvent::Req { req }).is_err()
        {
            break;
        }
    }
    writer.abort();
    let _ = ctx.send(ClientEvent::Gone { cid });
}

/// SIGTERM (выключение компьютера, launchd/systemd) — штатно остановить серверы и выйти.
/// SIGHUP (закрыли терминал, из которого запустили) — игнорируем.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int), Ok(mut hup)) =
            (signal(SignalKind::terminate()), signal(SignalKind::interrupt()), signal(SignalKind::hangup()))
        else {
            return std::future::pending().await;
        };
        loop {
            tokio::select! {
                _ = term.recv() => return,
                _ = int.recv() => return,
                _ = hup.recv() => continue,
            }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
        let (Ok(mut close), Ok(mut shut), Ok(mut c), Ok(mut brk)) =
            (ctrl_close(), ctrl_shutdown(), ctrl_c(), ctrl_break())
        else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = close.recv() => {}
            _ = shut.recv() => {}
            _ = c.recv() => {}
            _ = brk.recv() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_config(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mctui-daemon-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("servers.toml")
    }

    /// Приём подключений как в [`run`], но без серверов: события клиентов уходят в тест.
    /// В `daemon.json` пишется `endpoint_token` — так можно проверить окно с неверным токеном.
    async fn fake_daemon(cfg: &Path, token: &str, endpoint_token: &str) -> mpsc::UnboundedReceiver<ClientEvent> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ep = Endpoint { port, token: endpoint_token.into(), pid: 1, version: "test".into() };
        write_endpoint(&proto::endpoint_path(cfg), &ep).unwrap();
        let (ctx, crx) = mpsc::unbounded_channel();
        let token = token.to_string();
        tokio::spawn(async move {
            let mut cid = 0;
            while let Ok((stream, _)) = listener.accept().await {
                cid += 1;
                tokio::spawn(client_task(cid, stream, token.clone(), ctx.clone()));
            }
        });
        crx
    }

    fn welcome() -> String {
        proto::encode(&Msg::Welcome {
            version: "test".into(),
            protocol: proto::PROTOCOL,
            pid: 1,
            config_path: "/x".into(),
            language: None,
            can_detach: true,
        })
    }

    async fn next(crx: &mut mpsc::UnboundedReceiver<ClientEvent>) -> ClientEvent {
        tokio::time::timeout(Duration::from_secs(5), crx.recv()).await.expect("event").expect("channel")
    }

    /// Окно (`client::oneshot`) ↔ служба: Hello с токеном из daemon.json → Welcome → запрос → отключение.
    #[tokio::test]
    async fn client_and_daemon_handshake() {
        let cfg = tmp_config("ok");
        let mut crx = fake_daemon(&cfg, "secret", "secret").await;
        let cfg2 = cfg.clone();
        let client = tokio::spawn(async move { crate::client::oneshot(&cfg2, Request::Start { id: 4 }).await });
        let ClientEvent::Hello { cid, tx } = next(&mut crx).await else { panic!("expected Hello") };
        tx.send(welcome()).unwrap();
        let got = client.await.unwrap().unwrap().expect("connected");
        assert!(matches!(got, Msg::Welcome { can_detach: true, .. }));
        assert!(matches!(next(&mut crx).await, ClientEvent::Req { req: Request::Start { id: 4 } }));
        assert!(matches!(next(&mut crx).await, ClientEvent::Gone { cid: c } if c == cid));
        let _ = std::fs::remove_dir_all(cfg.parent().unwrap());
    }

    /// Неверный токен: служба молча закрывает соединение, окну — «служба не запущена».
    #[tokio::test]
    async fn wrong_token_is_dropped() {
        let cfg = tmp_config("token");
        let mut crx = fake_daemon(&cfg, "secret", "guess").await;
        let got = crate::client::oneshot(&cfg, Request::Shutdown).await.unwrap();
        assert!(got.is_none());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(crx.try_recv().is_err(), "no client may be registered");
        let _ = std::fs::remove_dir_all(cfg.parent().unwrap());
    }

    /// Первая строка не Hello — отключение без регистрации; чужой протокол — Denied с причиной.
    #[tokio::test]
    async fn non_hello_and_protocol_mismatch() {
        let cfg = tmp_config("proto");
        let mut crx = fake_daemon(&cfg, "secret", "secret").await;
        let ep: Endpoint = serde_json::from_str(&std::fs::read_to_string(proto::endpoint_path(&cfg)).unwrap()).unwrap();
        let read_all = |mut s: TcpStream| async move {
            use tokio::io::AsyncReadExt;
            let mut out = String::new();
            let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out)).await;
            out
        };

        let mut s = TcpStream::connect(("127.0.0.1", ep.port)).await.unwrap();
        s.write_all(proto::encode(&Request::Shutdown).as_bytes()).await.unwrap();
        assert_eq!(read_all(s).await, "");

        let mut s = TcpStream::connect(("127.0.0.1", ep.port)).await.unwrap();
        let hello = Request::Hello { token: "secret".into(), version: "9.0.0".into(), protocol: 99 };
        s.write_all(proto::encode(&hello).as_bytes()).await.unwrap();
        let reply = read_all(s).await;
        match serde_json::from_str::<Msg>(reply.trim()).unwrap() {
            Msg::Denied { reason } => assert!(reason.contains("99"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert!(crx.try_recv().is_err(), "no client may be registered");
        let _ = std::fs::remove_dir_all(cfg.parent().unwrap());
    }

    #[test]
    fn endpoint_file_is_private_and_atomic() {
        let cfg = tmp_config("ep");
        let path = proto::endpoint_path(&cfg);
        let ep = Endpoint { port: 1234, token: random_token(), pid: 5, version: "x".into() };
        write_endpoint(&path, &ep).unwrap();
        let back: Endpoint = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((back.port, back.token.as_str()), (1234, ep.token.as_str()));
        assert!(!path.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(cfg.parent().unwrap());
    }

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
