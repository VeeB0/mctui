//! Список серверов: добавление (экран «Добавить сервер»), создание нового, удаление.

use super::*;

impl App {
    pub fn open_setup(&mut self) {
        let mut s = Setup::new(self.servers.is_empty());
        s.error = None;
        self.setup = Some(s);
        self.screen = Screen::Setup;
        self.check_setup_versions();
    }

    pub(super) fn ask_remove_server(&mut self) {
        if !self.has_server() {
            return;
        }
        let s = self.sel();
        if s.running() || s.backup_progress.is_some() {
            self.toast(
                tr!(
                    "сначала остановите сервер и дождитесь окончания бэкапа",
                    "stop the server and wait for the backup to finish first"
                ),
                ToastKind::Error,
            );
            return;
        }
        self.popup = Some(Popup::Confirm {
            title: tr!("Убрать из списка", "Remove from list").into(),
            text: tr!(
                format!("Убрать «{}» из pechka? Файлы сервера и бэкапы останутся на диске.", s.cfg.name),
                format!("Remove “{}” from pechka? Server files and backups stay on disk.", s.cfg.name)
            ),
            action: Action::RemoveServer(s.id),
        });
    }

    pub(super) fn close_setup(&mut self) {
        self.setup = None;
        self.screen = Screen::Main;
        self.set_focus(Focus::Sidebar);
    }

    /// Загрузить список версий ядра для формы «Создать новый», если ещё не загружен.
    pub(super) fn check_setup_versions(&mut self) {
        let Some(setup) = &mut self.setup else { return };
        let Some(core) = setup.wants_versions() else { return };
        setup.set_versions(core, Versions::Loading);
        self.spawn_task(
            move || crate::create::list_versions(core).map_err(|e| format!("{e:#}")),
            move |app, res| {
                if let Some(s) = &mut app.setup {
                    s.set_versions(
                        core,
                        match res {
                            Ok(v) => Versions::Ready(v),
                            Err(e) => Versions::Failed(e),
                        },
                    );
                }
            },
        );
    }

    pub(super) fn add_server(&mut self, cfg: ServerConfig) {
        self.select_new_after = Some(self.servers.iter().map(|s| s.id).max().unwrap_or(0));
        let name = cfg.name.clone();
        let local = cfg.kind == Kind::Local;
        self.send(Request::AddServer { cfg: Box::new(cfg) });
        self.close_setup();
        let hint = if local { tr!(" — Ctrl+S, чтобы запустить", " — Ctrl+S to start") } else { "" };
        self.toast(
            tr!(format!("Сервер «{name}» добавлен{hint}"), format!("Server “{name}” added{hint}")),
            ToastKind::Ok,
        );
    }

    pub(super) fn create_server(&mut self, req: crate::setup::CreateRequest) {
        if let Some(s) = &mut self.setup {
            s.creating = Some(tr!("подготовка…", "preparing…").into());
            s.error = None;
        }
        let tx = self.tx.clone();
        let core = req.opts.core;
        self.spawn_task(
            move || {
                let mut progress = |p: crate::create::Progress| {
                    let text = match p {
                        crate::create::Progress::Step(s) => s,
                        crate::create::Progress::Download { what, done, total } => match total {
                            Some(t) if t > 0 => {
                                format!("{what}: {} / {} ({}%)", human_bytes(done), human_bytes(t), done * 100 / t)
                            }
                            _ => format!("{what}: {}", human_bytes(done)),
                        },
                    };
                    let _ = tx.send(AppEvent::Ui(UiTask(Box::new(move |app: &mut App| {
                        if let Some(s) = &mut app.setup {
                            s.creating = Some(text);
                        }
                    }))));
                };
                crate::create::create(&req.opts, &mut progress).map(|c| (c, req.name)).map_err(|e| format!("{e:#}"))
            },
            move |app, res| match res {
                Ok((created, name)) => {
                    // Paper и Purpur хранят Незер и Энд отдельными папками.
                    let paths: Vec<String> = match core {
                        crate::create::Core::Paper | crate::create::Core::Purpur => {
                            vec!["world".into(), "world_nether".into(), "world_the_end".into()]
                        }
                        crate::create::Core::Velocity => vec!["velocity.toml".into(), "plugins".into()],
                        _ => vec!["world".into()],
                    };
                    let cfg = ServerConfig::new_local(&name, created.dir, created.command, paths);
                    app.add_server(cfg);
                }
                Err(e) => {
                    if let Some(s) = &mut app.setup {
                        s.creating = None;
                        s.error = Some(e);
                    }
                }
            },
        );
    }

    pub(super) fn setup_key(&mut self, k: KeyEvent) {
        let Some(setup) = &mut self.setup else { return };
        match setup.handle_key(k) {
            SetupOutcome::None => {}
            SetupOutcome::Cancel => {
                if self.servers.is_empty() {
                    self.should_quit = true;
                } else {
                    self.close_setup();
                }
            }
            SetupOutcome::Save(cfg) => {
                // Та же папка уже в списке?
                let dup = self
                    .servers
                    .iter()
                    .find(|s| s.cfg.kind == Kind::Local && cfg.kind == Kind::Local && s.cfg.dir == cfg.dir)
                    .map(|s| s.cfg.name.clone());
                match dup {
                    Some(n) => {
                        if let Some(s) = &mut self.setup {
                            s.error = Some(tr!(
                                format!("Эта папка уже добавлена как «{n}»"),
                                format!("This folder is already added as “{n}”")
                            ));
                        }
                    }
                    None => self.add_server(*cfg),
                }
            }
            SetupOutcome::Create(req) => self.create_server(*req),
        }
        self.check_setup_versions();
    }
}
