//! Списки доступа сервера: whitelist.json, ops.json, banned-players.json, banned-ips.json.
//!
//! Два режима работы (выбирает интерфейс):
//! - сервер запущен — в консоль отправляется команда из [`command`] (сервер сам перепишет файл);
//! - сервер остановлен — файлы правятся напрямую через [`add`] / [`remove`].
//!
//! Файлы пишутся в том же виде, что и у vanilla/Paper (Gson с pretty printing): массив с отступом
//! в 2 пробела, тот же порядок ключей, HTML-символы экранированы как `\u0027`. Записи хранятся как
//! `serde_json::Value` (порядок ключей сохраняется — `preserve_order`), поэтому неизвестные поля
//! существующих записей не теряются: добавляются и удаляются только целые записи.
//!
//! UUID: online-mode — Mojang API через системный `curl` (как в `update.rs`), при недоступности
//! сети — `usercache.json` сервера; offline-mode — `UUID.nameUUIDFromBytes("OfflinePlayer:" + name)`.

use anyhow::{Context, Result, anyhow, bail};
use md5::{Digest, Md5};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Причина бана по умолчанию (как у vanilla).
pub const DEFAULT_REASON: &str = "Banned by an operator.";
/// Источник бана для записей, созданных не из игры.
pub const DEFAULT_SOURCE: &str = "Server";
/// Значение `expires` для бессрочного бана.
pub const FOREVER: &str = "forever";
/// Формат дат в banned-*.json: `2026-11-13 02:00:00 +0300`.
pub const DATE_FORMAT: &str = "%Y-%m-%d %H:%M:%S %z";
/// Уровень оператора, если в server.properties нет `op-permission-level`.
pub const DEFAULT_OP_LEVEL: u8 = 4;

/// Адреса поиска профиля по нику: основной и запасной (оба отвечают `{"id": "<32 hex>", "name": …}`,
/// 404 — такого игрока нет).
const PROFILE_URLS: [&str; 2] = [
    "https://api.minecraftservices.com/minecraft/profile/lookup/name/",
    "https://api.mojang.com/users/profiles/minecraft/",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    Whitelist,
    Ops,
    BannedPlayers,
    BannedIps,
}

impl List {
    pub const ALL: [List; 4] = [List::Whitelist, List::Ops, List::BannedPlayers, List::BannedIps];

    pub fn file_name(self) -> &'static str {
        match self {
            List::Whitelist => "whitelist.json",
            List::Ops => "ops.json",
            List::BannedPlayers => "banned-players.json",
            List::BannedIps => "banned-ips.json",
        }
    }

    pub fn path(self, server_dir: &Path) -> PathBuf {
        server_dir.join(self.file_name())
    }

    /// Название списка для интерфейса.
    pub fn title(self) -> &'static str {
        match self {
            List::Whitelist => crate::tr!("Белый список", "Whitelist"),
            List::Ops => crate::tr!("Операторы", "Operators"),
            List::BannedPlayers => crate::tr!("Забаненные игроки", "Banned players"),
            List::BannedIps => crate::tr!("Забаненные IP", "Banned IPs"),
        }
    }

    /// Записи списка — IP-адреса, а не игроки.
    pub fn is_ip(self) -> bool {
        self == List::BannedIps
    }

    /// Список поддерживает причину (баны).
    pub fn has_reason(self) -> bool {
        matches!(self, List::BannedPlayers | List::BannedIps)
    }

    /// Ключ, по которому запись видна в интерфейсе: `ip` для банов IP, иначе `name`.
    fn key(self) -> &'static str {
        if self.is_ip() { "ip" } else { "name" }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Ник игрока; для [`List::BannedIps`] — IP-адрес.
    pub name: String,
    pub uuid: Option<String>,
    /// Только для [`List::Ops`].
    pub level: Option<u8>,
    pub reason: Option<String>,
    pub created: Option<String>,
    pub expires: Option<String>,
    pub source: Option<String>,
}

impl Entry {
    fn from_json(list: List, v: &Value) -> Option<Entry> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let uuid = s("uuid");
        // Запись без ника (бывает в старых whitelist.json) показываем по UUID.
        let name = s(list.key()).or_else(|| if list.is_ip() { None } else { uuid.clone() })?;
        Some(Entry {
            name,
            uuid: if list.is_ip() { None } else { uuid },
            level: v.get("level").and_then(Value::as_u64).map(|l| l.min(u8::MAX as u64) as u8),
            reason: s("reason"),
            created: s("created"),
            expires: s("expires"),
            source: s("source"),
        })
    }
}

/// Профиль игрока: UUID с дефисами в нижнем регистре и ник (в online-mode — в написании Mojang).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub uuid: String,
    pub name: String,
}

/// Записи списка; нет файла (или он пуст) — пустой список. Сортировка по имени без учёта регистра.
pub fn read(server_dir: &Path, list: List) -> Result<Vec<Entry>> {
    let mut entries: Vec<Entry> =
        load_raw(&list.path(server_dir))?.iter().filter_map(|v| Entry::from_json(list, v)).collect();
    entries.sort_by_cached_key(|e| e.name.to_lowercase());
    Ok(entries)
}

/// Есть ли запись в списке (ник без учёта регистра, UUID или IP).
#[cfg(test)]
pub fn contains(server_dir: &Path, list: List, name_or_ip: &str) -> Result<bool> {
    let entries = load_raw(&list.path(server_dir))?;
    Ok(entries.iter().any(|e| matches(list, e, name_or_ip.trim())))
}

/// Добавить запись в файл (сервер должен быть остановлен). Для игроков UUID определяется по
/// `online_mode`; если игрок уже есть — ошибка. Если в списке есть тот же UUID под старым ником
/// (игрок сменил ник), ник в записи обновляется.
pub fn add(server_dir: &Path, list: List, name_or_ip: &str, reason: Option<&str>, online_mode: bool) -> Result<()> {
    let target = name_or_ip.trim();
    validate(list, target)?;
    let path = list.path(server_dir);
    let mut entries = load_raw(&path)?;
    if entries.iter().any(|e| matches(list, e, target)) {
        bail!(already_present(list, target));
    }
    let created = || chrono::Local::now().format(DATE_FORMAT).to_string();
    let reason = clean_reason(reason).unwrap_or_else(|| DEFAULT_REASON.to_string());

    let mut m = Map::new();
    if list.is_ip() {
        m.insert("ip".into(), target.into());
        m.insert("created".into(), created().into());
        m.insert("source".into(), DEFAULT_SOURCE.into());
        m.insert("expires".into(), FOREVER.into());
        m.insert("reason".into(), reason.into());
    } else {
        let profile = resolve_for_server(server_dir, target, online_mode)?;
        if entries.iter().any(|e| matches(list, e, &profile.name)) {
            bail!(already_present(list, &profile.name));
        }
        let same_uuid = entries
            .iter_mut()
            .find(|e| e.get("uuid").and_then(Value::as_str).is_some_and(|u| u.eq_ignore_ascii_case(&profile.uuid)));
        if let Some(Value::Object(e)) = same_uuid {
            e.insert("name".into(), profile.name.into());
            return save_raw(&path, &entries);
        }
        m.insert("uuid".into(), profile.uuid.into());
        m.insert("name".into(), profile.name.into());
        match list {
            List::Ops => {
                m.insert("level".into(), op_level(server_dir).into());
                m.insert("bypassesPlayerLimit".into(), false.into());
            }
            List::BannedPlayers => {
                m.insert("created".into(), created().into());
                m.insert("source".into(), DEFAULT_SOURCE.into());
                m.insert("expires".into(), FOREVER.into());
                m.insert("reason".into(), reason.into());
            }
            List::Whitelist | List::BannedIps => {}
        }
    }
    entries.push(Value::Object(m));
    save_raw(&path, &entries)
}

/// Удалить запись из файла (сервер должен быть остановлен): ник без учёта регистра, UUID или IP.
pub fn remove(server_dir: &Path, list: List, name_or_ip: &str) -> Result<()> {
    let target = name_or_ip.trim();
    let path = list.path(server_dir);
    let mut entries = load_raw(&path)?;
    let before = entries.len();
    entries.retain(|e| !matches(list, e, target));
    if entries.len() == before {
        bail!(crate::tr!(
            format!("«{target}» нет в списке «{}»", list.title()),
            format!("\"{target}\" is not in the list \"{}\"", list.title())
        ));
    }
    save_raw(&path, &entries)
}

/// Консольная команда для запущенного сервера: `whitelist add Steve`, `op Steve`, `deop Steve`,
/// `ban Steve Griefing`, `pardon Steve`, `ban-ip 1.2.3.4`, `pardon-ip 1.2.3.4`.
/// Причина (только для банов) очищается от переводов строк; пустая — не добавляется.
pub fn command(list: List, add: bool, name_or_ip: &str, reason: Option<&str>) -> String {
    let target = name_or_ip.trim();
    let verb = match (list, add) {
        (List::Whitelist, true) => "whitelist add",
        (List::Whitelist, false) => "whitelist remove",
        (List::Ops, true) => "op",
        (List::Ops, false) => "deop",
        (List::BannedPlayers, true) => "ban",
        (List::BannedPlayers, false) => "pardon",
        (List::BannedIps, true) => "ban-ip",
        (List::BannedIps, false) => "pardon-ip",
    };
    match clean_reason(reason) {
        Some(r) if add && list.has_reason() => format!("{verb} {target} {r}"),
        _ => format!("{verb} {target}"),
    }
}

/// Включить/выключить белый список на запущенном сервере (`white-list` в server.properties
/// сервер перепишет сам).
pub fn whitelist_switch_command(enabled: bool) -> &'static str {
    if enabled { "whitelist on" } else { "whitelist off" }
}

/// UUID игрока: online-mode — из Mojang API, offline-mode — [`offline_uuid`].
#[cfg(test)]
pub fn resolve_uuid(name: &str, online_mode: bool) -> Result<String> {
    Ok(resolve_profile(name, online_mode)?.uuid)
}

/// Профиль игрока (UUID и ник). В online-mode ник берётся в написании Mojang (`notch` → `Notch`).
pub fn resolve_profile(name: &str, online_mode: bool) -> Result<Profile> {
    let name = name.trim();
    if !valid_name(name) {
        bail!(invalid_name(name));
    }
    if !online_mode {
        return Ok(Profile { uuid: offline_uuid(name), name: name.to_string() });
    }
    match fetch_profile(name)? {
        Some(p) => Ok(p),
        None => bail!(not_found(name)),
    }
}

/// Offline-UUID: v3 из MD5("OfflinePlayer:" + ник), как `UUID.nameUUIDFromBytes` в Java.
pub fn offline_uuid(name: &str) -> String {
    let mut b: [u8; 16] = Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    b[6] = (b[6] & 0x0f) | 0x30; // версия 3
    b[8] = (b[8] & 0x3f) | 0x80; // вариант IETF
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    dashed(&hex).expect("32 hex")
}

/// Ник Java Edition: 1–16 символов из `A–Z a–z 0–9 _` (новые ники — от 3, но старые бывают короче).
pub fn valid_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// IPv4 или IPv6 адрес (`ban-ip` принимает оба).
pub fn valid_ip(s: &str) -> bool {
    s.parse::<std::net::IpAddr>().is_ok()
}

// ---------------------------------------------------------------------------------------------
// Файлы

fn load_raw(path: &Path) -> Result<Vec<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| {
                crate::tr!(
                    format!("не удалось прочитать {}", path.display()),
                    format!("cannot read {}", path.display())
                )
            });
        }
    };
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<Value>>(text).with_context(|| {
        crate::tr!(
            format!("{} повреждён (ожидался JSON-массив)", path.display()),
            format!("{} is malformed (expected a JSON array)", path.display())
        )
    })
}

/// Запись в формате Gson (`setPrettyPrinting`, HTML-экранирование включено), атомарно через tmp.
fn save_raw(path: &Path, entries: &[Value]) -> Result<()> {
    let text = to_gson(entries);
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, path)
    };
    write().with_context(|| {
        crate::tr!(format!("не удалось записать {}", path.display()), format!("cannot write {}", path.display()))
    })
}

fn to_gson(entries: &[Value]) -> String {
    let json = serde_json::to_string_pretty(entries).expect("Value serializes");
    // Эти символы встречаются только внутри строк (serde_json их не экранирует), поэтому
    // замена по всему тексту безопасна. Gson пишет их как \u00XX.
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' | '>' | '&' | '=' | '\'' | '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push(c),
        }
    }
    out
}

fn matches(list: List, e: &Value, target: &str) -> bool {
    let field = |k: &str| e.get(k).and_then(Value::as_str);
    let eq = |k: &str| field(k).is_some_and(|v| v.eq_ignore_ascii_case(target));
    eq(list.key()) || (!list.is_ip() && eq("uuid"))
}

fn validate(list: List, target: &str) -> Result<()> {
    if list.is_ip() {
        if !valid_ip(target) {
            bail!(crate::tr!(format!("«{target}» — не IP-адрес"), format!("\"{target}\" is not a valid IP address")));
        }
    } else if !valid_name(target) {
        bail!(invalid_name(target));
    }
    Ok(())
}

fn clean_reason(reason: Option<&str>) -> Option<String> {
    let r = reason?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!r.is_empty()).then_some(r)
}

/// `op-permission-level` из server.properties (1–4), иначе 4.
fn op_level(server_dir: &Path) -> u8 {
    crate::properties::Properties::load(&server_dir.join("server.properties"))
        .ok()
        .and_then(|p| p.get("op-permission-level").and_then(|v| v.trim().parse::<u8>().ok()))
        .filter(|l| (1..=4).contains(l))
        .unwrap_or(DEFAULT_OP_LEVEL)
}

fn already_present(list: List, name: &str) -> String {
    crate::tr!(
        format!("«{name}» уже есть в списке «{}»", list.title()),
        format!("\"{name}\" is already in the list \"{}\"", list.title())
    )
}

fn invalid_name(name: &str) -> String {
    crate::tr!(
        format!("«{name}» — недопустимый ник (1–16 символов: латиница, цифры, _)"),
        format!("\"{name}\" is not a valid player name (1–16 chars: letters, digits, _)")
    )
}

fn not_found(name: &str) -> String {
    crate::tr!(
        format!("игрок «{name}» не найден в Mojang (проверьте ник или включите offline-mode)"),
        format!("player \"{name}\" not found at Mojang (check the name or use offline mode)")
    )
}

// ---------------------------------------------------------------------------------------------
// UUID

/// 32 hex (с дефисами или без) → `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` в нижнем регистре.
fn dashed(id: &str) -> Option<String> {
    let hex: String = id.chars().filter(|c| *c != '-').collect::<String>().to_ascii_lowercase();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

/// Профиль для записи в файл сервера: при недоступном Mojang в online-mode пробуем usercache.json.
fn resolve_for_server(server_dir: &Path, name: &str, online_mode: bool) -> Result<Profile> {
    if !online_mode {
        return resolve_profile(name, false);
    }
    match fetch_profile(name) {
        Ok(Some(p)) => Ok(p),
        Ok(None) => bail!(not_found(name)),
        Err(e) => cached_profile(server_dir, name, true).ok_or(e),
    }
}

/// Профиль из usercache.json сервера, если версия UUID соответствует режиму (online — v4, offline — v3).
fn cached_profile(server_dir: &Path, name: &str, online_mode: bool) -> Option<Profile> {
    let text = std::fs::read_to_string(server_dir.join("usercache.json")).ok()?;
    let cache: Vec<Value> = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let want = if online_mode { '4' } else { '3' };
    cache.iter().find_map(|e| {
        let n = e.get("name")?.as_str()?;
        let uuid = dashed(e.get("uuid")?.as_str()?)?;
        (n.eq_ignore_ascii_case(name) && uuid.chars().nth(14) == Some(want))
            .then(|| Profile { uuid, name: n.to_string() })
    })
}

fn curl() -> Command {
    let mut c = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    c.args(["-sS", "--connect-timeout", "5", "--max-time", "10", "-H", "Accept: application/json"]);
    c
}

/// Профиль из Mojang API: `Ok(None)` — такого игрока нет, `Err` — сеть/API недоступны.
fn fetch_profile(name: &str) -> Result<Option<Profile>> {
    if !valid_name(name) {
        bail!(invalid_name(name));
    }
    let mut last_err = String::new();
    for base in PROFILE_URLS {
        // Ник уже проверен valid_name — в URL безопасен.
        let out = curl().args(["-w", "\n%{http_code}"]).arg(format!("{base}{name}")).output();
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                return Err(anyhow!(e)).context(crate::tr!("не удалось запустить curl", "failed to run curl"));
            }
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_ref()));
        match code.trim() {
            "200" => {
                if let Some(p) = parse_profile(body) {
                    return Ok(Some(p));
                }
                last_err = format!("unexpected response: {}", body.trim());
            }
            "204" | "404" => return Ok(None),
            "429" => last_err = "rate limited (HTTP 429)".into(),
            c if out.status.success() => last_err = format!("HTTP {c}"),
            _ => last_err = String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }
    }
    bail!(crate::tr!(format!("Mojang API недоступен ({last_err})"), format!("Mojang API is unavailable ({last_err})")))
}

fn parse_profile(body: &str) -> Option<Profile> {
    let v: Value = serde_json::from_str(body).ok()?;
    Some(Profile { uuid: dashed(v.get("id")?.as_str()?)?, name: v.get("name")?.as_str()?.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pechka-access-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn file(dir: &Path, list: List) -> String {
        std::fs::read_to_string(list.path(dir)).unwrap()
    }

    /// `created` каждой записи (для сравнения с ожидаемым текстом) + проверка формата.
    fn created(dir: &Path, list: List) -> Vec<String> {
        let v: Vec<Value> = serde_json::from_str(&file(dir, list)).unwrap();
        v.iter()
            .map(|e| {
                let c = e["created"].as_str().unwrap().to_string();
                assert!(chrono::DateTime::parse_from_str(&c, DATE_FORMAT).is_ok(), "bad created: {c}");
                c
            })
            .collect()
    }

    #[test]
    fn offline_uuid_known_values() {
        // Сверено с Java UUID.nameUUIDFromBytes / Python uuid (md5 + биты версии 3).
        assert_eq!(offline_uuid("Notch"), "b50ad385-829d-3141-a216-7e7d7539ba7f");
        assert_eq!(offline_uuid("Steve"), "5627dd98-e6be-3c21-b8a8-e92344183641");
        assert_eq!(offline_uuid("jeb_"), "a762f560-4fce-3236-812a-b80efff0b62b");
        assert_eq!(resolve_uuid("Notch", false).unwrap(), "b50ad385-829d-3141-a216-7e7d7539ba7f");
    }

    #[test]
    fn dashed_ids() {
        assert_eq!(dashed("069A79F444E94726A5BEFCA90E38AAF5").unwrap(), "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        assert_eq!(dashed("069a79f4-44e9-4726-a5be-fca90e38aaf5").unwrap(), "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        assert_eq!(dashed("xyz"), None);
        assert_eq!(
            parse_profile("{\n  \"id\" : \"069a79f444e94726a5befca90e38aaf5\",\n  \"name\" : \"Notch\"\n}"),
            Some(Profile { uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".into(), name: "Notch".into() })
        );
    }

    #[test]
    fn whitelist_round_trip() {
        let d = tmp("wl");
        assert!(read(&d, List::Whitelist).unwrap().is_empty());
        add(&d, List::Whitelist, "Steve", None, false).unwrap();
        add(&d, List::Whitelist, " Notch ", None, false).unwrap();
        assert_eq!(
            file(&d, List::Whitelist),
            r#"[
  {
    "uuid": "5627dd98-e6be-3c21-b8a8-e92344183641",
    "name": "Steve"
  },
  {
    "uuid": "b50ad385-829d-3141-a216-7e7d7539ba7f",
    "name": "Notch"
  }
]"#
        );
        let names: Vec<_> = read(&d, List::Whitelist).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Notch", "Steve"]);
        let err = add(&d, List::Whitelist, "steve", None, false).unwrap_err().to_string();
        assert!(err.contains("steve"), "{err}");
        assert!(contains(&d, List::Whitelist, "STEVE").unwrap());
        remove(&d, List::Whitelist, "STEVE").unwrap();
        assert!(remove(&d, List::Whitelist, "Steve").is_err());
        remove(&d, List::Whitelist, "b50ad385-829d-3141-a216-7e7d7539ba7f").unwrap();
        assert_eq!(file(&d, List::Whitelist), "[]");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ops_round_trip_and_level() {
        let d = tmp("ops");
        add(&d, List::Ops, "Steve", None, false).unwrap();
        std::fs::write(d.join("server.properties"), "op-permission-level=2\n").unwrap();
        add(&d, List::Ops, "Alex", None, false).unwrap();
        assert_eq!(
            file(&d, List::Ops),
            format!(
                r#"[
  {{
    "uuid": "5627dd98-e6be-3c21-b8a8-e92344183641",
    "name": "Steve",
    "level": 4,
    "bypassesPlayerLimit": false
  }},
  {{
    "uuid": "{}",
    "name": "Alex",
    "level": 2,
    "bypassesPlayerLimit": false
  }}
]"#,
                offline_uuid("Alex")
            )
        );
        let ops = read(&d, List::Ops).unwrap();
        assert_eq!((ops[0].name.as_str(), ops[0].level), ("Alex", Some(2)));
        remove(&d, List::Ops, "alex").unwrap();
        remove(&d, List::Ops, "steve").unwrap();
        assert_eq!(file(&d, List::Ops), "[]");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn banned_players_round_trip() {
        let d = tmp("bp");
        add(&d, List::BannedPlayers, "Steve", None, false).unwrap();
        add(&d, List::BannedPlayers, "Griefer_1", Some("  Griefing\nspawn  "), false).unwrap();
        let c = created(&d, List::BannedPlayers);
        assert_eq!(
            file(&d, List::BannedPlayers),
            format!(
                r#"[
  {{
    "uuid": "5627dd98-e6be-3c21-b8a8-e92344183641",
    "name": "Steve",
    "created": "{}",
    "source": "Server",
    "expires": "forever",
    "reason": "Banned by an operator."
  }},
  {{
    "uuid": "{}",
    "name": "Griefer_1",
    "created": "{}",
    "source": "Server",
    "expires": "forever",
    "reason": "Griefing spawn"
  }}
]"#,
                c[0],
                offline_uuid("Griefer_1"),
                c[1]
            )
        );
        let e = &read(&d, List::BannedPlayers).unwrap()[0];
        assert_eq!(e.name, "Griefer_1");
        assert_eq!(e.reason.as_deref(), Some("Griefing spawn"));
        assert_eq!(e.expires.as_deref(), Some("forever"));
        assert_eq!(e.source.as_deref(), Some("Server"));
        remove(&d, List::BannedPlayers, "griefer_1").unwrap();
        remove(&d, List::BannedPlayers, "Steve").unwrap();
        assert_eq!(file(&d, List::BannedPlayers), "[]");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn banned_ips_round_trip() {
        let d = tmp("bip");
        add(&d, List::BannedIps, "1.2.3.4", Some("Don't <spam> & flood"), false).unwrap();
        assert!(add(&d, List::BannedIps, "1.2.3.4", None, false).is_err());
        assert!(add(&d, List::BannedIps, "1.2.3", None, false).is_err());
        let c = created(&d, List::BannedIps);
        // Gson экранирует ' < > & как \u00XX.
        assert_eq!(
            file(&d, List::BannedIps),
            format!(
                r#"[
  {{
    "ip": "1.2.3.4",
    "created": "{}",
    "source": "Server",
    "expires": "forever",
    "reason": "Don\u0027t \u003cspam\u003e \u0026 flood"
  }}
]"#,
                c[0]
            )
        );
        let e = &read(&d, List::BannedIps).unwrap()[0];
        assert_eq!((e.name.as_str(), e.uuid.as_deref()), ("1.2.3.4", None));
        assert_eq!(e.reason.as_deref(), Some("Don't <spam> & flood"));
        remove(&d, List::BannedIps, "1.2.3.4").unwrap();
        assert_eq!(file(&d, List::BannedIps), "[]");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn real_world_files_keep_unknown_fields() {
        let d = tmp("real");
        // Как пишут сторонние инструменты: свой отступ, BOM, лишние поля, другой порядок.
        std::fs::write(
            List::Ops.path(&d),
            "\u{feff}[{\"uuid\":\"069a79f4-44e9-4726-a5be-fca90e38aaf5\",\"name\":\"Notch\",\"level\":3,\
             \"bypassesPlayerLimit\":true,\"x-note\":{\"by\":\"panel\"}},\
             {\"name\":\"zed\",\"uuid\":\"00000000-0000-3000-8000-000000000000\",\"level\":4,\"bypassesPlayerLimit\":false}]",
        )
        .unwrap();
        let ops = read(&d, List::Ops).unwrap();
        assert_eq!(ops.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["Notch", "zed"]);
        assert_eq!(ops[0].level, Some(3));
        add(&d, List::Ops, "Steve", None, false).unwrap();
        remove(&d, List::Ops, "ZED").unwrap();
        assert_eq!(
            file(&d, List::Ops),
            r#"[
  {
    "uuid": "069a79f4-44e9-4726-a5be-fca90e38aaf5",
    "name": "Notch",
    "level": 3,
    "bypassesPlayerLimit": true,
    "x-note": {
      "by": "panel"
    }
  },
  {
    "uuid": "5627dd98-e6be-3c21-b8a8-e92344183641",
    "name": "Steve",
    "level": 4,
    "bypassesPlayerLimit": false
  }
]"#
        );

        // Пустой файл — пустой список; мусор — понятная ошибка.
        std::fs::write(List::Whitelist.path(&d), "").unwrap();
        assert!(read(&d, List::Whitelist).unwrap().is_empty());
        std::fs::write(List::BannedIps.path(&d), "{not json").unwrap();
        assert!(read(&d, List::BannedIps).is_err());
        assert!(add(&d, List::BannedIps, "1.2.3.4", None, false).is_err());
        assert_eq!(file(&d, List::BannedIps), "{not json", "malformed file must not be overwritten");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn same_uuid_renamed_player_updates_name() {
        let d = tmp("rename");
        let uuid = offline_uuid("NewName");
        std::fs::write(
            List::Whitelist.path(&d),
            format!("[\n  {{\n    \"uuid\": \"{uuid}\",\n    \"name\": \"OldName\"\n  }}\n]"),
        )
        .unwrap();
        add(&d, List::Whitelist, "NewName", None, false).unwrap();
        assert_eq!(
            file(&d, List::Whitelist),
            format!("[\n  {{\n    \"uuid\": \"{uuid}\",\n    \"name\": \"NewName\"\n  }}\n]")
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn usercache_fallback() {
        let d = tmp("cache");
        std::fs::write(
            d.join("usercache.json"),
            r#"[{"name":"Notch","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","expiresOn":"2026-12-13 02:00:00 +0300"},
                {"name":"Steve","uuid":"5627dd98-e6be-3c21-b8a8-e92344183641","expiresOn":"2026-12-13 02:00:00 +0300"}]"#,
        )
        .unwrap();
        assert_eq!(cached_profile(&d, "notch", true).unwrap().uuid, "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        assert_eq!(cached_profile(&d, "notch", true).unwrap().name, "Notch");
        // Offline-UUID (v3) не подходит для online-mode и наоборот.
        assert!(cached_profile(&d, "Steve", true).is_none());
        assert!(cached_profile(&d, "Steve", false).is_some());
        assert!(cached_profile(&d, "Notch", false).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn commands() {
        assert_eq!(command(List::Whitelist, true, "Steve", None), "whitelist add Steve");
        assert_eq!(command(List::Whitelist, false, "Steve", Some("x")), "whitelist remove Steve");
        assert_eq!(command(List::Ops, true, "Steve", None), "op Steve");
        assert_eq!(command(List::Ops, false, " Steve ", None), "deop Steve");
        assert_eq!(command(List::BannedPlayers, true, "Steve", Some("Griefing")), "ban Steve Griefing");
        assert_eq!(command(List::BannedPlayers, true, "Steve", Some(" two\nlines ")), "ban Steve two lines");
        assert_eq!(command(List::BannedPlayers, true, "Steve", Some("   ")), "ban Steve");
        assert_eq!(command(List::BannedPlayers, false, "Steve", Some("r")), "pardon Steve");
        assert_eq!(command(List::BannedIps, true, "1.2.3.4", None), "ban-ip 1.2.3.4");
        assert_eq!(command(List::BannedIps, true, "1.2.3.4", Some("Spam")), "ban-ip 1.2.3.4 Spam");
        assert_eq!(command(List::BannedIps, false, "1.2.3.4", None), "pardon-ip 1.2.3.4");
        assert_eq!(whitelist_switch_command(true), "whitelist on");
        assert_eq!(whitelist_switch_command(false), "whitelist off");
    }

    #[test]
    fn validation() {
        for ok in ["Steve", "jeb_", "a", "Abcdefghijklmnop", "__007__"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "Abcdefghijklmnopq", "Ste ve", "Стив", "a-b", "a.b", ".Bedrock"] {
            assert!(!valid_name(bad), "{bad}");
        }
        for ok in ["1.2.3.4", "255.255.255.255", "::1", "2001:db8::1"] {
            assert!(valid_ip(ok), "{ok}");
        }
        for bad in ["", "1.2.3", "256.1.1.1", "1.2.3.4:25565", "example.com", " 1.2.3.4"] {
            assert!(!valid_ip(bad), "{bad}");
        }
        let d = tmp("val");
        assert!(add(&d, List::Whitelist, "bad name", None, false).is_err());
        let _ = std::fs::remove_dir_all(&d);
        assert!(resolve_uuid("bad name", true).is_err());
    }

    #[test]
    #[ignore = "network: Mojang API"]
    fn access_resolve_online() {
        assert_eq!(resolve_uuid("Notch", true).unwrap(), "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        assert_eq!(resolve_profile("notch", true).unwrap().name, "Notch");
        assert!(resolve_profile("zzzz_no_such_q9", true).is_err());
    }
}
