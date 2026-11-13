//! Предупреждения о нагрузке: низкий TPS, мало памяти у компьютера, OutOfMemoryError, мало места на диске.
//!
//! Служба проверяет условия каждую секунду; предупреждение «включается», когда условие держится
//! [`HOLD`] подряд, и снимается, когда оно так же долго не выполняется. О включении — строка в лог,
//! всплывающее сообщение в окне и (если `notify = true`) уведомление рабочего стола.

use crate::tr;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Сколько условие должно держаться, чтобы предупреждение включилось / снялось.
pub const HOLD: Duration = Duration::from_secs(60);
/// Не чаще одного уведомления рабочего стола одного вида за это время.
pub const RENOTIFY: Duration = Duration::from_secs(30 * 60);

/// TPS ниже — предупреждение; снимается, когда TPS снова не ниже `TPS_CLEAR`.
pub const TPS_LOW: f64 = 15.0;
pub const TPS_CLEAR: f64 = 18.0;
/// Свободной памяти компьютера меньше этой доли (или меньше `MEM_LOW_MB`) — предупреждение.
pub const MEM_LOW_FRACTION: f64 = 0.05;
pub const MEM_LOW_MB: u64 = 512;
/// Свободного места на диске сервера меньше — предупреждение.
pub const DISK_LOW_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// После строки OutOfMemoryError предупреждение держится столько (если строка не повторяется).
pub const OOM_STICKY: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    LowTps,
    LowMemory,
    OutOfMemory,
    LowDisk,
}

impl Kind {
    pub fn text(self) -> String {
        match self {
            Kind::LowTps => tr!(
                format!("низкий TPS (< {TPS_LOW:.0}) — сервер не успевает"),
                format!("low TPS (< {TPS_LOW:.0}) — the server is lagging")
            ),
            Kind::LowMemory => tr!("компьютеру не хватает памяти", "the computer is running out of memory").to_string(),
            Kind::OutOfMemory => tr!(
                "Java: OutOfMemoryError — увеличьте -Xmx в скрипте запуска",
                "Java: OutOfMemoryError — raise -Xmx in the start script"
            )
            .to_string(),
            Kind::LowDisk => tr!(
                format!("на диске сервера меньше {} ГиБ", DISK_LOW_BYTES >> 30),
                format!("less than {} GiB free on the server disk", DISK_LOW_BYTES >> 30)
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Raised(Kind),
    Cleared(Kind),
}

#[derive(Debug, Default)]
struct Entry {
    active: bool,
    /// С какого момента наблюдается состояние, противоположное `active`.
    flip_since: Option<Instant>,
    last_notified: Option<Instant>,
}

/// Состояние предупреждений одного сервера.
#[derive(Debug, Default)]
pub struct Tracker {
    entries: BTreeMap<Kind, Entry>,
    oom_at: Option<Instant>,
}

impl Tracker {
    /// Учитывает наблюдение `bad` (условие выполняется). Возвращает включение / снятие.
    pub fn observe(&mut self, kind: Kind, bad: bool, now: Instant) -> Option<Change> {
        let e = self.entries.entry(kind).or_default();
        if bad == e.active {
            e.flip_since = None;
            return None;
        }
        let since = *e.flip_since.get_or_insert(now);
        if now.duration_since(since) < HOLD {
            return None;
        }
        e.active = bad;
        e.flip_since = None;
        Some(if bad { Change::Raised(kind) } else { Change::Cleared(kind) })
    }

    /// Включает предупреждение сразу (без ожидания `HOLD`).
    pub fn raise_now(&mut self, kind: Kind) -> Option<Change> {
        let e = self.entries.entry(kind).or_default();
        e.flip_since = None;
        if e.active {
            return None;
        }
        e.active = true;
        Some(Change::Raised(kind))
    }

    pub fn is_active(&self, kind: Kind) -> bool {
        self.entries.get(&kind).is_some_and(|e| e.active)
    }

    /// Активные предупреждения (для окна).
    pub fn active(&self) -> Vec<Kind> {
        self.entries.iter().filter(|(_, e)| e.active).map(|(k, _)| *k).collect()
    }

    /// Пора ли слать уведомление рабочего стола о `kind` (и отметить, что отправлено).
    pub fn should_notify(&mut self, kind: Kind, now: Instant) -> bool {
        let e = self.entries.entry(kind).or_default();
        if e.last_notified.is_some_and(|t| now.duration_since(t) < RENOTIFY) {
            return false;
        }
        e.last_notified = Some(now);
        true
    }

    /// Строка лога с OutOfMemoryError.
    pub fn note_oom(&mut self, now: Instant) -> Option<Change> {
        self.oom_at = Some(now);
        self.raise_now(Kind::OutOfMemory)
    }

    /// Снимает OOM, если строка давно не повторялась.
    pub fn expire_oom(&mut self, now: Instant) -> Option<Change> {
        let stale = self.oom_at.is_none_or(|t| now.duration_since(t) >= OOM_STICKY);
        if stale && self.is_active(Kind::OutOfMemory) {
            self.entries.entry(Kind::OutOfMemory).or_default().active = false;
            return Some(Change::Cleared(Kind::OutOfMemory));
        }
        None
    }

    /// Сервер остановлен — все предупреждения снимаются без уведомлений.
    pub fn reset(&mut self) {
        for e in self.entries.values_mut() {
            e.active = false;
            e.flip_since = None;
        }
        self.oom_at = None;
    }
}

/// Условие «мало TPS» с гистерезисом: включается ниже `TPS_LOW`, держится, пока ниже `TPS_CLEAR`.
pub fn tps_bad(tps_1m: f64, active: bool) -> bool {
    if active { tps_1m < TPS_CLEAR } else { tps_1m < TPS_LOW }
}

pub fn memory_bad(available: u64, total: u64) -> bool {
    total > 0 && (available < MEM_LOW_MB * 1024 * 1024 || (available as f64) < total as f64 * MEM_LOW_FRACTION)
}

pub fn is_oom_line(line: &str) -> bool {
    line.contains("java.lang.OutOfMemoryError")
}

/// Уведомление рабочего стола (macOS — Центр уведомлений, Linux — notify-send). Ошибки молча игнорируются.
pub fn desktop(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!("display notification \"{}\" with title \"{}\"", esc(body), esc(title));
        spawn_quiet(std::process::Command::new("osascript").args(["-e", &script]));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        spawn_quiet(std::process::Command::new("notify-send").args(["--app-name=pechka", title, body]));
    }
    #[cfg(windows)]
    {
        let _ = (title, body);
    }
}

#[cfg(unix)]
fn spawn_quiet(cmd: &mut std::process::Command) {
    use std::process::Stdio;
    if let Ok(mut child) = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        // Не оставляем зомби: дожидаемся в отдельном потоке.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raises_after_hold_and_clears_after_hold() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert_eq!(t.observe(Kind::LowTps, true, t0), None);
        assert_eq!(t.observe(Kind::LowTps, true, t0 + Duration::from_secs(30)), None);
        // кратковременное улучшение сбрасывает отсчёт
        assert_eq!(t.observe(Kind::LowTps, false, t0 + Duration::from_secs(31)), None);
        assert_eq!(t.observe(Kind::LowTps, true, t0 + Duration::from_secs(32)), None);
        assert_eq!(t.observe(Kind::LowTps, true, t0 + Duration::from_secs(80)), None);
        assert_eq!(t.observe(Kind::LowTps, true, t0 + Duration::from_secs(92)), Some(Change::Raised(Kind::LowTps)));
        assert!(t.is_active(Kind::LowTps));
        assert_eq!(t.active(), vec![Kind::LowTps]);
        assert_eq!(t.observe(Kind::LowTps, false, t0 + Duration::from_secs(100)), None);
        assert_eq!(t.observe(Kind::LowTps, false, t0 + Duration::from_secs(160)), Some(Change::Cleared(Kind::LowTps)));
        assert!(t.active().is_empty());
    }

    #[test]
    fn tps_hysteresis() {
        assert!(tps_bad(14.0, false));
        assert!(!tps_bad(16.0, false));
        assert!(tps_bad(16.0, true));
        assert!(!tps_bad(19.5, true));
    }

    #[test]
    fn memory_threshold() {
        let gib = 1024 * 1024 * 1024;
        assert!(memory_bad(300 * 1024 * 1024, 16 * gib));
        assert!(memory_bad(gib / 2, 16 * gib)); // < 5 %
        assert!(!memory_bad(2 * gib, 16 * gib));
        assert!(!memory_bad(0, 0));
    }

    #[test]
    fn notify_cooldown() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert!(t.should_notify(Kind::LowDisk, t0));
        assert!(!t.should_notify(Kind::LowDisk, t0 + Duration::from_secs(60)));
        assert!(t.should_notify(Kind::LowDisk, t0 + RENOTIFY));
    }

    #[test]
    fn oom_is_immediate_and_expires() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert!(is_oom_line("Exception in thread \"main\" java.lang.OutOfMemoryError: Java heap space"));
        assert_eq!(t.note_oom(t0), Some(Change::Raised(Kind::OutOfMemory)));
        assert_eq!(t.note_oom(t0 + Duration::from_secs(5)), None);
        assert_eq!(t.expire_oom(t0 + Duration::from_secs(60)), None);
        assert_eq!(t.expire_oom(t0 + Duration::from_secs(5) + OOM_STICKY), Some(Change::Cleared(Kind::OutOfMemory)));
        t.note_oom(t0);
        t.reset();
        assert!(t.active().is_empty());
    }
}
