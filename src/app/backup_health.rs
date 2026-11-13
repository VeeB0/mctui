//! Свежесть бэкапов для вкладки «Статус»: «бэкапов нет», «последний — 9 дн назад».
//!
//! Время самого свежего бэкапа окно считает само по папке `backup_dir` (в фоне): служба знает только
//! бэкапы, сделанные при ней, а старые лежат на диске с прошлых запусков.

use super::*;

/// Без авто-бэкапа бэкап старше этого срока считается устаревшим.
pub const STALE_WITHOUT_AUTO: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupHealth {
    /// Ещё не посчитано (или сервер не локальный).
    Unknown,
    /// Ни одного бэкапа. `auto` — включён ли авто-бэкап.
    Missing { auto: bool },
    /// Последний бэкап `age` назад.
    Fresh { age: Duration },
    /// Авто-бэкап выключен, а последний бэкап старше [`STALE_WITHOUT_AUTO`].
    Stale { age: Duration },
}

/// Время самого свежего из списка (`backup::list` отдаёт новые первыми).
pub fn newest_of(list: &[BackupInfo]) -> Option<SystemTime> {
    list.iter().filter_map(|b| b.modified).max().or_else(|| (!list.is_empty()).then(SystemTime::now))
}

impl ServerUi {
    /// Свежесть бэкапов устарела: посчитать заново, ответ уже идущего подсчёта не принимать.
    pub(super) fn forget_backup_age(&mut self) {
        self.newest_backup = None;
        self.backup_scan = false;
        self.backup_gen = self.backup_gen.wrapping_add(1);
    }

    pub fn backup_health(&self, now: SystemTime) -> BackupHealth {
        if !self.is_local() {
            return BackupHealth::Unknown;
        }
        let auto = self.cfg.backup_every_hours > 0;
        match self.newest_backup {
            None => BackupHealth::Unknown,
            Some(None) => BackupHealth::Missing { auto },
            Some(Some(t)) => {
                let age = now.duration_since(t).unwrap_or_default();
                if !auto && age > STALE_WITHOUT_AUTO {
                    BackupHealth::Stale { age }
                } else {
                    BackupHealth::Fresh { age }
                }
            }
        }
    }
}

/// Грубый возраст: «только что», «12 мин назад», «5 ч назад», «9 дн назад».
pub fn ago(d: Duration) -> String {
    let m = d.as_secs() / 60;
    let h = m / 60;
    if m < 1 {
        tr!("только что", "just now").into()
    } else if h < 1 {
        tr!(format!("{m} мин назад"), format!("{m} min ago"))
    } else if h < 48 {
        tr!(format!("{h} ч назад"), format!("{h} h ago"))
    } else {
        tr!(format!("{} дн назад", h / 24), format!("{} days ago", h / 24))
    }
}

impl App {
    /// Посчитать в фоне свежесть бэкапов у локальных серверов, где она ещё неизвестна.
    pub(super) fn scan_backup_ages(&mut self) {
        for s in &mut self.servers {
            if !s.is_local() || s.newest_backup.is_some() || s.backup_scan {
                continue;
            }
            s.backup_scan = true;
            let (id, generation) = (s.id, s.backup_gen);
            let (dir, prefix) = (s.cfg.backup_dir(), backup::file_prefix(&s.cfg.name));
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let newest = newest_of(&backup::list(&dir, &prefix));
                let apply = move |app: &mut App| {
                    if let Some(s) = app.servers.iter_mut().find(|s| s.id == id && s.backup_gen == generation) {
                        s.backup_scan = false;
                        s.newest_backup = Some(newest);
                    }
                };
                let _ = tx.send(AppEvent::Ui(UiTask(Box::new(apply))));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};

    fn server(every: u32, newest: Option<Option<SystemTime>>) -> ServerUi {
        let mut cfg = ServerConfig::new_local("s", PathBuf::from("/nonexistent/s"), vec![], vec![]);
        cfg.backup_every_hours = every;
        let mut s = ServerUi::new(crate::server::ServerState::new(cfg, 0).view());
        s.newest_backup = newest;
        s
    }

    #[test]
    fn health() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * 24 * 3600);
        let days = |d: u64| Some(Some(now - Duration::from_secs(d * 24 * 3600)));
        assert_eq!(server(0, None).backup_health(now), BackupHealth::Unknown);
        assert_eq!(server(0, Some(None)).backup_health(now), BackupHealth::Missing { auto: false });
        assert_eq!(server(6, Some(None)).backup_health(now), BackupHealth::Missing { auto: true });
        let week = Duration::from_secs(7 * 24 * 3600);
        assert_eq!(server(0, days(3)).backup_health(now), BackupHealth::Fresh { age: week * 3 / 7 });
        assert_eq!(server(0, days(9)).backup_health(now), BackupHealth::Stale { age: week * 9 / 7 });
        // С авто-бэкапом старый бэкап — не тревога: служба не бэкапит, пока никто не играл.
        assert_eq!(server(6, days(9)).backup_health(now), BackupHealth::Fresh { age: week * 9 / 7 });
        // Часы ушли назад — возраст ноль, а не паника.
        let future = Some(Some(now + Duration::from_secs(60)));
        assert_eq!(server(0, future).backup_health(now), BackupHealth::Fresh { age: Duration::ZERO });
        let rcon = ServerUi::new(crate::server::ServerState::new(ServerConfig::new_rcon("r", "h", 1, "p"), 1).view());
        assert_eq!(rcon.backup_health(now), BackupHealth::Unknown);
    }

    #[test]
    fn newest() {
        let t = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        let b = |m: Option<SystemTime>| BackupInfo {
            path: PathBuf::from("x"),
            kind: backup::BackupKind::Full,
            size: 0,
            total_size: 0,
            files: None,
            modified: m,
        };
        assert_eq!(newest_of(&[]), None);
        assert_eq!(newest_of(&[b(Some(t(5))), b(Some(t(9))), b(None)]), Some(t(9)));
        assert!(newest_of(&[b(None)]).is_some(), "бэкап без времени — всё равно бэкап");
    }

    #[test]
    fn ago_text() {
        let m = |x: u64| Duration::from_secs(x * 60);
        with_lang(Lang::Ru, || {
            assert_eq!(ago(Duration::from_secs(20)), "только что");
            assert_eq!(ago(m(12)), "12 мин назад");
            assert_eq!(ago(m(5 * 60 + 3)), "5 ч назад");
            assert_eq!(ago(m(9 * 24 * 60)), "9 дн назад");
        });
        with_lang(Lang::En, || assert_eq!(ago(m(47 * 60)), "47 h ago"));
    }
}
