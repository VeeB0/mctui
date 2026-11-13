//! Клиент протокола RCON (Source RCON, как в Minecraft).
//!
//! Пакет: `len:i32le | id:i32le | type:i32le | body | \0 | \0`, где `len` не включает себя.

use anyhow::{Result, bail};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub const TYPE_AUTH: i32 = 3;
pub const TYPE_COMMAND: i32 = 2;
#[cfg_attr(not(test), allow(dead_code))]
pub const TYPE_RESPONSE: i32 = 0;
const MAX_PACKET: i32 = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const CHUNK: usize = 4096;
const CONTINUATION_WAIT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub id: i32,
    pub kind: i32,
    pub body: String,
}

pub fn encode(p: &Packet) -> Vec<u8> {
    let body = p.body.as_bytes();
    let len = (4 + 4 + body.len() + 2) as i32;
    let mut out = Vec::with_capacity(len as usize + 4);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&p.id.to_le_bytes());
    out.extend_from_slice(&p.kind.to_le_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&[0, 0]);
    out
}

pub async fn read_packet<R: AsyncRead + Unpin>(r: &mut R) -> Result<Packet> {
    let len = r.read_i32_le().await?;
    if !(10..=MAX_PACKET).contains(&len) {
        bail!("некорректная длина пакета RCON: {len}");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let id = i32::from_le_bytes(buf[0..4].try_into().unwrap());
    let kind = i32::from_le_bytes(buf[4..8].try_into().unwrap());
    let body = &buf[8..];
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    Ok(Packet { id, kind, body: String::from_utf8_lossy(&body[..end]).into_owned() })
}

pub async fn write_packet<W: AsyncWrite + Unpin>(w: &mut W, p: &Packet) -> Result<()> {
    w.write_all(&encode(p)).await?;
    w.flush().await?;
    Ok(())
}

pub struct RconClient {
    stream: TcpStream,
    next_id: i32,
}

impl RconClient {
    pub async fn connect(host: &str, port: u16, password: &str) -> Result<Self> {
        let stream = timeout(IO_TIMEOUT, TcpStream::connect((host, port))).await??;
        stream.set_nodelay(true).ok();
        let mut c = Self { stream, next_id: 1 };
        let id = c.take_id();
        write_packet(&mut c.stream, &Packet { id, kind: TYPE_AUTH, body: password.into() }).await?;
        loop {
            let p = timeout(IO_TIMEOUT, read_packet(&mut c.stream)).await??;
            if p.id == -1 {
                bail!("неверный пароль RCON");
            }
            // Некоторые реализации сначала шлют пустой RESPONSE_VALUE — ждём AUTH_RESPONSE (type 2).
            if p.kind == TYPE_COMMAND && p.id == id {
                return Ok(c);
            }
        }
    }

    fn take_id(&mut self) -> i32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Выполняет команду.
    ///
    /// Minecraft режет длинные ответы на пакеты по 4096 байт с тем же id. Трюк с пакетом-маркером
    /// не годится: новые версии сервера закрывают соединение на пакете неизвестного типа.
    /// Поэтому после «полного» пакета ждём продолжение недолго.
    pub async fn command(&mut self, cmd: &str) -> Result<String> {
        let id = self.take_id();
        let p = Packet { id, kind: TYPE_COMMAND, body: cmd.into() };
        write_packet(&mut self.stream, &p).await?;
        let mut out = String::new();
        let mut wait = IO_TIMEOUT;
        loop {
            let p = match timeout(wait, read_packet(&mut self.stream)).await {
                Ok(p) => p?,
                Err(_) if wait == CONTINUATION_WAIT => return Ok(out),
                Err(e) => return Err(e.into()),
            };
            if p.id != id {
                continue;
            }
            let full = p.body.len() >= CHUNK - 16;
            out.push_str(&p.body);
            if !full {
                return Ok(out);
            }
            wait = CONTINUATION_WAIT;
        }
    }
}

#[cfg(test)]
pub mod mock {
    //! Мок-сервер RCON с поведением как у vanilla Minecraft.
    use super::*;
    use tokio::net::TcpListener;

    pub async fn spawn(password: &'static str, handler: fn(&str) -> String) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut authed = false;
                    while let Ok(p) = read_packet(&mut s).await {
                        let reply = match p.kind {
                            TYPE_AUTH => {
                                authed = p.body == password;
                                Packet { id: if authed { p.id } else { -1 }, kind: TYPE_COMMAND, body: String::new() }
                            }
                            TYPE_COMMAND if authed => {
                                let body = handler(&p.body);
                                // Длинный ответ — несколькими пакетами по 4096 байт, как у Minecraft.
                                let bytes = body.as_bytes();
                                for chunk in bytes.chunks(4096).take(bytes.len().div_ceil(4096).saturating_sub(1)) {
                                    let part = Packet {
                                        id: p.id,
                                        kind: TYPE_RESPONSE,
                                        body: String::from_utf8_lossy(chunk).into(),
                                    };
                                    write_packet(&mut s, &part).await.unwrap();
                                }
                                let last = bytes.chunks(4096).last().unwrap_or(&[]);
                                Packet { id: p.id, kind: TYPE_RESPONSE, body: String::from_utf8_lossy(last).into() }
                            }
                            _ => {
                                // Как Minecraft 26.x: ответить и закрыть соединение.
                                let p = Packet {
                                    id: p.id,
                                    kind: TYPE_RESPONSE,
                                    body: format!("Unknown request {:x}", p.kind),
                                };
                                let _ = write_packet(&mut s, &p).await;
                                break;
                            }
                        };
                        if write_packet(&mut s, &reply).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_layout() {
        let b = encode(&Packet { id: 7, kind: TYPE_COMMAND, body: "list".into() });
        assert_eq!(&b[0..4], &14i32.to_le_bytes());
        assert_eq!(&b[4..8], &7i32.to_le_bytes());
        assert_eq!(&b[8..12], &2i32.to_le_bytes());
        assert_eq!(&b[12..16], b"list");
        assert_eq!(&b[16..], &[0, 0]);
    }

    #[tokio::test]
    async fn decode_roundtrip() {
        let p = Packet { id: 42, kind: TYPE_RESPONSE, body: "Привет".into() };
        let bytes = encode(&p);
        let mut r = &bytes[..];
        assert_eq!(read_packet(&mut r).await.unwrap(), p);
    }

    #[tokio::test]
    async fn login_and_command() {
        let port = mock::spawn("secret", |c| format!("echo:{c}")).await;
        let mut c = RconClient::connect("127.0.0.1", port, "secret").await.unwrap();
        assert_eq!(c.command("list").await.unwrap(), "echo:list");
        assert_eq!(c.command("tps").await.unwrap(), "echo:tps");
    }

    #[tokio::test]
    async fn multi_packet_response() {
        let port = mock::spawn("p", |c| if c == "big" { "x".repeat(10_000) } else { "y".repeat(8192) }).await;
        let mut c = RconClient::connect("127.0.0.1", port, "p").await.unwrap();
        assert_eq!(c.command("big").await.unwrap().len(), 10_000);
        // Длина ровно кратна 4096: продолжения нет, выходим по короткому таймауту.
        assert_eq!(c.command("exact").await.unwrap().len(), 8192);
        assert_eq!(c.command("big").await.unwrap().len(), 10_000);
    }

    #[tokio::test]
    async fn many_commands_on_one_connection() {
        let port = mock::spawn("p", |c| format!("ok {c}")).await;
        let mut c = RconClient::connect("127.0.0.1", port, "p").await.unwrap();
        for i in 0..20 {
            assert_eq!(c.command(&format!("cmd{i}")).await.unwrap(), format!("ok cmd{i}"));
        }
    }

    #[tokio::test]
    async fn wrong_password() {
        let port = mock::spawn("secret", |c| c.to_string()).await;
        let err = RconClient::connect("127.0.0.1", port, "nope").await.err().unwrap();
        assert!(err.to_string().contains("пароль"));
    }
}
