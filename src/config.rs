//! Конфигурация серверов (servers.toml) и автоопределение локального сервера по папке.
//!
//! Конфиг живёт в папке настроек пользователя и не трогается при обновлении программы:
//! - macOS / Linux: `~/.config/mctui/servers.toml`
//! - Windows: `%APPDATA%\mctui\servers.toml`

use crate::tr;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Local,
    Rcon,
}

/// Как делать бэкап: архив `.tar.gz` целиком или инкрементальный снимок-папка с жёсткими ссылками.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackupMode {
    /// `<prefix>-YYYYmmdd-HHMMSS.tar.gz`
    #[default]
    Full,
    /// `<prefix>-YYYYmmdd-HHMMSS.snap/`: неизменённые файлы — жёсткие ссылки на предыдущий снимок.
    Incremental,
}

impl BackupMode {
    pub fn is_full(&self) -> bool {
        *self == BackupMode::Full
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    /// Язык интерфейса: "ru", "en" или не задан — как в системе.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(rename = "server", default)]
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub name: String,
    pub kind: Kind,
    // --- локальные ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    #[serde(default = "d_command")]
    pub command: Vec<String>,
    #[serde(default = "d_true")]
    pub auto_restart: bool,
    #[serde(default = "d_restart_delay")]
    pub restart_delay_secs: u64,
    #[serde(default = "d_stop_timeout")]
    pub stop_timeout_secs: u64,
    #[serde(default = "d_backup_paths")]
    pub backup_paths: Vec<String>,
    #[serde(default = "d_backup_dir")]
    pub backup_dir: String,
    #[serde(default = "d_backup_keep")]
    pub backup_keep: usize,
    /// "full" (архив .tar.gz, по умолчанию) или "incremental" (снимок с жёсткими ссылками).
    #[serde(default, skip_serializing_if = "BackupMode::is_full")]
    pub backup_mode: BackupMode,
    /// Куда ещё копировать готовый бэкап: путь к папке (другой диск, NAS, папка Dropbox / iCloud /
    /// Яндекс.Диска) или `rclone:<remote>:<path>`. Там тоже хранятся последние `backup_keep`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backup_copy_to: Vec<String>,
    /// Запускать сервер вместе с фоновой службой (при входе в систему).
    #[serde(default)]
    pub autostart: bool,
    /// Авто-бэкап каждые N часов, пока сервер работает (0 — выключен).
    #[serde(default)]
    pub backup_every_hours: u32,
    /// Ежедневный рестарт в указанное местное время, "HH:MM" (с предупреждениями игрокам).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_times: Vec<String>,
    // --- RCON ---
    #[serde(default = "d_host")]
    pub host: String,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env: Option<String>,
    /// Путь к логу для RCON-сервера на этой же машине (опционально).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<PathBuf>,
}

fn d_command() -> Vec<String> {
    if cfg!(windows) { vec!["cmd".into(), "/C".into(), "start.bat".into()] } else { vec!["./start.sh".into()] }
}
fn d_true() -> bool {
    true
}
fn d_restart_delay() -> u64 {
    5
}
fn d_stop_timeout() -> u64 {
    60
}
fn d_backup_paths() -> Vec<String> {
    vec!["world".into()]
}
fn d_backup_dir() -> String {
    "backups".into()
}
fn d_backup_keep() -> usize {
    10
}
fn d_host() -> String {
    "127.0.0.1".into()
}
fn d_port() -> u16 {
    25575
}

impl ServerConfig {
    fn base(name: &str, kind: Kind) -> Self {
        Self {
            name: name.into(),
            kind,
            dir: None,
            command: d_command(),
            auto_restart: true,
            restart_delay_secs: d_restart_delay(),
            stop_timeout_secs: d_stop_timeout(),
            backup_paths: d_backup_paths(),
            backup_dir: d_backup_dir(),
            backup_keep: d_backup_keep(),
            backup_mode: BackupMode::Full,
            backup_copy_to: vec![],
            autostart: false,
            backup_every_hours: 0,
            restart_times: vec![],
            host: d_host(),
            port: d_port(),
            password: None,
            password_env: None,
            log_path: None,
        }
    }

    pub fn new_local(name: &str, dir: PathBuf, command: Vec<String>, backup_paths: Vec<String>) -> Self {
        Self { dir: Some(dir), command, backup_paths, ..Self::base(name, Kind::Local) }
    }

    pub fn new_rcon(name: &str, host: &str, port: u16, password: &str) -> Self {
        Self { host: host.into(), port, password: Some(password.into()), ..Self::base(name, Kind::Rcon) }
    }

    pub fn dir(&self) -> &Path {
        self.dir.as_deref().unwrap_or(Path::new("."))
    }

    pub fn properties_path(&self) -> Option<PathBuf> {
        match self.kind {
            Kind::Local => Some(self.dir().join("server.properties")),
            Kind::Rcon => None,
        }
    }

    /// Лимит памяти Java (-Xmx) в МиБ: из аргументов команды или из скрипта запуска.
    pub fn max_memory_mb(&self) -> Option<u64> {
        if self.kind != Kind::Local {
            return None;
        }
        let mut text = self.command.join(" ");
        if let Some(script) = self.command.iter().find(|a| {
            let l = a.to_ascii_lowercase();
            l.ends_with(".sh") || l.ends_with(".bat") || l.ends_with(".cmd") || l.ends_with(".command")
        }) && let Ok(t) = std::fs::read_to_string(self.dir().join(script))
        {
            text.push(' ');
            text.push_str(&t);
        }
        parse_xmx(&text)
    }

    /// Время рестартов по расписанию как (час, минута); неверные записи пропускаются.
    pub fn restart_hm(&self) -> Vec<(u32, u32)> {
        self.restart_times.iter().filter_map(|t| parse_hm(t)).collect()
    }

    /// ops.json: в папке локального сервера; для RCON — рядом с log_path (…/logs/latest.log → …/ops.json).
    pub fn ops_path(&self) -> Option<PathBuf> {
        match self.kind {
            Kind::Local => Some(self.dir().join("ops.json")),
            Kind::Rcon => self.log_path.as_ref().and_then(|p| p.parent()?.parent()).map(|d| d.join("ops.json")),
        }
    }

    pub fn logs_dir(&self) -> Option<PathBuf> {
        match self.kind {
            Kind::Local => Some(self.dir().join("logs")),
            Kind::Rcon => self.log_path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf)),
        }
    }

    pub fn backup_dir(&self) -> PathBuf {
        let p = PathBuf::from(&self.backup_dir);
        if p.is_absolute() { p } else { self.dir().join(p) }
    }

    pub fn rcon_password(&self) -> Option<String> {
        if let Some(v) = self.password_env.as_ref().and_then(|var| std::env::var(var).ok()) {
            return Some(v);
        }
        self.password.clone()
    }
}

/// Путь к конфигу по умолчанию (можно переопределить переменной MCTUI_CONFIG).
/// "-Xmx4G" / "-Xmx4096M" / "-Xmx4g" → МиБ (последнее вхождение побеждает, как у Java).
pub fn parse_xmx(text: &str) -> Option<u64> {
    let mut found = None;
    for (i, _) in text.match_indices("-Xmx") {
        let rest = &text[i + 4..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(n) = digits.parse::<u64>() else { continue };
        let unit = rest[digits.len()..].chars().next().map(|c| c.to_ascii_lowercase());
        found = Some(match unit {
            Some('g') => n * 1024,
            Some('m') => n,
            Some('k') => n / 1024,
            _ => n / (1024 * 1024),
        });
    }
    found
}

/// "05:00" / "5:30" → (5, 0) / (5, 30).
pub fn parse_hm(t: &str) -> Option<(u32, u32)> {
    let (h, m) = t.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

/// Папка служебных файлов (daemon.json, история графиков) — рядом с servers.toml.
pub fn state_dir(config_path: &Path) -> PathBuf {
    config_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MCTUI_CONFIG") {
        return PathBuf::from(p);
    }
    let base = if cfg!(windows) {
        dirs::config_dir()
    } else {
        // На macOS тоже ~/.config — так привычнее для консольных программ.
        dirs::home_dir().map(|h| h.join(".config"))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("mctui").join("servers.toml")
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        for s in &cfg.servers {
            if s.kind == Kind::Local && s.dir.is_none() {
                bail!(
                    "{}",
                    tr!(
                        format!("сервер «{}»: для kind = \"local\" нужен dir", s.name),
                        format!("server “{}”: kind = \"local\" needs dir", s.name)
                    )
                );
            }
            if s.kind == Kind::Local && s.command.is_empty() {
                bail!(
                    "{}",
                    tr!(format!("сервер «{}»: пустой command", s.name), format!("server “{}”: empty command", s.name))
                );
            }
        }
        Ok(cfg)
    }

    /// Загружает конфиг. Если файла нет — пустой конфиг (покажется экран добавления сервера).
    pub fn load(explicit: Option<PathBuf>) -> Result<(Self, PathBuf)> {
        let path = explicit.unwrap_or_else(default_path);
        if !path.exists() {
            return Ok((Self::default(), path));
        }
        let text = std::fs::read_to_string(&path).with_context(|| {
            tr!(format!("не читается {}", path.display()), format!("cannot read {}", path.display()))
        })?;
        let cfg = Self::parse(&text)
            .with_context(|| tr!(format!("ошибка в {}", path.display()), format!("error in {}", path.display())))?;
        Ok((cfg, path))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                tr!(format!("не создать {}", parent.display()), format!("cannot create {}", parent.display()))
            })?;
        }
        let body = toml::to_string_pretty(self)?;
        let text = format!(
            "{}\n# https://github.com/{}/blob/main/servers.example.toml\n\n{body}",
            tr!(
                "# Настройки mctui. Файл можно править руками; обновление программы его не трогает.",
                "# mctui settings. You can edit this file by hand; updates do not touch it."
            ),
            crate::update::REPO
        );
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

// ───────────────────────────── автоопределение сервера ─────────────────────────────

/// Результат проверки папки, введённой пользователем.
#[derive(Debug, Clone, Default)]
pub struct Detection {
    pub dir: Option<PathBuf>,
    pub name: String,
    pub command: Vec<String>,
    pub backup_paths: Vec<String>,
    /// Найденное хорошее (✓).
    pub found: Vec<String>,
    /// Предупреждения — сохранить можно (!).
    pub warnings: Vec<String>,
    /// Ошибка — сохранить нельзя (✗).
    pub error: Option<String>,
}

/// Превращает вставленный текст в путь: кавычки, `~`, `file://`, экранированные пробелы из Finder.
pub fn normalize_path_input(s: &str) -> PathBuf {
    let mut t = s.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
    if let Some(rest) = t.strip_prefix("file://") {
        t = rest.replace("%20", " ");
    }
    if !cfg!(windows) {
        t = t.replace("\\ ", " ");
    }
    if (t == "~" || t.starts_with("~/") || t.starts_with("~\\"))
        && let Some(h) = dirs::home_dir()
    {
        t = h.join(t.trim_start_matches('~').trim_start_matches(['/', '\\'])).display().to_string();
    }
    let p = PathBuf::from(t);
    if p.is_relative() { std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p) } else { p }
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Java, лежащая рядом с сервером (например, jdk25/ внутри папки сервера), иначе `java` из PATH.
fn find_java(dir: &Path) -> String {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            for cand in [p.join("Contents/Home/bin").join(exe), p.join("bin").join(exe)] {
                if cand.is_file()
                    && let Ok(rel) = cand.strip_prefix(dir)
                {
                    return format!(".{}{}", std::path::MAIN_SEPARATOR, rel.display());
                }
            }
        }
    }
    "java".into()
}

fn pick_jar(dir: &Path) -> Option<String> {
    let mut jars: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".jar"))
        .collect();
    jars.sort();
    let score = |n: &str| {
        let l = n.to_lowercase();
        ["purpur", "paper", "folia", "spigot", "fabric-server", "forge", "neoforge", "minecraft_server", "server"]
            .iter()
            .position(|k| l.contains(k))
            .unwrap_or(99)
    };
    jars.sort_by_key(|n| score(n));
    jars.into_iter().next()
}

fn script_command(dir: &Path, file: &str) -> Vec<String> {
    if file.ends_with(".bat") || file.ends_with(".cmd") {
        vec!["cmd".into(), "/C".into(), file.into()]
    } else if is_executable(&dir.join(file)) {
        vec![format!("./{file}")]
    } else {
        vec!["sh".into(), file.into()]
    }
}

fn jar_command(dir: &Path, jar: &str) -> Vec<String> {
    vec![find_java(dir), "-Xms1G".into(), "-Xmx2G".into(), "-jar".into(), jar.into(), "--nogui".into()]
}

/// Проверяет папку (или файл скрипта/jar внутри неё) и подбирает команду запуска.
pub fn detect_local(input: &str) -> Detection {
    let mut d = Detection::default();
    if input.trim().is_empty() {
        d.error = Some(tr!("Вставьте путь к папке сервера", "Paste the path to the server folder").into());
        return d;
    }
    let mut path = normalize_path_input(input);
    let mut chosen_file: Option<String> = None;
    if path.is_file() {
        chosen_file = path.file_name().map(|n| n.to_string_lossy().into_owned());
        path = path.parent().map(Path::to_path_buf).unwrap_or(path);
    }
    if !path.is_dir() {
        d.error =
            Some(tr!(format!("Папка не найдена: {}", path.display()), format!("Folder not found: {}", path.display())));
        return d;
    }
    let path = path.canonicalize().unwrap_or(path);
    d.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "server".into());

    let scripts: &[&str] =
        if cfg!(windows) { &["start.bat", "run.bat", "start.cmd"] } else { &["start.sh", "run.sh", "start.command"] };
    let cmd = match chosen_file {
        Some(f) if f.ends_with(".jar") => Some((jar_command(&path, &f), format!("jar: {f}"))),
        Some(f) => Some((script_command(&path, &f), tr!(format!("скрипт запуска: {f}"), format!("start script: {f}")))),
        None => scripts
            .iter()
            .find(|s| path.join(s).is_file())
            .map(|s| (script_command(&path, s), tr!(format!("скрипт запуска: {s}"), format!("start script: {s}"))))
            .or_else(|| pick_jar(&path).map(|j| (jar_command(&path, &j), format!("jar: {j}")))),
    };
    match cmd {
        Some((c, what)) => {
            d.found.push(what);
            d.command = c;
        }
        None => {
            d.error = Some(
                tr!(
                    "В папке нет скрипта запуска (start.sh / start.bat) и .jar сервера",
                    "No start script (start.sh / start.bat) or server .jar in the folder"
                )
                .into(),
            );
            return d;
        }
    }
    if d.command[0] == "java" && !d.command.is_empty() {
        d.warnings.push(
            tr!(
                "Java будет взята из PATH — убедитесь, что установлена нужная версия",
                "Java comes from PATH — make sure the right version is installed"
            )
            .into(),
        );
    } else if d.command[0].contains("java") {
        d.found.push(format!("Java: {}", d.command[0]));
    }

    match std::fs::read_to_string(path.join("server.properties")) {
        Ok(text) => {
            let props = crate::properties::Properties::parse(&text);
            let port = props.get("server-port").unwrap_or("25565").to_string();
            d.found.push(tr!(format!("server.properties (порт {port})"), format!("server.properties (port {port})")));
            let level = props.get("level-name").unwrap_or("world").to_string();
            d.backup_paths = vec![level];
        }
        Err(_) => {
            d.warnings.push(
                tr!(
                    "server.properties нет — сервер создаст его при первом запуске",
                    "no server.properties — the server creates it on first start"
                )
                .into(),
            );
            d.backup_paths = d_backup_paths();
        }
    }
    let eula = std::fs::read_to_string(path.join("eula.txt")).unwrap_or_default();
    if !eula.lines().any(|l| l.trim() == "eula=true") {
        d.warnings.push(
            tr!(
                "EULA не принята: сервер не запустится, пока в eula.txt не будет eula=true",
                "EULA not accepted: the server will not start until eula.txt has eula=true"
            )
            .into(),
        );
    }
    d.dir = Some(path);
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mctui-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_local_and_rcon_with_defaults() {
        let dir = std::env::temp_dir();
        let text = format!(
            "[[server]]\nname = \"A\"\nkind = \"local\"\ndir = {:?}\n\n[[server]]\nname = \"B\"\nkind = \"rcon\"\nhost = \"example.org\"\npassword = \"p\"\n",
            dir.display().to_string()
        );
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.servers.len(), 2);
        assert_eq!(c.servers[0].command, d_command());
        assert_eq!(c.servers[0].backup_keep, 10);
        assert_eq!(c.servers[1].port, 25575);
        assert_eq!(c.servers[1].rcon_password().as_deref(), Some("p"));
    }

    #[test]
    fn backup_mode_and_copy_targets() {
        let dir = std::env::temp_dir();
        let base = format!("[[server]]\nname = \"A\"\nkind = \"local\"\ndir = {:?}\n", dir.display().to_string());
        let c = Config::parse(&base).unwrap();
        assert_eq!(c.servers[0].backup_mode, BackupMode::Full);
        assert!(c.servers[0].backup_copy_to.is_empty());
        // значения по умолчанию не пишутся в файл
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(!text.contains("backup_mode"));
        assert!(!text.contains("backup_copy_to"));

        let c = Config::parse(&format!(
            "{base}backup_mode = \"incremental\"\nbackup_copy_to = [\"/Volumes/Disk/mc\", \"rclone:yadisk:mc\"]\n"
        ))
        .unwrap();
        assert_eq!(c.servers[0].backup_mode, BackupMode::Incremental);
        assert_eq!(c.servers[0].backup_copy_to, vec!["/Volumes/Disk/mc", "rclone:yadisk:mc"]);
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("backup_mode = \"incremental\""));
        let again = Config::parse(&text).unwrap();
        assert_eq!(again.servers[0].backup_mode, BackupMode::Incremental);
        assert_eq!(again.servers[0].backup_copy_to.len(), 2);
        assert!(Config::parse(&format!("{base}backup_mode = \"weird\"\n")).is_err());
    }

    #[test]
    fn local_without_dir_is_error() {
        assert!(Config::parse("[[server]]\nname = \"A\"\nkind = \"local\"\n").is_err());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let d = tmp("save");
        let path = d.join("sub/servers.toml");
        let cfg = Config {
            language: None,
            servers: vec![
                ServerConfig::new_local("Мир", d.clone(), vec!["./start.sh".into()], vec!["world".into()]),
                ServerConfig::new_rcon("R", "1.2.3.4", 25575, "pw"),
            ],
        };
        cfg.save(&path).unwrap();
        let (loaded, p) = Config::load(Some(path.clone())).unwrap();
        assert_eq!(p, path);
        assert_eq!(loaded.servers.len(), 2);
        assert_eq!(loaded.servers[0].name, "Мир");
        assert_eq!(loaded.servers[1].password.as_deref(), Some("pw"));
        let (empty, _) = Config::load(Some(d.join("missing.toml"))).unwrap();
        assert!(empty.servers.is_empty());
    }

    #[test]
    fn normalizes_pasted_paths() {
        let p = normalize_path_input("  '/tmp/My\\ Server'  ");
        if cfg!(windows) {
            assert!(p.display().to_string().contains("My"));
        } else {
            assert_eq!(p, PathBuf::from("/tmp/My Server"));
            assert_eq!(normalize_path_input("file:///tmp/a%20b"), PathBuf::from("/tmp/a b"));
        }
        let home = dirs::home_dir().unwrap();
        assert_eq!(normalize_path_input("~/x"), home.join("x"));
    }

    #[test]
    fn detects_script_jar_and_problems() {
        let d = tmp("detect");
        assert!(detect_local(d.join("nope").to_str().unwrap()).error.is_some());
        assert!(detect_local(d.to_str().unwrap()).error.is_some(), "пустая папка — ошибка");

        std::fs::write(d.join("paper-1.21.jar"), b"").unwrap();
        std::fs::write(d.join("other.jar"), b"").unwrap();
        let r = detect_local(d.to_str().unwrap());
        assert!(r.error.is_none());
        assert_eq!(r.command, vec!["java", "-Xms1G", "-Xmx2G", "-jar", "paper-1.21.jar", "--nogui"]);
        assert!(r.warnings.iter().any(|w| w.contains("EULA")));

        let script = if cfg!(windows) { "start.bat" } else { "start.sh" };
        std::fs::write(d.join(script), b"echo").unwrap();
        std::fs::write(d.join("eula.txt"), b"eula=true\n").unwrap();
        std::fs::write(d.join("server.properties"), b"server-port=25570\nlevel-name=myworld\n").unwrap();
        let r = detect_local(d.to_str().unwrap());
        assert!(r.found.iter().any(|f| f.contains(script)));
        assert!(r.found.iter().any(|f| f.contains("25570")));
        assert_eq!(r.backup_paths, vec!["myworld"]);
        assert!(!r.warnings.iter().any(|w| w.contains("EULA")));
        // путь к файлу скрипта — берётся его папка
        let r2 = detect_local(d.join(script).to_str().unwrap());
        assert_eq!(r2.dir, r.dir);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn xmx_and_times() {
        assert_eq!(parse_xmx("java -Xms2G -Xmx4G -jar x.jar"), Some(4096));
        assert_eq!(parse_xmx("-Xmx512m"), Some(512));
        assert_eq!(parse_xmx("-Xmx2G ... -Xmx6g"), Some(6144));
        assert_eq!(parse_xmx("java -jar x.jar"), None);
        assert_eq!(parse_hm("05:00"), Some((5, 0)));
        assert_eq!(parse_hm(" 7:30 "), Some((7, 30)));
        assert_eq!(parse_hm("24:00"), None);
        assert_eq!(parse_hm("abc"), None);
    }
}
