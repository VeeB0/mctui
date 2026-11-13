//! Экран добавления сервера: при первом запуске и по клавише `a`.

use crate::config::{Detection, ServerConfig, detect_local};
use crate::input::TextInput;
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupKind {
    Local,
    Rcon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Kind,
    Path,
    Name,
    Host,
    Port,
    Password,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Kind => "Тип",
            Field::Path => "Папка сервера",
            Field::Name => "Название",
            Field::Host => "Адрес",
            Field::Port => "Порт RCON",
            Field::Password => "Пароль RCON",
        }
    }
}

pub enum Outcome {
    None,
    Cancel,
    Save(Box<ServerConfig>),
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
    name_edited: bool,
    pub detection: Detection,
    pub error: Option<String>,
}

const LOCAL_FIELDS: [Field; 3] = [Field::Kind, Field::Path, Field::Name];
const RCON_FIELDS: [Field; 5] = [Field::Kind, Field::Name, Field::Host, Field::Port, Field::Password];

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
            name_edited: false,
            detection: Detection::default(),
            error: None,
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
            SetupKind::Rcon => &RCON_FIELDS,
        }
    }

    pub fn current(&self) -> Field {
        self.fields()[self.field.min(self.fields().len() - 1)]
    }

    pub fn input(&self, f: Field) -> Option<&TextInput> {
        match f {
            Field::Kind => None,
            Field::Path => Some(&self.path),
            Field::Name => Some(&self.name),
            Field::Host => Some(&self.host),
            Field::Port => Some(&self.port),
            Field::Password => Some(&self.password),
        }
    }

    fn input_mut(&mut self, f: Field) -> Option<&mut TextInput> {
        match f {
            Field::Kind => None,
            Field::Path => Some(&mut self.path),
            Field::Name => Some(&mut self.name),
            Field::Host => Some(&mut self.host),
            Field::Port => Some(&mut self.port),
            Field::Password => Some(&mut self.password),
        }
    }

    fn redetect(&mut self) {
        self.detection = detect_local(&self.path.value);
        if !self.name_edited && self.kind == SetupKind::Local {
            self.name.set(&self.detection.name);
        }
    }

    fn toggle_kind(&mut self) {
        self.kind = match self.kind {
            SetupKind::Local => SetupKind::Rcon,
            SetupKind::Rcon => SetupKind::Local,
        };
        self.field = 0;
        self.error = None;
        if self.kind == SetupKind::Local && !self.name_edited {
            self.redetect();
        }
    }

    fn after_edit(&mut self, f: Field) {
        self.error = None;
        match f {
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
        let n = self.fields().len();
        match k.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Enter => return self.try_save(),
            KeyCode::Tab | KeyCode::Down => self.field = (self.field + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.field = (self.field + n - 1) % n,
            _ if self.current() == Field::Kind => {
                if matches!(k.code, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) {
                    self.toggle_kind();
                }
            }
            _ => {
                let f = self.current();
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
        match self.build() {
            Ok(c) => Outcome::Save(Box::new(c)),
            Err(e) => {
                self.error = Some(e);
                Outcome::None
            }
        }
    }

    fn build(&self) -> Result<ServerConfig, String> {
        let name = self.name.value.trim();
        match self.kind {
            SetupKind::Local => {
                let d = &self.detection;
                if let Some(e) = &d.error {
                    return Err(e.clone());
                }
                let dir = d.dir.clone().ok_or("папка не выбрана")?;
                let name = if name.is_empty() { d.name.as_str() } else { name };
                Ok(ServerConfig::new_local(name, dir, d.command.clone(), d.backup_paths.clone()))
            }
            SetupKind::Rcon => {
                if name.is_empty() {
                    return Err("Введите название".into());
                }
                let host = match self.host.value.trim() {
                    "" => "127.0.0.1",
                    h => h,
                };
                let port: u16 = self.port.value.trim().parse().map_err(|_| "Порт — число от 1 до 65535".to_string())?;
                if self.password.value.is_empty() {
                    return Err("Введите пароль RCON (rcon.password из server.properties)".into());
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
        let d = std::env::temp_dir().join(format!("mctui-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("server.jar"), b"").unwrap();
        let mut s = Setup::new(true);
        s.path.set("");
        s.field = 1;
        s.paste(&format!("{}\n", d.display()));
        assert!(s.detection.error.is_none(), "{:?}", s.detection.error);
        assert!(s.name.value.starts_with("mctui-setup-"));
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
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.kind, SetupKind::Rcon);
        assert!(matches!(s.handle_key(key(KeyCode::Enter)), Outcome::None), "без имени и пароля нельзя");
        s.name.set("Remote");
        s.password.set("pw");
        assert!(matches!(s.handle_key(key(KeyCode::Enter)), Outcome::Save(_)));
        assert!(matches!(s.handle_key(key(KeyCode::Esc)), Outcome::Cancel));
    }
}
