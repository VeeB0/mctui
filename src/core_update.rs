//! Обновление ядра сервера (jar): какое ядро стоит ([`detect`]), что вышло новее ([`check`]) и замена ([`apply`]).
//!
//! - Сборка для той же версии Minecraft (Paper / Purpur / Velocity — номер сборки, Fabric — загрузчик и
//!   лаунчер) безопасна: миры не меняются.
//! - Переход на новую версию Minecraft ([`is_major_upgrade`]) необратим для миров: после запуска их не открыть
//!   старой версией. Интерфейс должен предупредить и сначала сделать резервную копию.
//!
//! [`apply`] скачивает jar (с проверкой контрольной суммы, если API её даёт), убирает старый в
//! `<dir>/.mctui-old/` (как `plugins.rs`), меняет имя jar в скрипте запуска (только этот токен) и, если
//! Java скрипта слишком старая для новой версии, ставит подходящую: системную, уже лежащую в папке или
//! скачанную Temurin JRE (как при создании сервера).
//!
//! Сервер должен быть остановлен — это гарантирует вызывающая сторона (по состоянию процесса в службе).
//! Файл `world/session.lock` намеренно не проверяется: на Unix его блокировка не видна другим процессам
//! надёжно, а сервер, которым управляет mctui, и так известен как остановленный.

use anyhow::{Context, Result, bail};
use std::cmp::Ordering;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::create::{self, Core, Progress};
use crate::tr;

/// Скрипты запуска, которые ищем (сначала родные для системы).
const SCRIPTS_UNIX: [&str; 3] = ["start.sh", "run.sh", "start.command"];
const SCRIPTS_WIN: [&str; 3] = ["start.bat", "run.bat", "start.cmd"];

/// Что установлено в папке сервера.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub core: Core,
    /// Версия Minecraft (для Velocity — версия прокси, например `3.4.0-SNAPSHOT`).
    pub mc_version: String,
    /// Номер сборки Paper / Purpur / Velocity; у Fabric — `загрузчик/лаунчер` (`0.20.1/1.1.2`); у Vanilla нет.
    pub build: Option<String>,
    /// Полный путь к jar.
    pub jar: PathBuf,
    /// Скрипт запуска, в котором указан jar (если сервер запускается скриптом).
    pub script: Option<PathBuf>,
    /// Java из скрипта или команды (`java`, `./jdk25/Contents/Home/bin/java`…), если найдена.
    pub java: Option<String>,
}

impl Installed {
    /// «Purpur 26.3 #2658», «Fabric 1.21.11 (loader 0.20.1)», «Vanilla 1.21.11».
    pub fn label(&self) -> String {
        let core = self.core.label();
        match (&self.core, &self.build) {
            (Core::Fabric, Some(b)) => {
                let loader = b.split('/').next().unwrap_or(b);
                format!("{core} {} (loader {loader})", self.mc_version)
            }
            (_, Some(b)) => format!("{core} {} #{b}", self.mc_version),
            (_, None) => format!("{core} {}", self.mc_version),
        }
    }

    pub fn jar_name(&self) -> String {
        self.jar.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// Последняя сборка для той же версии (как [`Installed::build`]); `None` у Vanilla или если API не ответил.
    pub latest_build: Option<String>,
    /// Есть более новая сборка той же версии (или установленная сборка неизвестна).
    pub build_update: bool,
    /// Более новые версии (Minecraft или Velocity), новые сверху.
    pub newer_versions: Vec<String>,
}

impl UpdateCheck {
    pub fn has_updates(&self) -> bool {
        self.build_update || !self.newer_versions.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub version: String,
    pub build: Option<String>,
    /// Имя нового jar в папке сервера.
    pub jar: String,
    /// Куда убран старый jar (`<dir>/.mctui-old/…`).
    pub old_jar: PathBuf,
    /// Изменённый скрипт запуска.
    pub script: Option<PathBuf>,
    /// Новая Java (как в скрипте), если её пришлось сменить. Если сервер запускается не скриптом, а командой
    /// из настроек, её нужно обновить через [`rewrite_command`].
    pub java: Option<String>,
    /// Что сделано (на языке интерфейса).
    pub notes: Vec<String>,
    /// Что проверить пользователю: плагины/моды под новую версию, Java и т. п.
    pub warnings: Vec<String>,
}

// ───────────────────────────── версии ─────────────────────────────

/// Сравнение версий: числовые части по точкам, затем релиз новее суффикса (`3.4.0` > `3.4.0-SNAPSHOT`,
/// `1.21.11` > `1.21.11-pre5`).
pub fn cmp_version(a: &str, b: &str) -> Ordering {
    let split = |v: &str| {
        let (base, suffix) = v.split_once('-').map_or((v, None), |(b, s)| (b, Some(s.to_string())));
        (create::version_key(base), suffix)
    };
    let (ka, sa) = split(a);
    let (kb, sb) = split(b);
    ka.cmp(&kb).then_with(|| match (sa, sb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(&y),
    })
}

/// Переход на другую версию Minecraft: миры конвертируются, назад дороги нет — нужна резервная копия.
/// Для Velocity (миров нет) — всегда `false`.
pub fn is_major_upgrade(inst: &Installed, target_version: &str) -> bool {
    !inst.core.is_proxy() && cmp_version(target_version.trim(), &inst.mc_version) != Ordering::Equal
}

fn build_is_newer(installed: Option<&str>, latest: Option<&str>) -> bool {
    match (installed, latest) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(a), Some(b)) => match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(x), Ok(y)) => y > x,
            // Fabric: `загрузчик/лаунчер` — сравниваем по частям.
            _ => {
                let (pa, pb): (Vec<&str>, Vec<&str>) = (a.split('/').collect(), b.split('/').collect());
                if pa.len() != pb.len() {
                    return a != b;
                }
                let cmps: Vec<Ordering> = pa.iter().zip(&pb).map(|(x, y)| cmp_version(y, x)).collect();
                cmps.contains(&Ordering::Greater) && !cmps.contains(&Ordering::Less)
            }
        },
    }
}

// ───────────────────────────── jar ─────────────────────────────

fn looks_like_version(v: &str) -> bool {
    v.chars().next().is_some_and(|c| c.is_ascii_digit())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || ".-_+".contains(c))
}

/// Ядро, версия и сборка по имени jar: `paper-1.21.4-232.jar`, `purpur-26.3-2658.jar`,
/// `velocity-3.4.0-SNAPSHOT-500.jar`, `fabric-server-mc.1.21.11-loader.0.20.1-launcher.1.1.2.jar`,
/// `minecraft_server.1.21.11.jar`. Неговорящие имена (`server.jar`) — `None`.
pub fn parse_jar_name(name: &str) -> Option<(Core, String, Option<String>)> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".jar")?;
    let stem = &name[..stem.len()];
    if let Some(rest) = stem.strip_prefix("fabric-server-mc.") {
        let (mc, rest) = rest.split_once("-loader.")?;
        let (loader, launcher) = rest.split_once("-launcher.")?;
        return (looks_like_version(mc) && looks_like_version(loader) && looks_like_version(launcher))
            .then(|| (Core::Fabric, mc.to_string(), Some(format!("{loader}/{launcher}"))));
    }
    if let Some(v) = stem.strip_prefix("minecraft_server.") {
        return looks_like_version(v).then(|| (Core::Vanilla, v.to_string(), None));
    }
    for (prefix, core) in [("paper-", Core::Paper), ("purpur-", Core::Purpur), ("velocity-", Core::Velocity)] {
        let Some(rest) = stem.strip_prefix(prefix) else { continue };
        let (version, build) = match rest.rsplit_once('-') {
            Some((v, b)) if !b.is_empty() && b.chars().all(|c| c.is_ascii_digit()) => (v, Some(b.to_string())),
            _ => (rest, None),
        };
        return looks_like_version(version).then(|| (core, version.to_string(), build));
    }
    None
}

fn zip_text(z: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    let mut f = z.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

fn manifest_value(manifest: &str, key: &str) -> Option<String> {
    manifest.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

fn props_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

/// Ядро и версия по содержимому jar: paperclip (`META-INF/versions.list`), ванильный bundler
/// (`version.json`), лаунчер Fabric (`install.properties`), Velocity (`Implementation-Version`).
pub fn inspect_jar(path: &Path) -> Option<(Core, String, Option<String>)> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let manifest = zip_text(&mut z, "META-INF/MANIFEST.MF").unwrap_or_default();
    let main = manifest_value(&manifest, "Main-Class").unwrap_or_default();

    if manifest_value(&manifest, "Implementation-Title").as_deref() == Some("Velocity") {
        // «4.3.1» или «3.4.0-SNAPSHOT (git-1a2b3c4d-b500)».
        let iv = manifest_value(&manifest, "Implementation-Version")?;
        let version = iv.split_whitespace().next()?.to_string();
        let build = iv.rsplit_once("-b").map(|(_, b)| b.trim_end_matches(')').to_string());
        let build = build.filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_digit()));
        return Some((Core::Velocity, version, build));
    }
    if let Some(props) = zip_text(&mut z, "install.properties") {
        let game = props_value(&props, "game-version")?;
        let loader = props_value(&props, "fabric-loader-version")?;
        let launcher = manifest_value(&manifest, "Implementation-Version").unwrap_or_else(|| "?".into());
        return Some((Core::Fabric, game, Some(format!("{loader}/{launcher}"))));
    }
    if main.contains("paperclip") {
        // «<sha256>\t26.3\t26.3/purpur-26.3.jar»
        let list = zip_text(&mut z, "META-INF/versions.list")?;
        let line = list.lines().next()?;
        let mut cols = line.split('\t');
        let version = cols.nth(1)?.to_string();
        let file = cols.next()?.rsplit('/').next()?.to_ascii_lowercase();
        let core = if file.starts_with("purpur") {
            Core::Purpur
        } else if file.starts_with("paper") {
            Core::Paper
        } else {
            return None; // Folia и другие форки — не умеем обновлять.
        };
        return Some((core, version, None));
    }
    let json = zip_text(&mut z, "version.json")?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let id = v.get("id")?.as_str()?.to_string();
    Some((Core::Vanilla, id, None))
}

fn identify(jar: &Path) -> Option<(Core, String, Option<String>)> {
    let name = jar.file_name()?.to_string_lossy();
    parse_jar_name(&name).or_else(|| inspect_jar(jar))
}

// ───────────────────────────── скрипт запуска ─────────────────────────────

/// Слово строки скрипта: байтовые границы в строке (с кавычками) и значение (без них).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tok {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) value: String,
}

/// Слова строки с позициями (кавычки `"…"` и в sh `'…'`, экранирование `\` в sh).
/// Общий разбор для `core_update` и `jvm`.
pub(crate) fn tokens(line: &str, bat: bool) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut cur: Option<(usize, String)> = None;
    let mut quote: Option<char> = None;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' && !bat {
                if let Some((_, n)) = chars.next()
                    && let Some((_, s)) = cur.as_mut()
                {
                    s.push(n);
                }
            } else if let Some((_, s)) = cur.as_mut() {
                s.push(c);
            }
            continue;
        }
        match c {
            ' ' | '\t' => {
                if let Some((start, value)) = cur.take() {
                    out.push(Tok { start, end: i, value });
                }
            }
            '"' => {
                cur.get_or_insert((i, String::new()));
                quote = Some('"');
            }
            '\'' if !bat => {
                cur.get_or_insert((i, String::new()));
                quote = Some('\'');
            }
            '\\' if !bat => {
                let start = cur.get_or_insert((i, String::new()));
                if let Some((_, n)) = chars.next() {
                    start.1.push(n);
                }
            }
            c => cur.get_or_insert((i, String::new())).1.push(c),
        }
    }
    if let Some((start, value)) = cur.take() {
        out.push(Tok { start, end: line.len(), value });
    }
    out
}

pub(crate) fn is_java(word: &str) -> bool {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word).to_ascii_lowercase();
    matches!(base.as_str(), "java" | "java.exe" | "javaw" | "javaw.exe")
}

/// Где в скрипте jar и java: номер строки (с 0) и слова.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptRef {
    pub(crate) line: usize,
    pub(crate) jar: Tok,
    pub(crate) java: Option<Tok>,
    /// Все слова строки запуска.
    pub(crate) toks: Vec<Tok>,
    /// Индекс `-jar` в [`ScriptRef::toks`] (jar — следующий).
    pub(crate) jar_flag: usize,
    /// Индекс java в [`ScriptRef::toks`].
    pub(crate) java_idx: Option<usize>,
}

pub(crate) fn is_comment(line: &str, bat: bool) -> bool {
    let t = line.trim_start().to_ascii_lowercase();
    t.starts_with('#') || (bat && (t.starts_with("rem ") || t == "rem" || t.starts_with("::")))
}

/// Первая строка `… java … -jar X.jar …` (комментарии пропускаются).
pub(crate) fn find_in_script(text: &str, bat: bool) -> Option<ScriptRef> {
    for (n, line) in text.lines().enumerate() {
        if is_comment(line, bat) {
            continue;
        }
        let toks = tokens(line, bat);
        let Some(j) = toks.iter().position(|t| t.value == "-jar") else { continue };
        let Some(jar) = toks.get(j + 1).filter(|t| t.value.to_ascii_lowercase().ends_with(".jar")) else { continue };
        let java_idx = toks[..j].iter().position(|t| is_java(&t.value));
        let (jar, java) = (jar.clone(), java_idx.map(|i| toks[i].clone()));
        return Some(ScriptRef { line: n, jar, java, toks, jar_flag: j, java_idx });
    }
    None
}

/// Текст с заменённой строкой `line` (с 0): остальные строки и окончание `\r\n` / `\n` этой строки — как были.
/// `edit` получает строку без окончания.
pub(crate) fn replace_line(text: &str, line: usize, edit: impl FnOnce(&mut String)) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut edit = Some(edit);
    for (n, chunk) in text.split_inclusive('\n').enumerate() {
        let Some(f) = edit.take_if(|_| n == line) else {
            out.push_str(chunk);
            continue;
        };
        let body = chunk.strip_suffix('\n').unwrap_or(chunk);
        let body = body.strip_suffix('\r').unwrap_or(body);
        let mut l = body.to_string();
        f(&mut l);
        out.push_str(&l);
        out.push_str(&chunk[body.len()..]);
    }
    out
}

/// Новый текст скрипта: jar → `new_jar`, java → `new_java` (если задана). Меняются только эти слова,
/// остальное (включая окончания строк `\r\n`) остаётся как было.
fn rewrite_script(text: &str, bat: bool, new_jar: &str, new_java: Option<&str>) -> Option<String> {
    let r = find_in_script(text, bat)?;
    let quote = |s: &str| if bat { create::bat_quote(&s.replace('/', "\\")) } else { create::sh_quote(s) };
    Some(replace_line(text, r.line, |line| {
        // Справа налево, чтобы позиции не сдвигались: jar стоит после java.
        line.replace_range(r.jar.start..r.jar.end, &quote(new_jar));
        if let (Some(java), Some(new)) = (&r.java, new_java) {
            line.replace_range(java.start..java.end, &quote(new));
        }
    }))
}

fn script_names() -> impl Iterator<Item = &'static str> {
    let (first, second) = if cfg!(windows) { (SCRIPTS_WIN, SCRIPTS_UNIX) } else { (SCRIPTS_UNIX, SCRIPTS_WIN) };
    first.into_iter().chain(second)
}

pub(crate) fn is_bat(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"))
}

/// Jar по умолчанию, если скрипта нет: сначала опознаваемые, по имени.
fn pick_jar(dir: &Path) -> Option<PathBuf> {
    let mut jars: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jar")))
        .collect();
    jars.sort();
    jars.into_iter().find(|p| identify(p).is_some())
}

/// Скрипт из команды запуска (`./start.sh`, `sh run.sh`, `cmd /C start.bat`), если она его запускает.
pub(crate) fn command_script(command: &[String]) -> Option<&str> {
    command.iter().map(|a| a.trim_start_matches("./")).find(|a| {
        let l = a.to_ascii_lowercase();
        [".sh", ".bat", ".cmd", ".command"].iter().any(|e| l.ends_with(e))
    })
}

/// Существующие скрипты запуска по порядку: из команды, затем стандартные имена (сначала родные для системы).
pub(crate) fn script_candidates(dir: &Path, command: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in command_script(command).map(|s| dir.join(s)).into_iter().chain(script_names().map(|s| dir.join(s))) {
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Что установлено в `dir`: jar из скрипта запуска (start.sh / run.sh / start.bat / run.bat…), иначе
/// единственный опознаваемый jar в папке.
pub fn detect(dir: &Path) -> Option<Installed> {
    detect_with(dir, &[])
}

/// Как [`detect`], но с командой запуска из настроек сервера: `java … -jar X.jar` или скрипт.
pub fn detect_with(dir: &Path, command: &[String]) -> Option<Installed> {
    // 1. Команда сама запускает java -jar.
    if let Some(j) = command.iter().position(|a| a == "-jar")
        && let Some(jar) = command.get(j + 1)
    {
        let path = dir.join(jar);
        let (core, mc_version, build) = identify(&path)?;
        let java = command[..j].iter().find(|a| is_java(a)).cloned();
        return Some(Installed { core, mc_version, build, jar: path, script: None, java });
    }
    // 2. Скрипт: из команды или стандартные имена.
    for script in script_candidates(dir, command) {
        let Ok(text) = std::fs::read_to_string(&script) else { continue };
        let Some(r) = find_in_script(&text, is_bat(&script)) else { continue };
        let path = dir.join(&r.jar.value);
        if let Some((core, mc_version, build)) = identify(&path) {
            return Some(Installed {
                core,
                mc_version,
                build,
                jar: path,
                script: Some(script),
                java: r.java.map(|t| t.value),
            });
        }
    }
    // 3. Просто jar в папке.
    let path = pick_jar(dir)?;
    let (core, mc_version, build) = identify(&path)?;
    Some(Installed { core, mc_version, build, jar: path, script: None, java: None })
}

// ───────────────────────────── проверка ─────────────────────────────

/// Новые сборки той же версии и новые версии. Сеть, блокирует — вызывать из `spawn_blocking`.
pub fn check(inst: &Installed) -> Result<UpdateCheck> {
    let latest = if inst.core == Core::Vanilla {
        Ok(None)
    } else {
        create::server_file(inst.core, &inst.mc_version, None).map(|f| f.build)
    };
    let versions = create::list_versions(inst.core);
    if let (Err(e), Err(_)) = (&latest, &versions) {
        return Err(anyhow::anyhow!("{e:#}"));
    }
    let latest_build = latest.unwrap_or(None);
    let newer_versions: Vec<String> = versions
        .unwrap_or_default()
        .into_iter()
        .filter(|v| cmp_version(v, &inst.mc_version) == Ordering::Greater)
        .collect();
    Ok(UpdateCheck {
        build_update: build_is_newer(inst.build.as_deref(), latest_build.as_deref()),
        latest_build,
        newer_versions,
    })
}

// ───────────────────────────── замена ─────────────────────────────

/// Основная версия Java из скрипта: `java` — из PATH, относительный путь — от папки сервера.
fn java_major_of(dir: &Path, java: &str) -> Option<u32> {
    if java.eq_ignore_ascii_case("java") || java.eq_ignore_ascii_case("java.exe") {
        return create::system_java_major();
    }
    let p = Path::new(java);
    let path = if p.is_absolute() { p.to_path_buf() } else { dir.join(java.trim_start_matches("./")) };
    create::java_major(&path)
}

/// Java, уже лежащая в папке сервера (`jre/`, `jdk25/`…) и подходящая по версии: путь относительно `dir`.
fn bundled_java(dir: &Path, required: u32) -> Option<String> {
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy().to_ascii_lowercase();
                    n.starts_with("jre") || n.starts_with("jdk") || n.contains("java")
                })
        })
        .collect();
    subdirs.sort();
    subdirs.into_iter().rev().find_map(|d| {
        let bin = create::find_java_binary(&d)?;
        (create::java_major(&bin)? >= required)
            .then(|| bin.strip_prefix(dir).unwrap_or(&bin).to_string_lossy().replace('\\', "/"))
    })
}

pub(crate) fn free_name(dir: &Path, base: &str) -> PathBuf {
    let p = dir.join(base);
    if !p.exists() {
        return p;
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    (1..).map(|i| dir.join(format!("{base}.{stamp}-{i}"))).find(|p| !p.exists()).expect("free name")
}

/// Записывает скрипт через временный файл и переименование, сохраняя права (исполняемость).
pub(crate) fn write_script(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_file_name(format!(".{}.mctui-tmp", path.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::write(&tmp, text)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Ставит скачанный `part` на место jar: старый — в `.mctui-old/`, скрипт — с новым именем jar и Java.
/// При ошибке всё возвращается как было. Возвращает (куда убран старый jar, изменённый скрипт).
fn swap_in(
    dir: &Path,
    inst: &Installed,
    part: &Path,
    new_name: &str,
    new_java: Option<&str>,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let old_name = inst.jar_name();
    let old_dir = dir.join(crate::plugins::OLD_DIR);
    std::fs::create_dir_all(&old_dir)?;
    let backup = free_name(&old_dir, &old_name);
    std::fs::rename(&inst.jar, &backup).with_context(|| {
        tr!(
            format!("не удалось убрать {old_name} в {}", crate::plugins::OLD_DIR),
            format!("failed to move {old_name} to {}", crate::plugins::OLD_DIR)
        )
    })?;
    let new_path = dir.join(new_name);
    if let Err(e) = std::fs::rename(part, &new_path) {
        let _ = std::fs::rename(&backup, &inst.jar);
        return Err(e)
            .with_context(|| tr!(format!("не удалось записать {new_name}"), format!("cannot write {new_name}")));
    }
    let mut changed_script = None;
    if let Some(script) = &inst.script
        && (new_name != old_name || new_java.is_some())
    {
        let res = std::fs::read_to_string(script)
            .map_err(anyhow::Error::from)
            .and_then(|text| {
                rewrite_script(&text, is_bat(script), new_name, new_java).with_context(|| {
                    tr!(
                        format!("в {} не найден запуск -jar", script.display()),
                        format!("no -jar launch line in {}", script.display())
                    )
                })
            })
            .and_then(|text| write_script(script, &text));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&new_path);
            let _ = std::fs::rename(&backup, &inst.jar);
            return Err(e);
        }
        changed_script = Some(script.clone());
    }
    Ok((backup, changed_script))
}

/// Команда из настроек (`java … -jar old.jar …`) с новым jar и, если задана, новой Java.
/// Для команд, запускающих скрипт, возвращает `None` — скрипт уже изменён [`apply`].
pub fn rewrite_command(command: &[String], applied: &Applied) -> Option<Vec<String>> {
    let j = command.iter().position(|a| a == "-jar")?;
    command.get(j + 1)?;
    let mut out = command.to_vec();
    out[j + 1] = applied.jar.clone();
    if let Some(java) = &applied.java
        && let Some(i) = out[..j].iter().position(|a| is_java(a))
    {
        out[i] = java.clone();
    }
    Some(out)
}

/// Скачивает ядро `target_version` (та же версия — новая сборка) и ставит его вместо установленного.
/// Сервер должен быть остановлен. Переход на более старую версию Minecraft запрещён (миры не откроются).
/// Блокирует — вызывать из `spawn_blocking`.
pub fn apply(
    dir: &Path,
    inst: &Installed,
    target_version: &str,
    progress: &mut dyn FnMut(Progress),
) -> Result<Applied> {
    let target = target_version.trim();
    if target.is_empty() {
        bail!(tr!("не выбрана версия", "no version selected"));
    }
    if !inst.jar.is_file() {
        bail!(tr!(format!("нет файла {}", inst.jar.display()), format!("file {} not found", inst.jar.display())));
    }
    let major = is_major_upgrade(inst, target);
    if major && cmp_version(target, &inst.mc_version) == Ordering::Less {
        bail!(tr!(
            format!(
                "нельзя перейти с {} на более старую {target}: миры новой версии старая не откроет",
                inst.mc_version
            ),
            format!("cannot go from {} down to {target}: an older version can't open newer worlds", inst.mc_version)
        ));
    }
    let core = inst.core;

    progress(Progress::Step(tr!(
        format!("Узнаю, какая Java нужна для {} {target}…", core.label()),
        format!("Checking which Java {} {target} needs…", core.label())
    )));
    let (required, mojang) = create::required_java(core, target)?;
    progress(Progress::Step(tr!(
        format!("Ищу последнюю сборку {} {target}…", core.label()),
        format!("Looking up the latest {} {target} build…", core.label())
    )));
    let remote = create::server_file(core, target, mojang.as_ref())?;
    if remote.name.contains(['/', '\\']) || remote.name.starts_with('.') || !remote.name.ends_with(".jar") {
        bail!(tr!(
            format!("подозрительное имя файла: {}", remote.name),
            format!("suspicious file name: {}", remote.name)
        ));
    }
    let old_name = inst.jar_name();
    // Неговорящее имя (`server.jar`) сохраняем — его могут ждать хостинг или чужие скрипты.
    let new_name = if parse_jar_name(&old_name).is_none() { old_name.clone() } else { remote.name.clone() };

    let mut guard = create::Cleanup { dir: dir.to_path_buf(), created_dir: false, paths: Vec::new(), armed: true };
    let part = guard.track(dir.join(format!("{new_name}.mctui-part")));
    create::download(&remote.url, &part, &remote.label, remote.size, progress)?;
    if let Some(sum) = &remote.checksum {
        progress(Progress::Step(tr!("Проверяю контрольную сумму…", "Verifying checksum…").to_string()));
        create::verify(&part, sum)?;
    }

    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    // Java
    let mut new_java = None;
    match inst.java.as_deref() {
        None => warnings.push(tr!(
            format!("Не нашёл, какой Java запускается сервер — нужна Java {required} или новее"),
            format!("Could not tell which Java runs the server — Java {required} or newer is needed")
        )),
        Some(j) if j.contains(['$', '%']) => warnings.push(tr!(
            format!("Java в скрипте задана переменной ({j}) — убедитесь, что это Java {required}+"),
            format!("Java in the script is a variable ({j}) — make sure it is Java {required}+")
        )),
        Some(j) => match java_major_of(dir, j) {
            Some(m) if m >= required => {}
            found => {
                let why = match found {
                    Some(m) => tr!(format!("Java {m} слишком старая"), format!("Java {m} is too old")),
                    None => tr!(format!("{j} не запускается"), format!("{j} does not run")),
                };
                progress(Progress::Step(tr!(
                    format!("{why} для {} {target} (нужна {required}+)…", core.label()),
                    format!("{why} for {} {target} ({required}+ needed)…", core.label())
                )));
                let cmd = match bundled_java(dir, required) {
                    Some(rel) => rel,
                    None => {
                        let name = if dir.join("jre").exists() { format!("jre-{required}") } else { "jre".into() };
                        let name = free_name(dir, &name).file_name().unwrap_or_default().to_string_lossy().into_owned();
                        create::provide_java(dir, &name, required, &mut guard, progress)?.0
                    }
                };
                // Стиль как в скрипте: `./jdk25/…` остаётся с `./`.
                let cmd = if j.starts_with("./") && cmd != "java" && !Path::new(&cmd).is_absolute() {
                    format!("./{cmd}")
                } else {
                    cmd
                };
                notes.push(tr!(format!("Java: {j} → {cmd}"), format!("Java: {j} → {cmd}")));
                new_java = Some(cmd);
            }
        },
    }

    progress(Progress::Step(tr!("Заменяю jar…", "Replacing the jar…").to_string()));
    let (old_jar, script) = swap_in(dir, inst, &part, &new_name, new_java.as_deref())?;
    guard.armed = false;

    let from = inst.label();
    notes.insert(
        0,
        tr!(
            format!("{from} → {} {target} ({new_name}); старый jar — в {}", core.label(), crate::plugins::OLD_DIR),
            format!("{from} → {} {target} ({new_name}); the old jar is in {}", core.label(), crate::plugins::OLD_DIR)
        ),
    );
    if let Some(s) = &script {
        let name = s.file_name().unwrap_or_default().to_string_lossy();
        notes.push(tr!(format!("Обновлён {name}"), format!("Updated {name}")));
    } else if new_name != old_name || new_java.is_some() {
        warnings.push(tr!(
            "Сервер запускается не скриптом — обновите команду запуска в настройках".to_string(),
            "The server is not started by a script — update the start command in the settings".to_string()
        ));
    }
    if major {
        warnings.push(tr!(
            format!(
                "Миры будут переведены на {target} при первом запуске — открыть их на {} уже не получится",
                inst.mc_version
            ),
            format!("Worlds are converted to {target} on the next start — {} won't open them again", inst.mc_version)
        ));
        match core {
            Core::Paper | Core::Purpur => warnings.push(tr!(
                format!("Проверьте, что плагины поддерживают {target}"),
                format!("Check that your plugins support {target}")
            )),
            Core::Fabric => warnings.push(tr!(
                format!("Обновите моды (и Fabric API) под {target} — старые обычно не загружаются"),
                format!("Update your mods (and Fabric API) for {target} — old ones usually fail to load")
            )),
            _ => {}
        }
    } else if core.is_proxy() && create::version_key(target).first() != create::version_key(&inst.mc_version).first() {
        warnings.push(tr!(
            "Новая основная версия Velocity — проверьте, что плагины прокси её поддерживают".to_string(),
            "New major Velocity version — check that the proxy plugins support it".to_string()
        ));
    }
    progress(Progress::Step(tr!("Готово", "Done").to_string()));
    Ok(Applied {
        version: target.to_string(),
        build: remote.build,
        jar: new_name,
        old_jar,
        script,
        java: new_java,
        notes,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("mctui-coreupd-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_jar(path: &Path, entries: &[(&str, &str)]) {
        let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            z.start_file(*name, o).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn jar_names() {
        let p = |n: &str| parse_jar_name(n);
        assert_eq!(p("paper-1.21.4-232.jar"), Some((Core::Paper, "1.21.4".into(), Some("232".into()))));
        assert_eq!(p("purpur-26.3-2658.jar"), Some((Core::Purpur, "26.3".into(), Some("2658".into()))));
        assert_eq!(
            p("velocity-3.4.0-SNAPSHOT-500.jar"),
            Some((Core::Velocity, "3.4.0-SNAPSHOT".into(), Some("500".into())))
        );
        assert_eq!(p("velocity-4.3.1-45.jar"), Some((Core::Velocity, "4.3.1".into(), Some("45".into()))));
        assert_eq!(
            p("fabric-server-mc.1.21.11-loader.0.20.1-launcher.1.1.2.jar"),
            Some((Core::Fabric, "1.21.11".into(), Some("0.20.1/1.1.2".into())))
        );
        assert_eq!(p("minecraft_server.1.21.11.jar"), Some((Core::Vanilla, "1.21.11".into(), None)));
        assert_eq!(p("paper-1.20.1.jar"), Some((Core::Paper, "1.20.1".into(), None)));
        assert_eq!(p("Paper-1.21.4-232.JAR"), None, "prefix is case-sensitive like the API names");
        assert_eq!(p("server.jar"), None);
        assert_eq!(p("paper.jar"), None);
        assert_eq!(p("paper-latest.jar"), None);
        assert_eq!(p("purpur-26.3-2658.jar.disabled"), None);
    }

    #[test]
    fn jar_contents() {
        let t = TempDir::new("inspect");
        let j = t.0.join("server.jar");
        make_jar(
            &j,
            &[
                ("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\r\nMain-Class: io.papermc.paperclip.Main\r\n"),
                ("META-INF/versions.list", "66785de9\t26.3\t26.3/purpur-26.3.jar\n"),
                ("version.json", r#"{"id":"26.3"}"#),
            ],
        );
        assert_eq!(inspect_jar(&j), Some((Core::Purpur, "26.3".into(), None)));
        make_jar(
            &j,
            &[
                ("META-INF/MANIFEST.MF", "Main-Class: io.papermc.paperclip.Main\n"),
                ("META-INF/versions.list", "ab\t1.21.4\t1.21.4/paper-1.21.4.jar\n"),
            ],
        );
        assert_eq!(inspect_jar(&j), Some((Core::Paper, "1.21.4".into(), None)));
        make_jar(
            &j,
            &[
                ("META-INF/MANIFEST.MF", "Main-Class: net.minecraft.bundler.Main\n"),
                ("META-INF/versions.list", "ab\t1.21.11\t1.21.11/server-1.21.11.jar\n"),
                ("version.json", r#"{"id": "1.21.11", "name": "1.21.11", "java_version": 21}"#),
            ],
        );
        assert_eq!(inspect_jar(&j), Some((Core::Vanilla, "1.21.11".into(), None)));
        make_jar(
            &j,
            &[
                (
                    "META-INF/MANIFEST.MF",
                    "Implementation-Version: 1.1.2\nMain-Class: net.fabricmc.installer.ServerLauncher\n",
                ),
                ("install.properties", "fabric-loader-version=0.20.1\ngame-version=1.21.11"),
            ],
        );
        assert_eq!(inspect_jar(&j), Some((Core::Fabric, "1.21.11".into(), Some("0.20.1/1.1.2".into()))));
        make_jar(
            &j,
            &[(
                "META-INF/MANIFEST.MF",
                "Implementation-Version: 3.4.0-SNAPSHOT (git-1a2b3c4d-b500)\nImplementation-Title: Velocity\n",
            )],
        );
        assert_eq!(inspect_jar(&j), Some((Core::Velocity, "3.4.0-SNAPSHOT".into(), Some("500".into()))));
        make_jar(&j, &[("META-INF/MANIFEST.MF", "Implementation-Version: 4.3.1\nImplementation-Title: Velocity\n")]);
        assert_eq!(inspect_jar(&j), Some((Core::Velocity, "4.3.1".into(), None)));
        make_jar(&j, &[("META-INF/MANIFEST.MF", "Main-Class: x.Y\n")]);
        assert_eq!(inspect_jar(&j), None);
        std::fs::write(&j, b"not a zip").unwrap();
        assert_eq!(inspect_jar(&j), None);
    }

    #[test]
    fn real_jar_if_present() {
        // Живой сервер пользователя (только чтение).
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../purpur-26.3-2658.jar");
        if p.is_file() {
            assert_eq!(inspect_jar(&p), Some((Core::Purpur, "26.3".into(), None)));
        }
    }

    #[test]
    fn versions_compare() {
        assert_eq!(cmp_version("26.3", "1.21.11"), Ordering::Greater);
        assert_eq!(cmp_version("1.21.11", "1.21.9"), Ordering::Greater);
        assert_eq!(cmp_version("1.21", "1.21.0"), Ordering::Less);
        assert_eq!(cmp_version("3.4.0", "3.4.0-SNAPSHOT"), Ordering::Greater);
        assert_eq!(cmp_version("3.4.0-SNAPSHOT", "3.4.0-SNAPSHOT"), Ordering::Equal);
        assert_eq!(cmp_version("4.3.1", "3.4.0-SNAPSHOT"), Ordering::Greater);
        assert!(build_is_newer(Some("232"), Some("249")));
        assert!(!build_is_newer(Some("249"), Some("249")));
        assert!(!build_is_newer(Some("250"), Some("249")));
        assert!(build_is_newer(None, Some("249")));
        assert!(!build_is_newer(Some("1"), None));
        assert!(build_is_newer(Some("0.20.1/1.1.2"), Some("0.20.2/1.1.2")));
        assert!(build_is_newer(Some("0.20.1/1.1.1"), Some("0.20.1/1.1.2")));
        assert!(!build_is_newer(Some("0.20.1/1.1.2"), Some("0.20.1/1.1.2")));
        assert!(!build_is_newer(Some("0.21.0/1.1.2"), Some("0.20.1/1.1.2")), "beta newer than stable");
        let inst = |core, v: &str| Installed {
            core,
            mc_version: v.into(),
            build: None,
            jar: PathBuf::from("x.jar"),
            script: None,
            java: None,
        };
        assert!(is_major_upgrade(&inst(Core::Paper, "1.21.4"), "1.21.11"));
        assert!(!is_major_upgrade(&inst(Core::Paper, "1.21.4"), " 1.21.4 "));
        assert!(!is_major_upgrade(&inst(Core::Velocity, "3.4.0-SNAPSHOT"), "4.3.1"));
        assert_eq!(inst(Core::Vanilla, "1.21.11").label(), "Vanilla 1.21.11");
        let mut f = inst(Core::Fabric, "1.21.11");
        f.build = Some("0.20.1/1.1.2".into());
        assert_eq!(f.label(), "Fabric 1.21.11 (loader 0.20.1)");
        f.core = Core::Purpur;
        f.build = Some("2658".into());
        assert_eq!(f.label(), "Purpur 1.21.11 #2658");
    }

    const USER_SH: &str = "#!/bin/bash\ncd \"$(dirname \"$0\")\"\nexec ./jdk25/Contents/Home/bin/java -Xms2G -Xmx4G -jar purpur-26.3-2658.jar --nogui\n";

    #[test]
    fn script_parse_and_rewrite() {
        let r = find_in_script(USER_SH, false).unwrap();
        assert_eq!(r.line, 2);
        assert_eq!(r.jar.value, "purpur-26.3-2658.jar");
        assert_eq!(r.java.as_ref().unwrap().value, "./jdk25/Contents/Home/bin/java");

        let out = rewrite_script(USER_SH, false, "purpur-26.3-2660.jar", None).unwrap();
        assert_eq!(out, USER_SH.replace("2658", "2660"));
        let out = rewrite_script(USER_SH, false, "purpur-26.4-1.jar", Some("./jre/jdk-26/bin/java")).unwrap();
        assert!(out.ends_with("exec ./jre/jdk-26/bin/java -Xms2G -Xmx4G -jar purpur-26.4-1.jar --nogui\n"));

        // Скрипт mctui (create.rs), кавычки, «$@».
        let sh = create::start_script("jre/a b/bin/java", 2048, "paper-1.21.4-232.jar", "--nogui", false);
        let r = find_in_script(&sh, false).unwrap();
        assert_eq!(r.java.unwrap().value, "jre/a b/bin/java");
        let out = rewrite_script(&sh, false, "paper-1.21.11-50.jar", None).unwrap();
        assert!(out.contains("exec 'jre/a b/bin/java' -Xms1024M -Xmx2048M -jar paper-1.21.11-50.jar --nogui \"$@\"\n"));

        // .bat с CRLF и путём в кавычках.
        let bat = "@echo off\r\nrem java -jar old.jar\r\ncd /d \"%~dp0\"\r\n\"C:\\Program Files\\Java\\bin\\java.exe\" -Xmx2G -jar \"paper-1.21.4-232.jar\" nogui %*\r\n";
        let r = find_in_script(bat, true).unwrap();
        assert_eq!(r.line, 3);
        assert_eq!(r.java.as_ref().unwrap().value, "C:\\Program Files\\Java\\bin\\java.exe");
        let out = rewrite_script(bat, true, "paper-1.21.11-50.jar", Some("jre/jdk-21/bin/java.exe")).unwrap();
        assert_eq!(
            out,
            "@echo off\r\nrem java -jar old.jar\r\ncd /d \"%~dp0\"\r\njre\\jdk-21\\bin\\java.exe -Xmx2G -jar paper-1.21.11-50.jar nogui %*\r\n"
        );

        // Без java -jar.
        assert!(find_in_script("#!/bin/sh\nexec ./run-forge.sh\n", false).is_none());
        // Java из переменной.
        let r = find_in_script("\"$JAVA_HOME/bin/java\" -jar server.jar\n", false).unwrap();
        assert_eq!(r.java.unwrap().value, "$JAVA_HOME/bin/java");
        assert_eq!(
            tokens(r"a\ b 'c d' e", false).iter().map(|t| t.value.as_str()).collect::<Vec<_>>(),
            ["a b", "c d", "e"]
        );
    }

    #[test]
    fn detect_layouts() {
        let t = TempDir::new("detect");
        let d = &t.0;
        // Как у живого сервера: start.sh + purpur jar.
        std::fs::write(d.join("start.sh"), USER_SH).unwrap();
        std::fs::write(d.join("purpur-26.3-2658.jar"), b"").unwrap();
        std::fs::write(d.join("other.jar"), b"").unwrap();
        let i = detect(d).unwrap();
        assert_eq!((i.core, i.mc_version.as_str(), i.build.as_deref()), (Core::Purpur, "26.3", Some("2658")));
        assert_eq!(i.jar, d.join("purpur-26.3-2658.jar"));
        assert_eq!(i.script, Some(d.join("start.sh")));
        assert_eq!(i.java.as_deref(), Some("./jdk25/Contents/Home/bin/java"));
        assert_eq!(i.label(), "Purpur 26.3 #2658");

        // Команда java -jar из настроек.
        let cmd: Vec<String> = ["java", "-Xmx2G", "-jar", "purpur-26.3-2658.jar", "--nogui"].map(String::from).to_vec();
        let i = detect_with(d, &cmd).unwrap();
        assert_eq!(i.script, None);
        assert_eq!(i.java.as_deref(), Some("java"));

        // Нет скрипта — vanilla server.jar по version.json.
        std::fs::remove_file(d.join("start.sh")).unwrap();
        std::fs::remove_file(d.join("purpur-26.3-2658.jar")).unwrap();
        make_jar(&d.join("server.jar"), &[("version.json", r#"{"id":"1.21.10"}"#)]);
        let i = detect(d).unwrap();
        assert_eq!((i.core, i.mc_version.as_str(), i.build.as_deref()), (Core::Vanilla, "1.21.10", None));
        assert_eq!(i.jar_name(), "server.jar");

        // run.bat с Velocity.
        std::fs::remove_file(d.join("server.jar")).unwrap();
        std::fs::write(d.join("run.bat"), "java -Xms512M -Xmx512M -jar velocity-3.4.0-SNAPSHOT-500.jar\r\n").unwrap();
        std::fs::write(d.join("velocity-3.4.0-SNAPSHOT-500.jar"), b"").unwrap();
        let i = detect(d).unwrap();
        assert_eq!((i.core, i.mc_version.as_str()), (Core::Velocity, "3.4.0-SNAPSHOT"));
        assert_eq!(i.script, Some(d.join("run.bat")));

        let empty = TempDir::new("detect-empty");
        assert_eq!(detect(&empty.0), None);
    }

    #[test]
    fn swap_moves_old_jar_and_rewrites_script() {
        let t = TempDir::new("swap");
        let d = &t.0;
        std::fs::write(d.join("start.sh"), USER_SH).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d.join("start.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(d.join("purpur-26.3-2658.jar"), b"old").unwrap();
        let inst = detect(d).unwrap();
        std::fs::write(d.join("new.part"), b"new").unwrap();
        let (backup, script) = swap_in(d, &inst, &d.join("new.part"), "purpur-26.3-2660.jar", None).unwrap();
        assert_eq!(backup, d.join(".mctui-old/purpur-26.3-2658.jar"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"old");
        assert_eq!(std::fs::read(d.join("purpur-26.3-2660.jar")).unwrap(), b"new");
        assert!(!d.join("purpur-26.3-2658.jar").exists() && !d.join("new.part").exists());
        assert_eq!(script, Some(d.join("start.sh")));
        assert_eq!(std::fs::read_to_string(d.join("start.sh")).unwrap(), USER_SH.replace("2658", "2660"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join("start.sh")).unwrap().permissions().mode() & 0o777, 0o755);
        }

        // Тот же jar ещё раз: в .mctui-old имя занято — берётся свободное.
        let inst = detect(d).unwrap();
        std::fs::write(d.join("new.part"), b"newer").unwrap();
        std::fs::write(d.join(".mctui-old/purpur-26.3-2660.jar"), b"x").unwrap();
        let (backup, _) = swap_in(d, &inst, &d.join("new.part"), "purpur-26.3-2660.jar", None).unwrap();
        assert_ne!(backup, d.join(".mctui-old/purpur-26.3-2660.jar"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"new");

        // Скрипт без -jar — откат: старый jar на месте, новый не оставлен.
        std::fs::write(d.join("start.sh"), "#!/bin/sh\necho hi\n").unwrap();
        let mut inst = detect_with(d, &["java".into(), "-jar".into(), "purpur-26.3-2660.jar".into()]).unwrap();
        inst.script = Some(d.join("start.sh"));
        std::fs::write(d.join("new.part"), b"z").unwrap();
        assert!(swap_in(d, &inst, &d.join("new.part"), "purpur-26.4-1.jar", None).is_err());
        assert_eq!(std::fs::read(d.join("purpur-26.3-2660.jar")).unwrap(), b"newer");
        assert!(!d.join("purpur-26.4-1.jar").exists());
    }

    #[test]
    fn command_rewrite() {
        let applied = Applied {
            version: "1.21.11".into(),
            build: Some("50".into()),
            jar: "paper-1.21.11-50.jar".into(),
            old_jar: PathBuf::new(),
            script: None,
            java: Some("jre/bin/java".into()),
            notes: vec![],
            warnings: vec![],
        };
        let cmd: Vec<String> = ["java", "-Xmx2G", "-jar", "paper-1.21.4-232.jar", "--nogui"].map(String::from).to_vec();
        assert_eq!(
            rewrite_command(&cmd, &applied).unwrap(),
            ["jre/bin/java", "-Xmx2G", "-jar", "paper-1.21.11-50.jar", "--nogui"]
        );
        assert_eq!(rewrite_command(&["./start.sh".into()], &applied), None);
    }

    #[test]
    fn apply_refuses_downgrade_and_missing_jar() {
        let t = TempDir::new("refuse");
        let inst = Installed {
            core: Core::Paper,
            mc_version: "1.21.11".into(),
            build: Some("1".into()),
            jar: t.0.join("paper-1.21.11-1.jar"),
            script: None,
            java: None,
        };
        assert!(apply(&t.0, &inst, "1.21.11", &mut |_| {}).is_err(), "no jar file");
        std::fs::write(&inst.jar, b"").unwrap();
        assert!(apply(&t.0, &inst, "1.21.4", &mut |_| {}).is_err(), "downgrade");
        assert!(apply(&t.0, &inst, "  ", &mut |_| {}).is_err());
    }

    // ───────── сетевые (cargo test -- --ignored core_update) ─────────

    fn fake(core: Core, v: &str, build: Option<&str>) -> Installed {
        Installed {
            core,
            mc_version: v.into(),
            build: build.map(String::from),
            jar: PathBuf::from("x.jar"),
            script: None,
            java: None,
        }
    }

    #[test]
    #[ignore = "network"]
    fn core_update_net_check() {
        for inst in [
            fake(Core::Paper, "1.21.4", Some("232")),
            fake(Core::Purpur, "1.21.11", Some("1")),
            fake(Core::Fabric, "1.21.11", Some("0.20.1/1.1.2")),
            fake(Core::Vanilla, "1.21.10", None),
            fake(Core::Velocity, "3.4.0-SNAPSHOT", Some("500")),
        ] {
            let c = check(&inst).unwrap();
            println!("{}: {c:?}", inst.label());
            assert!(!c.newer_versions.is_empty(), "{c:?}");
            if inst.core != Core::Vanilla {
                assert!(c.latest_build.is_some(), "{c:?}");
            }
        }
        let c = check(&fake(Core::Velocity, "3.4.0-SNAPSHOT", Some("500"))).unwrap();
        assert!(c.build_update && c.newer_versions.contains(&"3.4.0".to_string()));
    }

    #[test]
    #[ignore = "network, ~45 MB"]
    fn core_update_net_apply_velocity() {
        let t = TempDir::new("net-apply");
        let d = &t.0;
        std::fs::write(d.join("velocity-3.4.0-SNAPSHOT-500.jar"), b"old").unwrap();
        std::fs::write(
            d.join("start.sh"),
            "#!/bin/sh\nexec java -Xms512M -Xmx512M -jar velocity-3.4.0-SNAPSHOT-500.jar\n",
        )
        .unwrap();
        let inst = detect(d).unwrap();
        let c = check(&inst).unwrap();
        let target = c.newer_versions[0].clone();
        let mut log = |p: Progress| {
            if let Progress::Step(s) = p {
                println!("· {s}");
            }
        };
        let a = apply(d, &inst, &target, &mut log).unwrap();
        println!("{a:#?}");
        assert!(d.join(&a.jar).is_file() && a.old_jar.is_file());
        assert_eq!(inspect_jar(&d.join(&a.jar)).map(|i| i.1), Some(target.clone()));
        let script = std::fs::read_to_string(d.join("start.sh")).unwrap();
        println!("{script}");
        assert!(script.contains(&a.jar));
        let again = detect(d).unwrap();
        assert_eq!(again.mc_version, target);
        assert!(d.read_dir().unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".mctui-part")));
    }
}
