//! Вкладка «Дополнения»: подвкладки «Плагины» и «Датапаки», навигация стрелками.

use super::sub_tabs::{Arrow, sub_arrow};
use super::*;

impl App {
    /// Сколько записей в выбранной подвкладке.
    pub(super) fn addons_len(&self) -> usize {
        match self.addons.sub {
            AddonsSub::Plugins => self.plugins.items.len(),
            AddonsSub::Datapacks => self.datapacks.items.len(),
        }
    }

    /// Стрелки на вкладке «Дополнения» (фокус на содержимом): те же уровни, что на «Доступе».
    /// `false` — клавиша не наша, её обработает общий код (←→ по вкладкам окна, ↑↓ по записям).
    pub(super) fn addons_arrow(&mut self, code: KeyCode) -> bool {
        if !self.sel().is_local() {
            return false;
        }
        let len = self.addons_len();
        let sel = match self.addons.sub {
            AddonsSub::Plugins => &mut self.plugins.sel,
            AddonsSub::Datapacks => &mut self.datapacks.sel,
        };
        match sub_arrow(&mut self.addons.level, sel, len, code) {
            Arrow::NotMine => false,
            Arrow::Handled => true,
            Arrow::Cycle(d) => {
                self.cycle_addons_sub(d);
                true
            }
        }
    }

    /// Листнуть подвкладку. Выбор в подвкладках не сбрасывается; пустой список не может быть на уровне записей.
    pub(super) fn cycle_addons_sub(&mut self, d: isize) {
        let subs = [AddonsSub::Plugins, AddonsSub::Datapacks];
        let i = subs.iter().position(|s| *s == self.addons.sub).unwrap_or(0) as isize;
        self.addons.sub = subs[(i + d).rem_euclid(subs.len() as isize) as usize];
        if self.addons.level == SubLevel::Entries && self.addons_len() == 0 {
            self.addons.level = SubLevel::Lists;
        }
    }
}
