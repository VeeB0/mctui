//! Подвкладка «Датапаки»: список датапаков текущего мира.

use super::*;

impl App {
    pub(super) fn refresh_datapacks(&mut self, force: bool) {
        if !self.has_server() || !self.sel().is_local() {
            self.datapacks.items.clear();
            self.datapacks.loaded_for = None;
            return;
        }
        let dir = datapacks::level_dir(self.sel().cfg.dir());
        let key = (self.sel().id, dir.clone());
        if !force && self.datapacks.loaded_for.as_ref() == Some(&key) {
            return;
        }
        if self.datapacks.loaded_for.as_ref() != Some(&key) {
            self.datapacks.updates.clear();
            self.datapacks.versions.clear();
            self.datapacks.checked = false;
        }
        self.datapacks.loaded_for = Some(key);
        self.datapacks.items = datapacks::scan(&dir);
        self.datapacks.sel = self.datapacks.sel.min(self.datapacks.items.len().saturating_sub(1));
        if self.addons.sub == AddonsSub::Datapacks
            && self.addons.level == SubLevel::Entries
            && self.datapacks.items.is_empty()
        {
            self.addons.level = SubLevel::Lists;
        }
    }
}
