//! Статистика игроков.
//!
//! 1. Из файлов сервера ([`load`]) — работает и для прошлого, даже если pechka тогда не следил за сервером:
//!    `usercache.json` (ник ↔ UUID), статистика vanilla `<level>/players/stats/<uuid>.json` (26.x) или
//!    `<level>/stats/<uuid>.json` (раньше), данные игрока `<level>/players/data/<uuid>.dat` /
//!    `<level>/playerdata/<uuid>.dat` (время последнего входа Bukkit/Paper или время изменения файла).
//! 2. Журнал сессий ([`SessionLog`]) — ведёт фоновая служба по событиям входа/выхода: время онлайн
//!    сегодня / за неделю, пик игроков по дням. Хранится в JSON, только последние [`KEEP_DAYS`] дней.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::world::nbt;

// ---------- Статистика из файлов сервера ----------

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerStats {
    /// Ник (из usercache.json или данных Bukkit); если неизвестен — UUID.
    pub name: String,
    pub uuid: String,
    /// Наиграно всего (`minecraft:play_time`, раньше `play_one_minute`; в тиках → секунды).
    pub play_time_secs: u64,
    /// Последний вход / выход (Bukkit `lastPlayed`, Paper `LastSeen`) или время изменения файлов игрока.
    pub last_seen: Option<SystemTime>,
    /// Первый вход (только Bukkit / Paper).
    pub first_played: Option<SystemTime>,
    pub deaths: u64,
    pub mob_kills: u64,
    pub player_kills: u64,
    /// Пройдено (все способы передвижения `*_one_cm`, кроме падения), км.
    pub distance_km: f64,
    pub jumps: u64,
    /// Сколько раз выходил с сервера.
    pub leave_game: u64,
    /// Добыто блоков (сумма `minecraft:mined`).
    pub blocks_mined: u64,
}

impl PlayerStats {
    pub fn play_time(&self) -> Duration {
        Duration::from_secs(self.play_time_secs)
    }
}

/// Игроки мира `level_name` по наигранному времени (больше — выше).
/// Где служба хранит журнал сессий сервера (окно читает его оттуда же).
pub fn session_log_path(config_path: &Path, server_name: &str) -> PathBuf {
    crate::config::state_dir(config_path)
        .join("sessions")
        .join(format!("{}.json", crate::backup::file_prefix(server_name)))
}

pub fn load(dir: &Path, level_name: &str) -> Vec<PlayerStats> {
    let level = dir.join(level_name);
    let names = usercache(dir);
    let stats_dirs = [level.join("players/stats"), level.join("stats")];
    let data_dirs = [level.join("players/data"), level.join("playerdata")];
    let mut uuids = BTreeSet::new();
    for d in stats_dirs.iter().chain(&data_dirs) {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            if let Some(u) = f.strip_suffix(".json").or_else(|| f.strip_suffix(".dat"))
                && is_uuid(u)
            {
                uuids.insert(u.to_lowercase());
            }
        }
    }
    let mut out: Vec<PlayerStats> = uuids
        .into_iter()
        .map(|uuid| {
            let mut p = PlayerStats { uuid: uuid.clone(), ..Default::default() };
            let mut mtimes = vec![];
            if let Some(path) = stats_dirs.iter().map(|d| d.join(format!("{uuid}.json"))).find(|p| p.is_file()) {
                mtimes.push(mtime(&path));
                if let Some(v) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()) {
                    apply_stats(&mut p, &v);
                }
            }
            let mut known_name = None;
            if let Some(path) = data_dirs.iter().map(|d| d.join(format!("{uuid}.dat"))).find(|p| p.is_file()) {
                mtimes.push(mtime(&path));
                if let Some(tag) = nbt::read_file(&path) {
                    let ms = |k: &str| {
                        tag.find_long(k)
                            .filter(|&v| v > 0)
                            .map(|v| SystemTime::UNIX_EPOCH + Duration::from_millis(v as u64))
                    };
                    p.last_seen = [ms("lastPlayed"), ms("LastSeen")].into_iter().flatten().max();
                    p.first_played = ms("firstPlayed");
                    known_name = tag.find_str("lastKnownName").map(str::to_string);
                }
            }
            if p.last_seen.is_none() {
                p.last_seen = mtimes.into_iter().flatten().max();
            }
            p.name = names.get(&uuid).cloned().or(known_name).unwrap_or_else(|| uuid.clone());
            p
        })
        .collect();
    out.sort_by(|a, b| {
        b.play_time_secs.cmp(&a.play_time_secs).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// UUID (в нижнем регистре) → ник из `usercache.json`.
pub fn usercache(dir: &Path) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct Entry {
        name: String,
        uuid: String,
    }
    let text = std::fs::read_to_string(dir.join("usercache.json")).unwrap_or_default();
    serde_json::from_str::<Vec<Entry>>(text.trim_start_matches('\u{feff}'))
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.uuid.to_lowercase(), e.name))
        .collect()
}

/// Заполнить счётчики из JSON статистики: формат 1.13+ (`stats → minecraft:custom → minecraft:play_time`)
/// или старый плоский (`stat.playOneMinute`).
fn apply_stats(p: &mut PlayerStats, v: &Value) {
    let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    if let Some(stats) = v.get("stats") {
        let custom = stats.get("minecraft:custom");
        let c = |k: &str| n(custom.and_then(|m| m.get(format!("minecraft:{k}"))));
        let ticks = custom.and_then(|m| m.get("minecraft:play_time").or_else(|| m.get("minecraft:play_one_minute")));
        p.play_time_secs = n(ticks) / 20;
        p.deaths = c("deaths");
        p.mob_kills = c("mob_kills");
        p.player_kills = c("player_kills");
        p.jumps = c("jump");
        p.leave_game = c("leave_game");
        let cm: u64 = custom
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| k.ends_with("_one_cm") && *k != "minecraft:fall_one_cm")
                    .map(|(_, v)| n(Some(v)))
                    .sum()
            })
            .unwrap_or(0);
        p.distance_km = cm as f64 / 100_000.0;
        p.blocks_mined = stats
            .get("minecraft:mined")
            .and_then(Value::as_object)
            .map(|m| m.values().map(|v| n(Some(v))).sum())
            .unwrap_or(0);
    } else if let Some(m) = v.as_object() {
        let c = |k: &str| n(m.get(&format!("stat.{k}")));
        p.play_time_secs = c("playOneMinute") / 20;
        p.deaths = c("deaths");
        p.mob_kills = c("mobKills");
        p.player_kills = c("playerKills");
        p.jumps = c("jump");
        p.leave_game = c("leaveGame");
        let cm: u64 = m
            .iter()
            .filter(|(k, _)| k.starts_with("stat.") && k.ends_with("OneCm") && *k != "stat.fallOneCm")
            .map(|(_, v)| n(Some(v)))
            .sum();
        p.distance_km = cm as f64 / 100_000.0;
        p.blocks_mined = m.iter().filter(|(k, _)| k.starts_with("stat.mineBlock.")).map(|(_, v)| n(Some(v))).sum();
    }
}

// ---------- Журнал сессий ----------

/// Сколько дней истории хранить в журнале.
pub const KEEP_DAYS: i64 = 90;
/// Предел числа сессий в файле (на случай «мигающих» подключений).
const MAX_SESSIONS: usize = 50_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    /// Unix-время входа, с.
    pub start: i64,
    /// Unix-время выхода, с; `None` — игрок ещё на сервере.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
}

impl Session {
    pub fn started(&self) -> DateTime<Utc> {
        utc(self.start)
    }
    pub fn ended(&self) -> Option<DateTime<Utc>> {
        self.end.map(utc)
    }
    /// Длительность; открытая сессия считается до `now`.
    pub fn duration(&self, now: i64) -> Duration {
        Duration::from_secs(self.end.unwrap_or(now).saturating_sub(self.start).max(0) as u64)
    }
    /// Сколько секунд сессии попало в окно `[from, to)`.
    fn overlap(&self, from: i64, to: i64, now: i64) -> u64 {
        let (s, e) = (self.start.max(from), self.end.unwrap_or(now).min(to));
        (e - s).max(0) as u64
    }
}

fn utc(ts: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(ts, 0).unwrap_or_default()
}

/// Время онлайн игрока по журналу.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerPlaytime {
    pub name: String,
    pub today: Duration,
    /// Сегодня и 6 предыдущих дней.
    pub week: Duration,
    /// За всю хранимую историю (до [`KEEP_DAYS`] дней).
    pub total: Duration,
    pub sessions: usize,
    /// На сервере с этого момента (`None` — не в игре).
    pub online_since: Option<DateTime<Utc>>,
    /// Последний выход (или вход, если ещё в игре).
    pub last_seen: Option<DateTime<Utc>>,
}

/// Сводка за календарный день (в часовом поясе `now`).
#[derive(Debug, Clone, PartialEq)]
pub struct DayStats {
    pub date: NaiveDate,
    /// Наибольшее число игроков одновременно.
    pub peak: u32,
    /// Разных игроков за день.
    pub unique_players: u32,
    /// Суммарное время всех игроков.
    pub play_time: Duration,
}

/// Журнал сессий игроков одного сервера. Изменения только в памяти — сохранять через [`SessionLog::save`].
///
/// Порядок работы в службе: при старте `load` → `recover()`; на «joined»/«left» — `record_join` /
/// `record_leave`; при остановке или падении сервера — `close_all`; периодически (раз в минуту) —
/// `heartbeat(now)` + `save`, чтобы после падения самой службы сессии закрылись временем последнего пульса.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionLog {
    #[serde(skip)]
    path: PathBuf,
    /// Последний момент, когда служба точно работала (Unix, с).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    heartbeat: Option<i64>,
    #[serde(default)]
    sessions: Vec<Session>,
}

impl SessionLog {
    #[cfg(test)]
    /// Пустой журнал, который будет сохраняться в `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), ..Default::default() }
    }

    /// Журнал из файла; нет файла — пустой. Повреждённый файл переименовывается в `*.corrupt`.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut log = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<SessionLog>(&text) {
                Ok(l) => l,
                Err(_) => {
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    SessionLog::default()
                }
            },
            Err(_) => SessionLog::default(),
        };
        log.path = path;
        log
    }

    /// Атомарная запись (через временный файл).
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?).with_context(|| tmp.display().to_string())?;
        std::fs::rename(&tmp, &self.path).with_context(|| self.path.display().to_string())?;
        Ok(())
    }

    /// Вход игрока. Повторный вход уже онлайн-игрока игнорируется (событие пришло и из лога, и из RCON).
    /// Возвращает `true`, если сессия открыта.
    pub fn record_join<Tz: TimeZone>(&mut self, name: &str, at: DateTime<Tz>) -> bool {
        let at = at.timestamp();
        self.prune(at);
        if self.open_index(name).is_some() {
            return false;
        }
        self.sessions.push(Session { name: name.to_string(), start: at, end: None });
        true
    }

    /// Выход игрока. Возвращает `true`, если была открытая сессия.
    pub fn record_leave<Tz: TimeZone>(&mut self, name: &str, at: DateTime<Tz>) -> bool {
        let at = at.timestamp();
        let Some(i) = self.open_index(name) else { return false };
        let s = &mut self.sessions[i];
        s.end = Some(at.max(s.start));
        self.prune(at);
        true
    }

    /// Сервер остановился или упал: закрыть все открытые сессии.
    pub fn close_all<Tz: TimeZone>(&mut self, at: DateTime<Tz>) {
        let at = at.timestamp();
        for s in self.sessions.iter_mut().filter(|s| s.end.is_none()) {
            s.end = Some(at.max(s.start));
        }
        self.prune(at);
    }

    /// Отметка «служба жива» — см. [`SessionLog::recover`].
    pub fn heartbeat<Tz: TimeZone>(&mut self, at: DateTime<Tz>) {
        self.heartbeat = Some(at.timestamp());
    }

    /// После перезапуска службы: сессии, оставшиеся открытыми, закрываются временем последнего пульса
    /// (а без него — моментом входа, то есть не засчитываются).
    pub fn recover(&mut self) {
        let hb = self.heartbeat;
        for s in self.sessions.iter_mut().filter(|s| s.end.is_none()) {
            s.end = Some(hb.unwrap_or(s.start).max(s.start));
        }
    }

    #[cfg(test)]
    /// Сессии игрока (ник без учёта регистра), старые первыми.
    pub fn sessions(&self, name: &str) -> Vec<Session> {
        self.sessions.iter().filter(|s| s.name.eq_ignore_ascii_case(name)).cloned().collect()
    }

    #[cfg(test)]
    pub fn all_sessions(&self) -> &[Session] {
        &self.sessions
    }

    #[cfg(test)]
    /// С какого момента игрок на сервере (`None` — не в игре).
    pub fn online_since(&self, name: &str) -> Option<DateTime<Utc>> {
        self.open_index(name).map(|i| self.sessions[i].started())
    }

    #[cfg(test)]
    /// Кто сейчас в игре.
    pub fn online(&self) -> Vec<String> {
        self.sessions.iter().filter(|s| s.end.is_none()).map(|s| s.name.clone()).collect()
    }

    fn open_index(&self, name: &str) -> Option<usize> {
        self.sessions.iter().rposition(|s| s.end.is_none() && s.name.eq_ignore_ascii_case(name))
    }

    /// Убрать сессии, закончившиеся больше [`KEEP_DAYS`] дней назад, и ограничить размер файла.
    pub fn prune(&mut self, now: i64) {
        let cutoff = now - KEEP_DAYS * 86_400;
        self.sessions.retain(|s| s.end.is_none_or(|e| e >= cutoff));
        if self.sessions.len() > MAX_SESSIONS {
            let mut excess = self.sessions.len() - MAX_SESSIONS;
            self.sessions.retain(|s| {
                if excess > 0 && s.end.is_some() {
                    excess -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Время игрока за сегодня (с полуночи в часовом поясе `now`).
    pub fn playtime_today<Tz: TimeZone>(&self, name: &str, now: DateTime<Tz>) -> Duration {
        self.playtime_days(name, now, 1)
    }

    /// Время игрока за `days` календарных дней, включая сегодняшний.
    pub fn playtime_days<Tz: TimeZone>(&self, name: &str, now: DateTime<Tz>, days: u32) -> Duration {
        let to = now.timestamp();
        let from = day_start(&now.timezone(), now.date_naive() - ChronoDuration::days(days as i64 - 1));
        let secs =
            self.sessions.iter().filter(|s| s.name.eq_ignore_ascii_case(name)).map(|s| s.overlap(from, to, to)).sum();
        Duration::from_secs(secs)
    }

    /// Все игроки журнала: сегодня / неделя / всего; сортировка по времени за неделю.
    pub fn summary<Tz: TimeZone>(&self, now: DateTime<Tz>) -> Vec<PlayerPlaytime> {
        let ts = now.timestamp();
        let mut by_name: HashMap<String, PlayerPlaytime> = HashMap::new();
        for s in &self.sessions {
            let e = by_name.entry(s.name.to_lowercase()).or_insert_with(|| PlayerPlaytime {
                name: s.name.clone(),
                today: Duration::ZERO,
                week: Duration::ZERO,
                total: Duration::ZERO,
                sessions: 0,
                online_since: None,
                last_seen: None,
            });
            e.name = s.name.clone(); // последнее написание ника
            e.sessions += 1;
            e.total += s.duration(ts);
            if s.end.is_none() {
                e.online_since = Some(s.started());
            }
            let seen = s.ended().unwrap_or_else(|| s.started());
            e.last_seen = e.last_seen.max(Some(seen));
        }
        let mut out: Vec<PlayerPlaytime> = by_name
            .into_values()
            .map(|mut p| {
                p.today = self.playtime_today(&p.name, now.clone());
                p.week = self.playtime_days(&p.name, now.clone(), 7);
                p
            })
            .collect();
        out.sort_by(|a, b| b.week.cmp(&a.week).then_with(|| b.total.cmp(&a.total)).then_with(|| a.name.cmp(&b.name)));
        out
    }

    /// Сводка по дням за `days` дней (старые первыми, сегодняшний — последний; пустые дни тоже есть).
    pub fn daily<Tz: TimeZone>(&self, now: DateTime<Tz>, days: u32) -> Vec<DayStats> {
        let tz = now.timezone();
        let ts = now.timestamp();
        let today = now.date_naive();
        (0..days as i64)
            .rev()
            .map(|back| {
                let date = today - ChronoDuration::days(back);
                let (from, to) = (day_start(&tz, date), day_start(&tz, date + ChronoDuration::days(1)));
                let mut events = vec![];
                let mut players = BTreeSet::new();
                let mut total = 0;
                for s in &self.sessions {
                    let end = s.end.unwrap_or(ts);
                    let (st, en) = (s.start.max(from), end.min(to));
                    // Мгновенная сессия (вход и выход в одну секунду) засчитывается дню, в котором была.
                    let inside = if end > s.start { st < en } else { (from..to).contains(&s.start) };
                    if !inside {
                        continue;
                    }
                    players.insert(s.name.to_lowercase());
                    total += (en - st).max(0) as u64;
                    if en > st {
                        events.push((st, 1i32));
                        events.push((en, -1));
                    }
                }
                // При равном времени сначала выходы, потом входы.
                events.sort();
                let (mut cur, mut peak) = (0i32, 0i32);
                for (_, d) in events {
                    cur += d;
                    peak = peak.max(cur);
                }
                DayStats {
                    date,
                    peak: peak as u32,
                    unique_players: players.len() as u32,
                    play_time: Duration::from_secs(total),
                }
            })
            .collect()
    }
}

/// Unix-время полуночи `date` в поясе `tz` (если полночь выпала на перевод часов — ближайший момент после).
fn day_start<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> i64 {
    (0..=3)
        .find_map(|h| tz.from_local_datetime(&date.and_hms_opt(h, 0, 0)?).earliest())
        .map(|d| d.timestamp())
        .unwrap_or_else(|| date.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use std::io::Write;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pechka-pstats-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const A: &str = "11111111-2222-4333-8444-555555555555";
    const B: &str = "aaaaaaaa-bbbb-3ccc-9ddd-eeeeeeeeeeee";
    const C: &str = "cccccccc-0000-4000-8000-000000000001";

    /// Данные игрока Bukkit: {bukkit: {lastKnownName, lastPlayed, firstPlayed}} в gzip.
    fn bukkit_dat(path: &Path, name: &str, last_ms: i64) {
        let s = |b: &mut Vec<u8>, v: &str| {
            b.extend((v.len() as u16).to_be_bytes());
            b.extend(v.as_bytes());
        };
        let mut b = vec![10u8];
        s(&mut b, "");
        b.push(10);
        s(&mut b, "bukkit");
        b.push(4);
        s(&mut b, "lastPlayed");
        b.extend(last_ms.to_be_bytes());
        b.push(4);
        s(&mut b, "firstPlayed");
        b.extend((last_ms - 1000).to_be_bytes());
        b.push(8);
        s(&mut b, "lastKnownName");
        s(&mut b, name);
        b.push(0);
        b.push(0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(path).unwrap(), flate2::Compression::fast());
        gz.write_all(&b).unwrap();
        gz.finish().unwrap();
    }

    #[test]
    fn stats_from_files_both_layouts_and_key_names() {
        let d = tmp("files");
        std::fs::write(d.join("usercache.json"), format!(r#"[{{"name":"Alice","uuid":"{A}","expiresOn":"x"}}]"#))
            .unwrap();
        // 26.x: players/stats, ключ play_time
        let st = d.join("world/players/stats");
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(
            st.join(format!("{A}.json")),
            r#"{"stats":{"minecraft:mined":{"minecraft:stone":10,"minecraft:dirt":5},
                "minecraft:custom":{"minecraft:play_time":72000,"minecraft:deaths":3,"minecraft:mob_kills":7,
                "minecraft:walk_one_cm":150000,"minecraft:fly_one_cm":50000,"minecraft:fall_one_cm":99999,
                "minecraft:jump":17,"minecraft:leave_game":4}},"DataVersion":5023}"#,
        )
        .unwrap();
        // старый vanilla: stats/, ключ play_one_minute; ник из данных Bukkit
        let old = d.join("world/stats");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(
            old.join(format!("{B}.json")),
            r#"{"stats":{"minecraft:custom":{"minecraft:play_one_minute":200000,"minecraft:player_kills":2}},"DataVersion":1500}"#,
        )
        .unwrap();
        bukkit_dat(&d.join(format!("world/playerdata/{B}.dat")), "Bob", 1_700_000_000_000);
        // до 1.13: плоский формат
        std::fs::write(
            old.join(format!("{C}.json")),
            r#"{"stat.playOneMinute":2000,"stat.walkOneCm":100000,"stat.deaths":1}"#,
        )
        .unwrap();
        // мусор игнорируется
        std::fs::write(old.join("not-a-uuid.json"), "{}").unwrap();
        let v = load(&d, "world");
        assert_eq!(v.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["Bob", "Alice", C]);
        let bob = &v[0];
        assert_eq!((bob.play_time_secs, bob.player_kills), (10_000, 2));
        assert_eq!(bob.last_seen, Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000)));
        assert!(bob.first_played.is_some());
        let alice = &v[1];
        assert_eq!(alice.uuid, A);
        assert_eq!(alice.play_time(), Duration::from_secs(3600));
        assert_eq!(
            (alice.deaths, alice.mob_kills, alice.jumps, alice.leave_game, alice.blocks_mined),
            (3, 7, 17, 4, 15)
        );
        assert!((alice.distance_km - 2.0).abs() < 1e-9);
        assert!(alice.last_seen.is_some()); // время изменения файла
        assert_eq!((v[2].play_time_secs, v[2].deaths), (100, 1));
        assert!((v[2].distance_km - 1.0).abs() < 1e-9);
        assert!(load(&d, "nope").is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn msk() -> FixedOffset {
        FixedOffset::east_opt(3 * 3600).unwrap()
    }

    fn at(d: u32, h: u32, m: u32) -> DateTime<FixedOffset> {
        msk().with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap()
    }

    #[test]
    fn sessions_join_leave_and_aggregates_across_midnight() {
        let mut log = SessionLog::new("unused.json");
        // Steve: 23:30 12-го → 00:45 13-го (через полночь)
        assert!(log.record_join("Steve", at(12, 23, 30)));
        assert!(!log.record_join("steve", at(12, 23, 40))); // дубль
        assert_eq!(log.online_since("STEVE"), Some(at(12, 23, 30).with_timezone(&Utc)));
        assert!(log.record_leave("Steve", at(13, 0, 45)));
        assert!(!log.record_leave("Steve", at(13, 0, 50)));
        assert_eq!(log.online_since("Steve"), None);
        // Alex: 00:00–01:00 13-го, Steve снова с 10:00 (ещё в игре)
        log.record_join("Alex", at(13, 0, 0));
        log.record_leave("Alex", at(13, 1, 0));
        log.record_join("Steve", at(13, 10, 0));
        let now = at(13, 10, 30);
        assert_eq!(log.playtime_today("Steve", now), Duration::from_secs((45 + 30) * 60));
        assert_eq!(log.playtime_days("Steve", now, 7), Duration::from_secs((75 + 30) * 60));
        assert_eq!(log.sessions("steve").len(), 2);
        assert_eq!(log.online(), vec!["Steve".to_string()]);
        let sum = log.summary(now);
        assert_eq!(sum[0].name, "Steve");
        assert_eq!(sum[0].sessions, 2);
        assert_eq!(sum[0].week, Duration::from_secs(105 * 60));
        assert!(sum[0].online_since.is_some());
        assert_eq!(sum[1].name, "Alex");
        assert_eq!(sum[1].today, Duration::from_secs(3600));
        let days = log.daily(now, 14);
        assert_eq!(days.len(), 14);
        let (d12, d13) = (&days[12], &days[13]);
        assert_eq!(d13.date, NaiveDate::from_ymd_opt(2026, 11, 13).unwrap());
        assert_eq!((d12.peak, d12.unique_players), (1, 1));
        assert_eq!(d12.play_time, Duration::from_secs(30 * 60));
        assert_eq!((d13.peak, d13.unique_players), (2, 2)); // Steve и Alex вместе в 00:00–00:45
        assert_eq!(d13.play_time, Duration::from_secs((45 + 60 + 30) * 60));
        assert_eq!(days[0].peak, 0);
        // В UTC те же сессии делятся по дням иначе: всё в 12-м числе (20:30–22:00 UTC) кроме Steve с 07:00.
        let utc_days = log.daily(now.with_timezone(&Utc), 2);
        assert_eq!(utc_days[0].play_time, Duration::from_secs((75 + 60) * 60));
        assert_eq!(utc_days[0].peak, 2);
        // Сервер упал — все сессии закрыты.
        log.close_all(at(13, 11, 0));
        assert!(log.online().is_empty());
        assert_eq!(log.playtime_today("Steve", at(13, 23, 0)), Duration::from_secs((45 + 60) * 60));
    }

    #[test]
    fn back_to_back_sessions_do_not_inflate_peak() {
        let mut log = SessionLog::new("unused.json");
        log.record_join("A", at(13, 10, 0));
        log.record_leave("A", at(13, 11, 0));
        log.record_join("B", at(13, 11, 0));
        log.record_leave("B", at(13, 12, 0));
        assert_eq!(log.daily(at(13, 13, 0), 1)[0].peak, 1);
    }

    #[test]
    fn persistence_bounding_and_recover() {
        let d = tmp("log");
        let path = d.join("sub/sessions.json");
        let mut log = SessionLog::load(&path);
        assert!(log.all_sessions().is_empty());
        // старая сессия (100 дней назад) и свежая
        log.record_join("Old", at(1, 10, 0) - ChronoDuration::days(100));
        log.record_leave("Old", at(1, 11, 0) - ChronoDuration::days(100));
        log.record_join("New", at(13, 10, 0));
        assert_eq!(log.sessions("Old").len(), 0); // выброшена при записи свежего входа
        log.heartbeat(at(13, 10, 20));
        log.save().unwrap();
        // служба упала с открытой сессией → при загрузке закрываем временем пульса
        let mut back = SessionLog::load(&path);
        assert_eq!(back.online(), vec!["New".to_string()]);
        back.recover();
        assert!(back.online().is_empty());
        assert_eq!(back.sessions("new")[0].duration(0), Duration::from_secs(20 * 60));
        // долгое «мигание» не раздувает файл бесконечно
        let mut big = SessionLog::new(d.join("big.json"));
        for i in 0..(MAX_SESSIONS as i64 + 10) {
            big.sessions.push(Session { name: "X".into(), start: 1_800_000_000 + i, end: Some(1_800_000_000 + i) });
        }
        big.prune(1_800_000_000);
        assert_eq!(big.all_sessions().len(), MAX_SESSIONS);
        assert_eq!(big.all_sessions()[0].start, 1_800_000_010);
        // повреждённый файл не мешает запуску
        std::fs::write(&path, "{oops").unwrap();
        assert!(SessionLog::load(&path).all_sessions().is_empty());
        assert!(path.with_extension("json.corrupt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
