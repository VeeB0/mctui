//! Вкладка «Мир»: измерения и их размер, сброс Незера/Энда, смена мира (`level-name`),
//! предварительная генерация чанков через Chunky.

use super::*;
use crate::world::{self, Dimension, PregenCenter, WorldInfo};

#[derive(Default)]
pub struct WorldView {
    pub info: Option<WorldInfo>,
    pub loading: bool,
    pub sel: usize,
    pub chunky: Option<world::ChunkyInstall>,
    loaded_for: Option<usize>,
    loaded_at: Option<Instant>,
}

/// Строка списка вкладки: измерение или другой мир.
pub enum WorldRow<'a> {
    Dim(&'a world::DimensionInfo),
    Other(&'a str),
}

impl WorldView {
    pub fn rows(&self) -> Vec<WorldRow<'_>> {
        let Some(info) = &self.info else { return vec![] };
        info.dimensions
            .iter()
            .map(WorldRow::Dim)
            .chain(info.other_worlds.iter().map(|w| WorldRow::Other(w.as_str())))
            .collect()
    }
}

impl App {
    pub(super) fn refresh_world(&mut self, force: bool) {
        if !self.has_server() || !self.sel().is_local() {
            self.world = WorldView::default();
            return;
        }
        let id = self.sel().id;
        let fresh = self.world.loaded_at.is_some_and(|t| t.elapsed() < Duration::from_secs(30));
        if !force && self.world.loaded_for == Some(id) && (fresh || self.world.loading) {
            return;
        }
        if self.world.loaded_for != Some(id) {
            self.world = WorldView::default();
        }
        self.world.loading = true;
        self.world.loaded_for = Some(id);
        let dir = self.sel().cfg.dir().to_path_buf();
        let level = self.sel().prop("level-name").map(str::to_string);
        self.spawn_task(
            move || {
                let level = level.unwrap_or_else(|| world::current_level_name(&dir));
                (world::scan(&dir, &level), world::chunky_installed(&dir))
            },
            move |app, (info, chunky)| {
                if app.world.loaded_for != Some(id) {
                    return;
                }
                app.world.loading = false;
                app.world.loaded_at = Some(Instant::now());
                let n = info.dimensions.len() + info.other_worlds.len();
                app.world.sel = app.world.sel.min(n.saturating_sub(1));
                app.world.info = Some(info);
                app.world.chunky = chunky;
            },
        );
    }

    pub(super) fn world_char(&mut self, c: char) {
        match c {
            'u' => self.refresh_world(true),
            'R' => self.ask_reset_dimension(),
            'p' => self.start_pregen(),
            'P' => self.pregen_command(world::PREGEN_PAUSE),
            'C' => self.pregen_command(world::PREGEN_CONTINUE),
            'x' => {
                for c in world::PREGEN_CANCEL {
                    self.pregen_command(c);
                }
            }
            _ => {}
        }
    }

    pub(super) fn world_enter(&mut self) {
        let other = match self.world.rows().get(self.world.sel) {
            Some(WorldRow::Other(w)) => w.to_string(),
            Some(WorldRow::Dim(_)) => return self.start_pregen(),
            None => return,
        };
        self.popup = Some(Popup::Confirm {
            title: tr!("Сменить мир", "Switch world").into(),
            text: tr!(
                format!("Сделать «{other}» основным миром (level-name)? Действует после перезапуска сервера."),
                format!("Make “{other}” the main world (level-name)? Takes effect after a server restart.")
            ),
            action: Action::SetLevel(other),
        });
    }

    fn selected_dim(&self) -> Option<Dimension> {
        match self.world.rows().get(self.world.sel) {
            Some(WorldRow::Dim(d)) => Some(d.dim.clone()),
            _ => None,
        }
    }

    fn ask_reset_dimension(&mut self) {
        let Some(dim) = self.selected_dim() else { return };
        if !dim.resettable() {
            return self.toast(
                tr!("Сбросить можно только Незер или Энд", "Only the Nether or the End can be reset"),
                ToastKind::Info,
            );
        }
        if self.sel().running() {
            return self
                .toast(tr!("Сначала остановите сервер (Ctrl+X)", "Stop the server first (Ctrl+X)"), ToastKind::Info);
        }
        let t = dim.title();
        self.popup = Some(Popup::Confirm {
            title: tr!(format!("Сброс: {t}"), format!("Reset: {t}")),
            text: tr!(
                format!(
                    "Сбросить {t}? При следующем запуске он сгенерируется заново. Старая версия не удаляется, а переносится в {}/.",
                    world::RESET_STASH_DIR
                ),
                format!(
                    "Reset {t}? It is generated anew on the next start. The old copy is not deleted but moved to {}/.",
                    world::RESET_STASH_DIR
                )
            ),
            action: Action::ResetDimension(dim),
        });
    }

    pub(super) fn reset_dimension(&mut self, dim: Dimension) {
        let dir = self.sel().cfg.dir().to_path_buf();
        let level = self.world.info.as_ref().map(|i| i.level_name.clone()).unwrap_or_else(|| "world".into());
        self.spawn_task(
            move || world::reset_dimension(&dir, &level, dim),
            |app, res| {
                match res {
                    Ok(stash) => app.toast(
                        tr!(
                            format!("сброшено, старое — в {}", stash.display()),
                            format!("reset; the old copy is in {}", stash.display())
                        ),
                        ToastKind::Ok,
                    ),
                    Err(e) => app.toast(format!("{e:#}"), ToastKind::Error),
                }
                app.refresh_world(true);
            },
        );
    }

    pub(super) fn set_level(&mut self, name: String) {
        self.set_property("level-name", &name);
        self.refresh_world(true);
    }

    fn pregen_command(&mut self, cmd: &str) {
        if self.world.chunky.is_none() || !self.sel().running() {
            return;
        }
        let id = self.sel().id;
        self.send(Request::Command { id, cmd: cmd.into(), user: false });
    }

    fn start_pregen(&mut self) {
        if self.world.chunky.is_none() {
            return self.offer_chunky();
        }
        if !self.sel().running() {
            return self.toast(
                tr!(
                    "Генерация идёт на работающем сервере — запустите его (Ctrl+S)",
                    "Pregeneration runs on a live server — start it (Ctrl+S)"
                ),
                ToastKind::Info,
            );
        }
        let Some(dim) = self.selected_dim() else { return };
        self.popup = Some(Popup::Edit {
            title: tr!(
                format!("Сгенерировать {}: радиус в блоках от 0,0", dim.title()),
                format!("Pregenerate {}: radius in blocks around 0,0", dim.title())
            ),
            input: TextInput::with("2000"),
            target: EditTarget::PregenRadius(dim),
        });
    }

    pub(super) fn run_pregen(&mut self, dim: Dimension, radius: &str) {
        let Ok(r) = radius.trim().parse::<u32>() else {
            return self.toast(
                tr!("радиус — число блоков, например 2000", "the radius is a number of blocks, e.g. 2000"),
                ToastKind::Error,
            );
        };
        if !(16..=100_000).contains(&r) {
            return self.toast(tr!("радиус: от 16 до 100000", "radius: 16 to 100000"), ToastKind::Error);
        }
        let Some(ch) = &self.world.chunky else { return };
        let level = self.world.info.as_ref().map(|i| i.level_name.clone()).unwrap_or_else(|| "world".into());
        let w = world::chunky_world(ch.kind, &level, &dim);
        let id = self.sel().id;
        for cmd in world::pregen_commands(&w, PregenCenter::Origin, r) {
            self.send(Request::Command { id, cmd, user: false });
        }
        let n = world::pregen_chunk_count(r);
        self.toast(
            tr!(
                format!("генерация запущена: ~{n} чанков (ход — на вкладке «Мир»)"),
                format!("pregeneration started: ~{n} chunks (progress on the World tab)")
            ),
            ToastKind::Ok,
        );
    }

    /// Chunky не установлен — предложить поставить с Modrinth.
    fn offer_chunky(&mut self) {
        let s = self.sel();
        let Some(target) =
            crate::plugin_search::Target::detect(s.cfg.dir(), &s.cfg.command.join(" "), s.version.as_deref())
        else {
            return self.toast(
                tr!(
                    "Для генерации нужен Chunky (плагин или мод) — поставьте его на вкладке «Плагины»",
                    "Pregeneration needs Chunky (plugin or mod) — install it on the Plugins tab"
                ),
                ToastKind::Info,
            );
        };
        let dir = s.cfg.dir().to_path_buf();
        self.toast(tr!("ищу Chunky…", "looking up Chunky…"), ToastKind::Info);
        self.spawn_task(
            move || crate::plugin_search::plan_by_slug("chunky", &target, &dir),
            |app, res| match res {
                Ok(Some(plan)) => app.confirm_install(plan),
                Ok(None) => app.toast(
                    tr!("Chunky для этой версии не найден", "No Chunky build for this version"),
                    ToastKind::Error,
                ),
                Err(e) => app.toast(format!("{e:#}"), ToastKind::Error),
            },
        );
    }

    /// Последний ход генерации из живого лога сервера.
    pub fn pregen_progress(&self) -> Option<world::PregenProgress> {
        let s = self.sel();
        let n = s.log.len();
        s.log.iter().skip(n.saturating_sub(300)).filter_map(|l| world::parse_pregen_progress(&l.text)).last()
    }
}
