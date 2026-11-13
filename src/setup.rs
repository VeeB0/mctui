//! Экран добавления сервера: при первом запуске и по клавише `a`.
//! Три варианта: существующая папка, новый сервер с нуля, удалённый по RCON.

use crate::config::{Detection, ServerConfig, detect_local, normalize_path_input};
use crate::create::{Core, CreateOptions};
use crate::input::TextInput;
use crate::tr;
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupKind {
    Local,
    New,
    Rcon,
}

impl SetupKind {
    pub const ALL: [SetupKind; 3] = [SetupKind::Local, SetupKind::New, SetupKind::Rcon];

    pub fn label(self) -> &'static str {
        match self {
            SetupKind::Local => tr!("Есть папка", "Existing folder"),
            SetupKind::New => tr!("Создать новый", "Create new"),
            SetupKind::Rcon => tr!("Удалённый (RCON)", "Remote (RCON)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Kind,
    Path,
    Name,
    Host,
    Port,
    Password,
    Core,
    Version,
    Memory,
    GamePort,
    Eula,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Kind => tr!("Сервер", "Server"),
            Field::Path => tr!("Папка", "Folder"),
            Field::Name => tr!("Название", "Name"),
            Field::Host => tr!("Адрес", "Address"),
            Field::Port => tr!("Порт RCON", "RCON port"),
            Field::Password => tr!("Пароль RCON", "RCON password"),
            Field::Core => tr!("Ядро", "Core"),
            Field::Version => tr!("Версия", "Version"),
            Field::Memory => tr!("Память", "Memory"),
            Field::GamePort => tr!("Порт", "Port"),
            Field::Eula => "EULA",
        }
    }

    /// Поле-переключатель (←→), а не ввод текста.
    pub fn is_choice(self) -> bool {
        matches!(self, Field::Kind | Field::Core | Field::Version | Field::Eula)
    }
}

/// Создание нового сервера: что передать в [`crate::create::create`] и как назвать.
pub struct CreateRequest {
    pub opts: CreateOptions,
    pub name: String,
}

pub enum Outcome {
    None,
    Cancel,
    Save(Box<ServerConfig>),
    Create(Box<CreateRequest>),
}

/// Список версий ядра: загружается в фоне.
pub enum Versions {
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

pub struct Setup {
    pub first_run: bool,
    pub kind: SetupKind,
    pub field: usize,
    pub path: TextInput,
    pub name: TextInput,
    pub host: TextInput,
    pub port: TextInput,
    pub password: TextInput,
    pub memory: TextInput,
    pub game_port: TextInput,
    pub core: Core,
    pub versions: HashMap<Core, Versions>,
    pub version_sel: usize,
    pub eula: bool,
    name_edited: bool,
    new_path_edited: bool,
    pub detection: Detection,
    pub error: Option<String>,
    /// Идёт создание сервера: текущий шаг.
    pub creating: Option<String>,
}

const LOCAL_FIELDS: [Field; 3] = [Field::Kind, Field::Path, Field::Name];
const NEW_FIELDS: [Field; 8] =
    [Field::Kind, Field::Core, Field::Version, Field::Path, Field::Name, Field::Memory, Field::GamePort, Field::Eula];
/// Прокси Velocity: без EULA (это не сервер Minecraft).
const PROXY_FIELDS: [Field; 7] =
    [Field::Kind, Field::Core, Field::Version, Field::Path, Field::Name, Field::Memory, Field::GamePort];
const RCON_FIELDS: [Field; 5] = [Field::Kind, Field::Name, Field::Host, Field::Port, Field::Password];

fn default_memory_mb() -> u32 {
    let total = sysinfo::System::new()
        .ok()
        .map(|mut s| {
            s.refresh_memory();
            s.total_memory() / (1024 * 1024)
        })
        .unwrap_or(8192);
    if total >= 12_000 {
        4096
    } else if total >= 6_000 {
        2048
    } else {
        1024
    }
}

/// «4G», «4096», «2.5 ГБ» → мегабайты.
pub fn parse_memory(s: &str) -> Option<u32> {
    let t = s.trim().to_lowercase().replace(',', ".");
    let num: String = t.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let unit = t[num.len()..].trim();
    let v: f64 = num.parse().ok()?;
    let mb = match unit.chars().next() {
        Some('g' | 'г') => v * 1024.0,
        Some('m' | 'м') | None => {
            if v < 64.0 {
                v * 1024.0 // «4» — скорее гигабайты
            } else {
                v
            }
        }
        _ => return None,
    };
    (mb >= 1.0).then_some(mb.round() as u32)
}

impl Setup {
    pub fn new(first_run: bool) -> Self {
        let mut s = Self {
            first_run,
            kind: SetupKind::Local,
            field: 1,
            path: TextInput::default(),
            name: TextInput::default(),
            host: TextInput::default(),
            port: TextInput::with("25575"),
            password: TextInput::default(),
            memory: TextInput::with(&format!("{}M", default_memory_mb())),
            game_port: TextInput::with("25565"),
            core: Core::Paper,
            versions: HashMap::new(),
            version_sel: 0,
            eula: false,
            name_edited: false,
            new_path_edited: false,
            detection: Detection::default(),
            error: None,
            creating: None,
        };
        // Запустили из папки сервера — сразу подставим её.
        if let Ok(cwd) = std::env::current_dir() {
            let looks_like_server =
                ["server.properties", "eula.txt", "start.sh", "start.bat"].iter().any(|f| cwd.join(f).is_file());
            if looks_like_server {
                s.path.set(&cwd.display().to_string());
            }
        }
        s.redetect();
        s
    }

    pub fn fields(&self) -> &'static [Field] {
        match self.kind {
            SetupKind::Local => &LOCAL_FIELDS,
            SetupKind::New if self.core.is_proxy() => &PROXY_FIELDS,
            SetupKind::New => &NEW_FIELDS,
            SetupKind::Rcon => &RCON_FIELDS,
        }
    }

    pub fn current(&self) -> Field {
        self.fields()[self.field.min(self.fields().len() - 1)]
    }

    pub fn input(&self, f: Field) -> Option<&TextInput> {
        match f {
            Field::Path => Some(&self.path),
            Field::Name => Some(&self.name),
            Field::Host => Some(&self.host),
            Field::Port => Some(&self.port),
            Field::Password => Some(&self.password),
            Field::Memory => Some(&self.memory),
            Field::GamePort => Some(&self.game_port),
            _ => None,
        }
    }

    fn input_mut(&mut self, f: Field) -> Option<&mut TextInput> {
        match f {
            Field::Path => Some(&mut self.path),
            Field::Name => Some(&mut self.name),
            Field::Host => Some(&mut self.host),
            Field::Port => Some(&mut self.port),
            Field::Password => Some(&mut self.password),
            Field::Memory => Some(&mut self.memory),
            Field::GamePort => Some(&mut self.game_port),
            _ => None,
        }
    }

    /// Ядро, для которого надо загрузить список версий (окно запускает загрузку и помечает Loading).
    pub fn wants_versions(&self) -> Option<Core> {
        (self.kind == SetupKind::New && !self.versions.contains_key(&self.core)).then_some(self.core)
    }

    pub fn set_versions(&mut self, core: Core, v: Versions) {
        self.versions.insert(core, v);
        if core == self.core {
            self.version_sel = 0;
            self.update_new_defaults();
        }
    }

    pub fn selected_version(&self) -> Option<&str> {
        match self.versions.get(&self.core) {
            Some(Versions::Ready(v)) => v.get(self.version_sel).map(String::as_str),
            _ => None,
        }
    }

    fn redetect(&mut self) {
        self.detection = detect_local(&self.path.value);
        if !self.name_edited && self.kind == SetupKind::Local {
            self.name.set(&self.detection.name);
        }
    }

    /// Имя и папка по умолчанию для нового сервера: «Paper 1.21.11», ~/minecraft/paper-1.21.11.
    fn update_new_defaults(&mut self) {
        if self.kind != SetupKind::New {
            return;
        }
        let v = self.selected_version().unwrap_or("").to_string();
        if !self.name_edited {
            self.name.set(format!("{} {v}", self.core.label()).trim());
        }
        if !self.new_path_edited {
            let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("minecraft");
            let mut dir = base.join(format!("{}-{v}", self.core.label().to_lowercase()).trim_end_matches('-'));
            let mut n = 2;
            while dir.exists() {
                dir = base.join(format!("{}-{v}-{n}", self.core.label().to_lowercase()));
                n += 1;
            }
            self.path.set(&dir.display().to_string());
        }
    }

    /// Прокси и игровой сервер по умолчанию отличаются портом и памятью: меняем, если пользователь их не трогал.
    fn swap_proxy_defaults(&mut self) {
        let (port, mem) = if self.core.is_proxy() {
            (("25565", "25577"), (format!("{}M", default_memory_mb()), "512M".to_string()))
        } else {
            (("25577", "25565"), ("512M".to_string(), format!("{}M", default_memory_mb())))
        };
        if self.game_port.value.trim() == port.0 {
            self.game_port.set(port.1);
        }
        if self.memory.value.trim() == mem.0 {
            self.memory.set(&mem.1);
        }
    }

    fn set_kind(&mut self, k: SetupKind) {
        let prev = self.kind;
        self.kind = k;
        self.field = 0;
        self.error = None;
        if prev == SetupKind::New || k == SetupKind::New {
            // У нового сервера своя папка по умолчанию — не путать с введённой существующей.
            if k == SetupKind::New {
                self.new_path_edited = false;
            } else if !self.new_path_edited {
                self.path.set("");
            }
            if !self.name_edited || prev == SetupKind::New {
                self.name_edited = false;
            }
        }
        match k {
            SetupKind::Local => self.redetect(),
            SetupKind::New => self.update_new_defaults(),
            SetupKind::Rcon => {
                if !self.name_edited {
                    self.name.set("");
                }
            }
        }
    }

    fn cycle(&mut self, f: Field, d: isize) {
        match f {
            Field::Kind => {
                let all = SetupKind::ALL;
                let i = all.iter().position(|k| *k == self.kind).unwrap_or(0) as isize;
                self.set_kind(all[(i + d).rem_euclid(all.len() as isize) as usize]);
            }
            Field::Core => {
                let all = Core::WITH_PROXY;
                let i = all.iter().position(|k| *k == self.core).unwrap_or(0) as isize;
                let was_proxy = self.core.is_proxy();
                self.core = all[(i + d).rem_euclid(all.len() as isize) as usize];
                self.version_sel = 0;
                if was_proxy != self.core.is_proxy() {
                    self.swap_proxy_defaults();
                }
                self.update_new_defaults();
            }
            Field::Version => {
                if let Some(Versions::Ready(v)) = self.versions.get(&self.core)
                    && !v.is_empty()
                {
                    // Список — новые сверху: → — к более старым.
                    self.version_sel = (self.version_sel as isize + d).clamp(0, v.len() as isize - 1) as usize;
                    self.update_new_defaults();
                }
            }
            Field::Eula => self.eula = !self.eula,
            _ => {}
        }
    }

    fn after_edit(&mut self, f: Field) {
        self.error = None;
        match f {
            Field::Path if self.kind == SetupKind::New => self.new_path_edited = !self.path.value.is_empty(),
            Field::Path => self.redetect(),
            Field::Name => self.name_edited = !self.name.value.is_empty(),
            _ => {}
        }
    }

    pub fn paste(&mut self, s: &str) {
        let f = self.current();
        if let Some(inp) = self.input_mut(f) {
            inp.insert_str(s.trim_end_matches(['\n', '\r']));
            self.after_edit(f);
        }
    }

    pub fn focus(&mut self, i: usize) {
        self.field = i.min(self.fields().len() - 1);
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> Outcome {
        if self.creating.is_some() {
            return Outcome::None; // ждём окончания создания
        }
        let n = self.fields().len();
        let f = self.current();
        match k.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Enter => return self.try_save(),
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.field = (self.field + n - 1) % n,
            KeyCode::Left if f.is_choice() => self.cycle(f, -1),
            KeyCode::Right | KeyCode::Char(' ') if f.is_choice() => self.cycle(f, 1),
            KeyCode::PageDown if f == Field::Version => self.cycle(f, 10),
            KeyCode::PageUp if f == Field::Version => self.cycle(f, -10),
            _ if f.is_choice() => {}
            _ => {
                if let Some(inp) = self.input_mut(f)
                    && inp.handle(&k)
                {
                    self.after_edit(f);
                }
            }
        }
        Outcome::None
    }

    fn try_save(&mut self) -> Outcome {
        let res = match self.kind {
            SetupKind::New => self.build_new().map(|r| Outcome::Create(Box::new(r))),
            _ => self.build().map(|c| Outcome::Save(Box::new(c))),
        };
        res.unwrap_or_else(|e| {
            self.error = Some(e);
            Outcome::None
        })
    }

    fn build_new(&self) -> Result<CreateRequest, String> {
        let version = self
            .selected_version()
            .ok_or_else(|| tr!("Список версий ещё не загружен", "The version list has not loaded yet").to_string())?
            .to_string();
        if self.path.value.trim().is_empty() {
            return Err(tr!("Укажите папку для сервера", "Choose a folder for the server").into());
        }
        let dir = normalize_path_input(&self.path.value);
        let dir = if dir.is_relative() { std::path::absolute(&dir).unwrap_or(dir) } else { dir };
        let memory_mb = parse_memory(&self.memory.value).filter(|m| *m >= 512).ok_or_else(|| {
            tr!("Память — например 4G или 4096M (не меньше 512M)", "Memory — e.g. 4G or 4096M (at least 512M)")
        })?;
        let port: u16 = self
            .game_port
            .value
            .trim()
            .parse()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| tr!("Порт — число от 1 до 65535", "Port must be a number from 1 to 65535"))?;
        if !self.eula && !self.core.is_proxy() {
            return Err(tr!(
                "Нужно принять EULA Minecraft (поле EULA → пробел)",
                "You need to accept the Minecraft EULA (EULA field → Space)"
            )
            .into());
        }
        let name = match self.name.value.trim() {
            "" => format!("{} {version}", self.core.label()),
            n => n.to_string(),
        };
        Ok(CreateRequest {
            opts: CreateOptions { core: self.core, version, dir, memory_mb, port, accept_eula: true },
            name,
        })
    }

    fn build(&self) -> Result<ServerConfig, String> {
        let name = self.name.value.trim();
        match self.kind {
            SetupKind::Local | SetupKind::New => {
                let d = &self.detection;
                if let Some(e) = &d.error {
                    return Err(e.clone());
                }
                let dir = d.dir.clone().ok_or(tr!("папка не выбрана", "no folder selected"))?;
                let name = if name.is_empty() { d.name.as_str() } else { name };
                Ok(ServerConfig::new_local(name, dir, d.command.clone(), d.backup_paths.clone()))
            }
            SetupKind::Rcon => {
                if name.is_empty() {
                    return Err(tr!("Введите название", "Enter a name").into());
                }
                let host = match self.host.value.trim() {
                    "" => "127.0.0.1",
                    h => h,
                };
                let port: u16 = self.port.value.trim().parse().map_err(|_| {
                    tr!("Порт — число от 1 до 65535", "Port must be a number from 1 to 65535").to_string()
                })?;
                if self.password.value.is_empty() {
                    return Err(tr!(
                        "Введите пароль RCON (rcon.password из server.properties)",
                        "Enter the RCON password (rcon.password from server.properties)"
                    )
                    .into());
                }
                Ok(ServerConfig::new_rcon(name, host, port, &self.password.value))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }

    #[test]
    fn paste_path_and_save_local() {
        let d = std::env::temp_dir().join(format!("pechka-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("server.jar"), b"").unwrap();
        let mut s = Setup::new(true);
        s.path.set("");
        s.field = 1;
        s.paste(&format!("{}\n", d.display()));
        assert!(s.detection.error.is_none(), "{:?}", s.detection.error);
        assert!(s.name.value.starts_with("pechka-setup-"));
        match s.handle_key(key(KeyCode::Enter)) {
            Outcome::Save(c) => {
                assert_eq!(c.dir.unwrap(), d.canonicalize().unwrap());
                assert!(c.command.contains(&"server.jar".to_string()));
            }
            _ => panic!("ожидалось сохранение"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_path_shows_error_and_rcon_form_validates() {
        let mut s = Setup::new(false);
        s.path.set("");
        s.field = 1;
        s.paste("/definitely/not/here");
        assert!(matches!(s.handle_key(key(KeyCode::Enter)), Outcome::None));
        assert!(s.error.is_some());
        s.field = 0;
        s.handle_key(key(KeyCode::Left));
        assert_eq!(s.kind, SetupKind::Rcon);
        assert!(matches!(s.handle_key(key(KeyCode::Enter)), Outcome::None), "без имени и пароля нельзя");
        s.name.set("Remote");
        s.password.set("pw");
        assert!(matches!(s.handle_key(key(KeyCode::Enter)), Outcome::Save(_)));
        assert!(matches!(s.handle_key(key(KeyCode::Esc)), Outcome::Cancel));
    }

    #[test]
    fn new_server_form() {
        let mut s = Setup::new(false);
        s.field = 0;
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.kind, SetupKind::New);
        assert_eq!(s.wants_versions(), Some(Core::Paper));
        s.set_versions(Core::Paper, Versions::Ready(vec!["1.21.11".into(), "1.21.10".into()]));
        assert_eq!(s.wants_versions(), None);
        assert_eq!(s.name.value, "Paper 1.21.11");
        assert!(s.path.value.contains("paper-1.21.11"), "{}", s.path.value);
        assert!(matches!(s.handle_key(key(KeyCode::Enter)), Outcome::None), "без EULA нельзя");
        s.eula = true;
        s.memory.set("3G");
        match s.handle_key(key(KeyCode::Enter)) {
            Outcome::Create(r) => {
                assert_eq!(r.opts.memory_mb, 3072);
                assert_eq!(r.opts.version, "1.21.11");
                assert_eq!(r.opts.port, 25565);
                assert_eq!(r.name, "Paper 1.21.11");
            }
            _ => panic!("ожидалось создание"),
        }
    }

    #[test]
    fn memory_parsing() {
        assert_eq!(parse_memory("4G"), Some(4096));
        assert_eq!(parse_memory("4096"), Some(4096));
        assert_eq!(parse_memory("2.5 ГБ"), Some(2560));
        assert_eq!(parse_memory("4"), Some(4096));
        assert_eq!(parse_memory("abc"), None);
    }
}
