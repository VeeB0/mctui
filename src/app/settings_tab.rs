//! Вкладка «Настройки»: параметры pechka для выбранного сервера (автозапуск, авто-рестарт,
//! уведомления, бэкапы, расписание, язык).

use super::*;

impl App {
    /// Строки настроек pechka над server.properties.
    pub fn opts(&self) -> Vec<Opt> {
        if self.has_server() && self.sel().is_local() {
            vec![
                Opt::Autostart,
                Opt::AutoRestart,
                Opt::Notify,
                Opt::BackupEvery,
                Opt::BackupKeep,
                Opt::BackupMode,
                Opt::BackupCopyTo,
                Opt::RestartTimes,
                Opt::Language,
            ]
        } else {
            vec![Opt::Language]
        }
    }

    pub fn opt_value(&self, o: Opt) -> String {
        let c = &self.sel().cfg;
        let yes_no = |b: bool| if b { tr!("да", "yes") } else { tr!("нет", "no") }.to_string();
        match o {
            Opt::Autostart => yes_no(c.autostart),
            Opt::AutoRestart => yes_no(c.auto_restart),
            Opt::Notify => yes_no(c.notify),
            Opt::BackupEvery => c.backup_every_hours.to_string(),
            Opt::BackupKeep => c.backup_keep.to_string(),
            Opt::BackupMode => match c.backup_mode {
                crate::config::BackupMode::Full => tr!("полный архив .tar.gz", "full .tar.gz archive"),
                crate::config::BackupMode::Incremental => {
                    tr!("инкрементальный (только изменения)", "incremental (changes only)")
                }
            }
            .into(),
            Opt::BackupCopyTo => {
                if c.backup_copy_to.is_empty() {
                    tr!("никуда", "nowhere").into()
                } else {
                    c.backup_copy_to.join("; ")
                }
            }
            Opt::RestartTimes => {
                if c.restart_times.is_empty() {
                    tr!("выкл", "off").into()
                } else {
                    c.restart_times.join(", ")
                }
            }
            Opt::Language => match self.language.as_deref() {
                Some("ru") => "Русский".into(),
                Some("en") => "English".into(),
                _ => tr!("как в системе", "system").into(),
            },
        }
    }

    pub(super) fn update_cfg(&mut self, f: impl FnOnce(&mut ServerConfig)) {
        let s = self.sel_mut();
        let mut cfg = s.view.cfg.clone();
        f(&mut cfg);
        s.view.cfg = cfg.clone(); // сразу видно в окне, служба пришлёт подтверждение
        let id = s.id;
        self.send(Request::UpdateServer { id, cfg: Box::new(cfg) });
    }

    pub(super) fn toggle_opt(&mut self, o: Opt) {
        match o {
            Opt::Autostart => self.update_cfg(|c| c.autostart = !c.autostart),
            Opt::AutoRestart => self.update_cfg(|c| c.auto_restart = !c.auto_restart),
            Opt::Notify => self.update_cfg(|c| c.notify = !c.notify),
            Opt::BackupMode => self.update_cfg(|c| {
                use crate::config::BackupMode;
                c.backup_mode =
                    if c.backup_mode == BackupMode::Full { BackupMode::Incremental } else { BackupMode::Full }
            }),
            Opt::Language => {
                let next = match self.language.as_deref() {
                    None => Some("ru"),
                    Some("ru") => Some("en"),
                    _ => None,
                };
                self.language = next.map(str::to_string);
                crate::i18n::init(next);
                self.send(Request::SetLanguage { language: self.language.clone() });
                if self.screen == Screen::Logs {
                    self.build_log_sources();
                }
            }
            _ => self.edit_opt(o),
        }
    }

    pub(super) fn edit_opt(&mut self, o: Opt) {
        let c = &self.sel().cfg;
        let v = match o {
            Opt::BackupEvery => c.backup_every_hours.to_string(),
            Opt::BackupKeep => c.backup_keep.to_string(),
            Opt::RestartTimes => c.restart_times.join(", "),
            Opt::BackupCopyTo => c.backup_copy_to.join("; "),
            _ => return self.toggle_opt(o),
        };
        self.popup =
            Some(Popup::Edit { title: o.label().into(), input: TextInput::with(&v), target: EditTarget::Opt(o) });
    }

    pub(super) fn apply_opt(&mut self, o: Opt, v: &str) {
        let bad = |app: &mut App| app.toast(tr!("неверное значение", "invalid value"), ToastKind::Error);
        match o {
            Opt::BackupEvery => match v.trim().parse::<u32>() {
                Ok(n) if n <= 24 * 30 => self.update_cfg(|c| c.backup_every_hours = n),
                _ => bad(self),
            },
            Opt::BackupKeep => match v.trim().parse::<usize>() {
                Ok(n) if n >= 1 => self.update_cfg(|c| c.backup_keep = n),
                _ => bad(self),
            },
            Opt::BackupCopyTo => {
                let targets: Vec<String> =
                    v.split(';').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect();
                self.update_cfg(|c| c.backup_copy_to = targets);
            }
            Opt::RestartTimes => {
                let parts: Vec<String> =
                    v.split([',', ' ', ';']).map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect();
                if parts.iter().all(|p| crate::config::parse_hm(p).is_some()) {
                    let norm: Vec<String> = parts
                        .iter()
                        .filter_map(|p| crate::config::parse_hm(p))
                        .map(|(h, m)| format!("{h:02}:{m:02}"))
                        .collect();
                    self.update_cfg(|c| c.restart_times = norm);
                } else {
                    self.toast(
                        tr!(
                            "время — ЧЧ:ММ через запятую, например 04:00, 16:00",
                            "times are HH:MM, comma-separated, e.g. 04:00, 16:00"
                        ),
                        ToastKind::Error,
                    );
                }
            }
            _ => {}
        }
    }
}
