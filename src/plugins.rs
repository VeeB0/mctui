//! Плагины (`plugins/`: Paper / Purpur / Spigot / Folia) и моды (`mods/`: Fabric / Quilt / Forge / NeoForge):
//! список, включение/выключение (переименование `x.jar` ↔ `x.jar.disabled`) и обновление через Modrinth.
//!
//! Метаданные читаются прямо из jar: `paper-plugin.yml` / `plugin.yml`, `fabric.mod.json`, `quilt.mod.json`,
//! `META-INF/mods.toml` / `META-INF/neoforge.mods.toml`. Обновления ищутся по SHA-1 установленных файлов
//! (bulk-эндпоинты Modrinth), так что работают только для файлов, опубликованных на Modrinth.
//!
//! Все функции блокирующие: сетевые (`check_updates`, `apply_update`) вызывать через `spawn_blocking`.
//! Сеть — системный `curl`, как в `update.rs`. Изменения вступают в силу после перезапуска сервера.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MODRINTH_API: &str = "https://api.modrinth.com/v2";
/// Папка внутри `plugins/` / `mods/`, куда откладываются заменённые jar (сервер её не загружает).
pub const OLD_DIR: &str = ".mctui-old";
const DISABLED_SUFFIX: &str = ".disabled";

pub const PLUGIN_LOADERS: &[&str] = &["paper", "purpur", "spigot", "bukkit", "folia"];
const FABRIC_LOADERS: &[&str] = &["fabric"];
const QUILT_LOADERS: &[&str] = &["quilt", "fabric"];
const FORGE_LOADERS: &[&str] = &["forge"];
const NEOFORGE_LOADERS: &[&str] = &["neoforge"];
const UNKNOWN_MOD_LOADERS: &[&str] = &["fabric", "quilt"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    Plugin,
    Mod,
}

/// Откуда взяты метаданные — по нему выбираются загрузчики для запроса к Modrinth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaFormat {
    /// `paper-plugin.yml`
    Paper,
    /// `plugin.yml` (Bukkit / Spigot / Paper)
    Bukkit,
    /// `fabric.mod.json`
    Fabric,
    /// `quilt.mod.json`
    Quilt,
    /// `META-INF/mods.toml`
    Forge,
    /// `META-INF/neoforge.mods.toml`
    NeoForge,
    /// Метаданные не найдены или не читаются (имя — из имени файла).
    Unknown,
}

#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub path: PathBuf,
    pub file_name: String,
    pub enabled: bool,
    pub kind: PluginKind,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub authors: Vec<String>,
    /// SHA-1 файла (hex, нижний регистр); пусто, если файл не прочитался.
    pub sha1: String,
    pub format: MetaFormat,
}

impl PluginInfo {
    /// Загрузчики Modrinth, под которые искать обновления этого файла.
    pub fn loaders(&self) -> &'static [&'static str] {
        match (self.kind, self.format) {
            (_, MetaFormat::Paper | MetaFormat::Bukkit) => PLUGIN_LOADERS,
            (_, MetaFormat::Fabric) => FABRIC_LOADERS,
            (_, MetaFormat::Quilt) => QUILT_LOADERS,
            (_, MetaFormat::Forge) => FORGE_LOADERS,
            (_, MetaFormat::NeoForge) => NEOFORGE_LOADERS,
            (PluginKind::Plugin, MetaFormat::Unknown) => PLUGIN_LOADERS,
            (PluginKind::Mod, MetaFormat::Unknown) => UNKNOWN_MOD_LOADERS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub file_name: String,
    pub url: String,
    pub sha1: Option<String>,
    /// Страница версии на Modrinth.
    #[allow(dead_code)] // пригодится для ссылки «что нового»
    pub page: String,
}

// ---------------------------------------------------------------------------------------------------------------
// Список

/// Сканирует `<dir>/plugins` и `<dir>/mods`: `*.jar` (включён) и `*.jar.disabled` (выключен).
/// Сортировка по имени без учёта регистра. Если метаданные не читаются — имя из имени файла.
/// Подпапки (папки данных плагинов, `.mctui-old`) пропускаются.
pub fn scan(server_dir: &Path) -> Vec<PluginInfo> {
    let mut out = Vec::new();
    for (sub, kind) in [("plugins", PluginKind::Plugin), ("mods", PluginKind::Mod)] {
        let Ok(rd) = std::fs::read_dir(server_dir.join(sub)) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Some((stem, enabled)) = split_jar_name(&file_name) else { continue };
            let stem = stem.to_string();
            out.push(read_plugin(path, file_name, &stem, enabled, kind));
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.file_name.cmp(&b.file_name)));
    out
}

/// `X.jar` → (`X`, true), `X.jar.disabled` → (`X`, false), остальное — None. Регистр расширения не важен.
fn split_jar_name(file_name: &str) -> Option<(&str, bool)> {
    if file_name.starts_with('.') {
        return None;
    }
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".jar") {
        Some((&file_name[..file_name.len() - 4], true))
    } else if lower.ends_with(".jar.disabled") {
        Some((&file_name[..file_name.len() - 13], false))
    } else {
        None
    }
}

fn read_plugin(path: PathBuf, file_name: String, stem: &str, enabled: bool, kind: PluginKind) -> PluginInfo {
    let sha1 = sha1_file(&path).unwrap_or_default();
    let meta = read_meta(&path).unwrap_or_default();
    PluginInfo {
        name: meta.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| stem.to_string()),
        version: meta.version.filter(|v| !v.trim().is_empty() && !v.contains("${")),
        description: meta.description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
        authors: meta.authors.into_iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect(),
        format: meta.format.unwrap_or(MetaFormat::Unknown),
        path,
        file_name,
        enabled,
        kind,
        sha1,
    }
}

/// Включает/выключает: переименовывает `x.jar` ↔ `x.jar.disabled`. Возвращает новый путь.
/// Вступает в силу после перезапуска сервера.
pub fn set_enabled(p: &PluginInfo, enabled: bool) -> Result<PathBuf> {
    let name = p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.file_name.clone());
    let currently_enabled = !name.to_ascii_lowercase().ends_with(DISABLED_SUFFIX);
    if currently_enabled == enabled {
        return Ok(p.path.clone());
    }
    let new_name = if enabled {
        name[..name.len() - DISABLED_SUFFIX.len()].to_string()
    } else {
        format!("{name}{DISABLED_SUFFIX}")
    };
    let new_path = p.path.with_file_name(&new_name);
    if new_path.exists() {
        bail!(crate::tr!(format!("файл {new_name} уже существует"), format!("file {new_name} already exists")));
    }
    std::fs::rename(&p.path, &new_path)
        .with_context(|| crate::tr!(format!("не удалось переименовать {name}"), format!("failed to rename {name}")))?;
    Ok(new_path)
}

// ---------------------------------------------------------------------------------------------------------------
// Метаданные из jar

#[derive(Debug, Default)]
struct Meta {
    format: Option<MetaFormat>,
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    authors: Vec<String>,
}

fn read_meta(path: &Path) -> Option<Meta> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let mut entry = |name: &str| -> Option<String> {
        let mut f = zip.by_name(name).ok()?;
        // Защита от «zip-бомб»: метаданные — это килобайты.
        let mut buf = Vec::new();
        f.by_ref().take(1 << 20).read_to_end(&mut buf).ok()?;
        Some(String::from_utf8_lossy(&buf).into_owned())
    };
    for (file, format) in [("paper-plugin.yml", MetaFormat::Paper), ("plugin.yml", MetaFormat::Bukkit)] {
        if let Some(text) = entry(file) {
            return Some(parse_plugin_yml(&text, format));
        }
    }
    if let Some(text) = entry("fabric.mod.json")
        && let Some(m) = parse_fabric_json(&text)
    {
        return Some(m);
    }
    if let Some(text) = entry("quilt.mod.json")
        && let Some(m) = parse_quilt_json(&text)
    {
        return Some(m);
    }
    for (file, format) in
        [("META-INF/neoforge.mods.toml", MetaFormat::NeoForge), ("META-INF/mods.toml", MetaFormat::Forge)]
    {
        if let Some(text) = entry(file) {
            let manifest = entry("META-INF/MANIFEST.MF").unwrap_or_default();
            return Some(parse_mods_toml(&text, &manifest, format));
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
enum YamlValue {
    Str(String),
    List(Vec<String>),
}

impl YamlValue {
    fn into_string(self) -> Option<String> {
        match self {
            YamlValue::Str(s) => Some(s),
            YamlValue::List(l) => l.into_iter().next(),
        }
    }
    fn into_list(self) -> Vec<String> {
        match self {
            YamlValue::Str(s) if s.is_empty() => Vec::new(),
            YamlValue::Str(s) => vec![s],
            YamlValue::List(l) => l,
        }
    }
}

/// Терпимый разбор верхнего уровня YAML: `key: scalar`, `key: [a, "b"]`, блочные списки `- a`
/// и блочные строки `|` / `>`. Вложенные словари пропускаются. Для plugin.yml этого достаточно.
fn parse_top_level_yaml(text: &str) -> HashMap<String, YamlValue> {
    let mut out = HashMap::new();
    let lines: Vec<&str> = text.trim_start_matches('\u{feff}').lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.is_empty() || line.starts_with([' ', '\t', '#', '-']) || line.starts_with("---") {
            continue;
        }
        let Some((key, rest)) = split_key(line) else { continue };
        let rest = rest.trim();
        // Продолжение: строки с отступом или элементы списка `- x` в нулевой колонке.
        let mut block = Vec::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() || l.starts_with([' ', '\t']) || l.starts_with("- ") || l == "-" {
                block.push(l);
                i += 1;
            } else {
                break;
            }
        }
        let value = if rest.starts_with('[') {
            YamlValue::List(parse_flow_list(rest))
        } else if rest.starts_with('|') || rest.starts_with('>') {
            let folded = rest.starts_with('>');
            let parts: Vec<&str> = block.iter().map(|l| l.trim()).collect();
            let joined = parts.join(if folded { " " } else { "\n" });
            YamlValue::Str(joined.trim().to_string())
        } else if rest.is_empty() || rest.starts_with('#') {
            let items: Vec<String> = block
                .iter()
                .map(|l| l.trim())
                .filter_map(|l| l.strip_prefix('-'))
                .map(|l| unquote(strip_comment(l.trim())))
                .filter(|s| !s.is_empty())
                .collect();
            YamlValue::List(items)
        } else {
            YamlValue::Str(unquote(strip_comment(rest)))
        };
        out.insert(key.to_string(), value);
    }
    out
}

/// `key: rest` → (key, rest). Ключ может быть в кавычках.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = if let Some(q) = line.strip_prefix(['"', '\'']) {
        let quote = line.as_bytes()[0] as char;
        let end = q.find(quote)?;
        (&q[..end], q[end + 1..].trim_start().strip_prefix(':')?)
    } else {
        let idx = line.find(": ").or_else(|| line.strip_suffix(':').map(|s| s.len()))?;
        (line[..idx].trim(), line[idx + 1..].strip_prefix(' ').unwrap_or(&line[idx + 1..]))
    };
    Some((key, rest))
}

/// Обрезает комментарий ` # …` у скаляра без кавычек.
fn strip_comment(s: &str) -> &str {
    if s.starts_with(['"', '\'']) {
        return s;
    }
    match s.find(" #") {
        Some(i) => s[..i].trim_end(),
        None => s,
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => {}
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    s.to_string()
}

/// `[A, "B, C", 'D']` → [A, B, C, D]
fn parse_flow_list(s: &str) -> Vec<String> {
    let inner = s.trim().trim_start_matches('[');
    let inner = match inner.rfind(']') {
        Some(i) => &inner[..i],
        None => inner,
    };
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in inner.chars() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (Some(q), _) if c == q => {
                quote = None;
                cur.push(c);
            }
            (None, ',') => items.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    items.push(cur);
    items.iter().map(|i| unquote(i)).filter(|i| !i.is_empty()).collect()
}

fn parse_plugin_yml(text: &str, format: MetaFormat) -> Meta {
    let mut y = parse_top_level_yaml(text);
    let mut authors = y.remove("authors").map(YamlValue::into_list).unwrap_or_default();
    if let Some(a) = y.remove("author").and_then(YamlValue::into_string)
        && !a.is_empty()
        && !authors.contains(&a)
    {
        authors.insert(0, a);
    }
    Meta {
        format: Some(format),
        name: y.remove("name").and_then(YamlValue::into_string),
        version: y.remove("version").and_then(YamlValue::into_string),
        description: y.remove("description").and_then(YamlValue::into_string),
        authors,
    }
}

fn json_str(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Автор в fabric.mod.json — строка или `{ "name": … }`.
fn json_person(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(o) => json_str(o.get("name")),
        _ => None,
    }
}

fn parse_fabric_json(text: &str) -> Option<Meta> {
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let authors = v.get("authors").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(json_person).collect());
    Some(Meta {
        format: Some(MetaFormat::Fabric),
        name: json_str(v.get("name")).or_else(|| json_str(v.get("id"))),
        version: json_str(v.get("version")),
        description: json_str(v.get("description")),
        authors: authors.unwrap_or_default(),
    })
}

fn parse_quilt_json(text: &str) -> Option<Meta> {
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let ql = v.get("quilt_loader")?;
    let md = ql.get("metadata");
    let authors = md
        .and_then(|m| m.get("contributors"))
        .and_then(|c| c.as_object())
        .map(|c| c.keys().cloned().collect())
        .unwrap_or_default();
    Some(Meta {
        format: Some(MetaFormat::Quilt),
        name: json_str(md.and_then(|m| m.get("name"))).or_else(|| json_str(ql.get("id"))),
        version: json_str(ql.get("version")),
        description: json_str(md.and_then(|m| m.get("description"))),
        authors,
    })
}

fn parse_mods_toml(text: &str, manifest: &str, format: MetaFormat) -> Meta {
    let mut meta = Meta { format: Some(format), ..Meta::default() };
    let Ok(table) = text.trim_start_matches('\u{feff}').parse::<toml::Table>() else { return meta };
    let Some(m) = table.get("mods").and_then(|m| m.as_array()).and_then(|a| a.first()) else { return meta };
    let s = |k: &str| m.get(k).and_then(|v| v.as_str()).map(str::to_string);
    meta.name = s("displayName").or_else(|| s("modId"));
    meta.version = s("version");
    if meta.version.as_deref().is_some_and(|v| v.contains("${")) {
        meta.version = manifest_value(manifest, "Implementation-Version");
    }
    meta.description = s("description");
    meta.authors = s("authors")
        .or_else(|| table.get("authors").and_then(|v| v.as_str()).map(str::to_string))
        .map(|a| a.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    meta
}

fn manifest_value(manifest: &str, key: &str) -> Option<String> {
    manifest.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_string()).filter(|v| !v.is_empty() && !v.contains("${"))
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Хэш и сеть

pub fn sha1_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn user_agent() -> String {
    format!("mctui/{} (github.com/{})", crate::update::VERSION, crate::update::REPO)
}

fn curl() -> Command {
    let mut c = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    c.args(["-fsSL", "--connect-timeout", "10", "--retry", "2", "--max-time", "120", "-A"]).arg(user_agent());
    c
}

/// POST JSON (тело — через stdin, чтобы не упираться в длину командной строки на Windows).
fn post_json(url: &str, body: &serde_json::Value) -> Result<String> {
    let mut child = curl()
        .args(["-X", "POST", "-H", "Content-Type: application/json", "--data-binary", "@-", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(crate::tr!("не удалось запустить curl", "failed to run curl"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.to_string().as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(crate::tr!(format!("Modrinth недоступен ({err})"), format!("Modrinth is unreachable ({err})")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn download(url: &str, dest: &Path) -> Result<()> {
    if !url.starts_with("https://") {
        bail!(crate::tr!(
            format!("неподдерживаемый адрес загрузки: {url}"),
            format!("unsupported download URL: {url}")
        ));
    }
    let out = curl()
        .args(["--max-time", "600", "-o"])
        .arg(dest)
        .arg(url)
        .output()
        .context(crate::tr!("не удалось запустить curl", "failed to run curl"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(crate::tr!(format!("не скачался {url} ({err})"), format!("download failed: {url} ({err})")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Modrinth

#[derive(Debug, Clone, Deserialize)]
struct MrVersion {
    id: String,
    project_id: String,
    version_number: String,
    #[serde(default)]
    date_published: String,
    #[serde(default)]
    files: Vec<MrFile>,
}

#[derive(Debug, Clone, Deserialize)]
struct MrFile {
    #[serde(default)]
    hashes: HashMap<String, String>,
    url: String,
    filename: String,
    #[serde(default)]
    primary: bool,
}

impl MrVersion {
    fn has_hash(&self, sha1: &str) -> bool {
        self.files.iter().any(|f| f.hashes.get("sha1").is_some_and(|h| h.eq_ignore_ascii_case(sha1)))
    }

    fn primary_file(&self) -> Option<&MrFile> {
        self.files.iter().find(|f| f.primary).or_else(|| self.files.first())
    }

    fn published(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        chrono::DateTime::parse_from_rfc3339(&self.date_published).ok()
    }
}

/// Ответ `version_files` / `version_files/update`: `{ "<sha1>": Version, … }` (неизвестные хэши отсутствуют).
fn parse_versions_response(json: &str) -> Result<HashMap<String, MrVersion>> {
    serde_json::from_str(json).context(crate::tr!("непонятный ответ Modrinth", "unexpected Modrinth response"))
}

/// Решает, есть ли обновление: последняя версия не содержит установленный файл и не старше установленной
/// (с фильтром по версии игры Modrinth может вернуть версию старее установленной — это не обновление).
fn pick_update(sha1: &str, latest: &MrVersion, current: Option<&MrVersion>) -> Option<UpdateInfo> {
    if latest.has_hash(sha1) {
        return None;
    }
    if let Some(cur) = current {
        if cur.id == latest.id {
            return None;
        }
        if let (Some(a), Some(b)) = (latest.published(), cur.published())
            && a <= b
        {
            return None;
        }
    }
    let file = latest.primary_file()?;
    Some(UpdateInfo {
        version: latest.version_number.clone(),
        file_name: file.filename.clone(),
        url: file.url.clone(),
        sha1: file.hashes.get("sha1").map(|h| h.to_ascii_lowercase()),
        page: format!("https://modrinth.com/project/{}/version/{}", latest.project_id, latest.id),
    })
}

/// Ищет обновления на Modrinth по SHA-1 файлов. Загрузчики — по типу файла (см. [`PluginInfo::loaders`]):
/// плагины — paper/purpur/spigot/bukkit/folia, моды — fabric (+quilt), forge, neoforge.
/// `game_version` (например "1.21.4") ограничивает обновления совместимыми с этой версией Minecraft.
/// Возвращает только файлы, для которых есть более новая версия (с другим хэшем).
pub fn check_updates(plugins: &[PluginInfo], game_version: Option<&str>) -> Result<HashMap<PathBuf, UpdateInfo>> {
    check_updates_with(plugins, game_version, None)
}

/// Как [`check_updates`], но `plugin_loaders` переопределяет загрузчики для плагинов (например, только
/// `["spigot", "bukkit"]` на Spigot-сервере, чтобы не предлагать Paper-only сборки).
pub fn check_updates_with(
    plugins: &[PluginInfo],
    game_version: Option<&str>,
    plugin_loaders: Option<&[&str]>,
) -> Result<HashMap<PathBuf, UpdateInfo>> {
    let plugins: Vec<&PluginInfo> = plugins.iter().filter(|p| !p.sha1.is_empty()).collect();
    if plugins.is_empty() {
        return Ok(HashMap::new());
    }
    let loaders_of = |p: &PluginInfo| -> Vec<String> {
        let ls = match plugin_loaders {
            Some(ls) if p.loaders() == PLUGIN_LOADERS => ls,
            _ => p.loaders(),
        };
        ls.iter().map(|s| s.to_string()).collect()
    };

    // Один запрос на набор загрузчиков.
    let mut groups: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    for p in &plugins {
        let loaders = loaders_of(p);
        let idx = match groups.iter().position(|(l, _)| *l == loaders) {
            Some(i) => i,
            None => {
                groups.push((loaders, Vec::new()));
                groups.len() - 1
            }
        };
        if !groups[idx].1.contains(&p.sha1) {
            groups[idx].1.push(p.sha1.clone());
        }
    }
    let mut latest: HashMap<(Vec<String>, String), MrVersion> = HashMap::new();
    for (loaders, hashes) in groups {
        let mut body = serde_json::json!({ "hashes": hashes, "algorithm": "sha1", "loaders": loaders });
        if let Some(gv) = game_version.map(str::trim).filter(|g| !g.is_empty()) {
            body["game_versions"] = serde_json::json!([gv]);
        }
        let resp = parse_versions_response(&post_json(&format!("{MODRINTH_API}/version_files/update"), &body)?)?;
        for (hash, v) in resp {
            latest.insert((loaders.clone(), hash.to_ascii_lowercase()), v);
        }
    }
    if latest.is_empty() {
        return Ok(HashMap::new());
    }

    // Установленные версии — чтобы не предлагать «обновление» на более старую.
    let mut known: Vec<&str> = latest.keys().map(|(_, h)| h.as_str()).collect();
    known.sort_unstable();
    known.dedup();
    let body = serde_json::json!({ "hashes": known, "algorithm": "sha1" });
    let current: HashMap<String, MrVersion> =
        parse_versions_response(&post_json(&format!("{MODRINTH_API}/version_files"), &body)?)?
            .into_iter()
            .map(|(h, v)| (h.to_ascii_lowercase(), v))
            .collect();

    let mut out = HashMap::new();
    for p in plugins {
        let key = (loaders_of(p), p.sha1.to_ascii_lowercase());
        if let Some(l) = latest.get(&key)
            && let Some(u) = pick_update(&key.1, l, current.get(&key.1))
        {
            out.insert(p.path.clone(), u);
        }
    }
    Ok(out)
}

/// Имя файла из Modrinth должно быть простым `*.jar` без путей.
fn safe_jar_name(name: &str) -> Result<&str> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', ':', '\0'])
        && name.to_ascii_lowercase().ends_with(".jar");
    if !ok {
        bail!(crate::tr!(format!("недопустимое имя файла: {name}"), format!("invalid file name: {name}")));
    }
    Ok(name)
}

/// Скачивает новый jar во временный файл в той же папке, проверяет SHA-1, переносит старый jar в
/// `<plugins|mods>/.mctui-old/<файл>` (с перезаписью) и кладёт новый на место (выключенный плагин
/// остаётся выключенным: `*.jar.disabled`). Возвращает путь к новому файлу. Действует после перезапуска.
pub fn apply_update(p: &PluginInfo, u: &UpdateInfo) -> Result<PathBuf> {
    apply_update_with(p, u, download)
}

fn apply_update_with(p: &PluginInfo, u: &UpdateInfo, fetch: impl Fn(&str, &Path) -> Result<()>) -> Result<PathBuf> {
    let dir = p.path.parent().context(crate::tr!("нет папки плагина", "plugin has no parent folder"))?;
    let jar = safe_jar_name(&u.file_name)?;
    let new_name = if p.enabled { jar.to_string() } else { format!("{jar}{DISABLED_SUFFIX}") };
    let new_path = dir.join(&new_name);
    let old_name = p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.file_name.clone());
    if new_name != old_name && new_path.exists() {
        bail!(crate::tr!(format!("файл {new_name} уже существует"), format!("file {new_name} already exists")));
    }
    if !p.path.is_file() {
        bail!(crate::tr!(format!("файл {old_name} не найден"), format!("file {old_name} not found")));
    }

    // Без расширения .jar — сервер не подхватит недокачанный файл.
    let tmp = dir.join(format!(".mctui-download-{}.part", std::process::id()));
    let res = (|| -> Result<PathBuf> {
        fetch(&u.url, &tmp)?;
        let actual = sha1_file(&tmp)?;
        if let Some(expected) = &u.sha1
            && !expected.eq_ignore_ascii_case(&actual)
        {
            bail!(crate::tr!(
                format!("контрольная сумма не совпала (ожидалось {expected}, получено {actual})"),
                format!("checksum mismatch (expected {expected}, got {actual})")
            ));
        }
        zip::ZipArchive::new(std::fs::File::open(&tmp)?)
            .context(crate::tr!("скачанный файл — не jar", "downloaded file is not a jar"))?;

        let old_dir = dir.join(OLD_DIR);
        std::fs::create_dir_all(&old_dir)?;
        let backup = old_dir.join(&old_name);
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(&p.path, &backup).with_context(|| {
            crate::tr!(
                format!("не удалось убрать {old_name} в {OLD_DIR}"),
                format!("failed to move {old_name} to {OLD_DIR}")
            )
        })?;
        if let Err(e) = std::fs::rename(&tmp, &new_path) {
            let _ = std::fs::rename(&backup, &p.path);
            return Err(e)
                .context(crate::tr!(format!("не удалось записать {new_name}"), format!("failed to write {new_name}")));
        }
        Ok(new_path.clone())
    })();
    let _ = std::fs::remove_file(&tmp);
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "mctui-plugins-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_jar(path: &Path, entries: &[(&str, &str)]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, body) in entries {
            w.start_file(*name, SimpleFileOptions::default()).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn yaml_forms() {
        let y = parse_top_level_yaml(
            "\u{feff}name: \"My Plugin\" \nversion: 1.0 # comment\nmain: a.b.C\nauthors: [A, 'B, C', \"D\"]\n\
             commands:\n  foo:\n    description: nested\ndescription: |\n  line one\n  line two\nsoftdepend:\n- X\n- 'Y'\n",
        );
        assert_eq!(y["name"], YamlValue::Str("My Plugin".into()));
        assert_eq!(y["version"], YamlValue::Str("1.0".into()));
        assert_eq!(y["main"], YamlValue::Str("a.b.C".into()));
        assert_eq!(y["authors"], YamlValue::List(vec!["A".into(), "B, C".into(), "D".into()]));
        assert_eq!(y["description"], YamlValue::Str("line one\nline two".into()));
        assert_eq!(y["softdepend"], YamlValue::List(vec!["X".into(), "Y".into()]));
        assert!(!y.contains_key("foo"));

        let m = parse_plugin_yml("name: P\nauthor: X\nauthors:\n  - Y\n  - X\n", MetaFormat::Bukkit);
        assert_eq!(m.authors, vec!["Y", "X"]);
        let m = parse_plugin_yml("name: P\nauthor: 'It''s me'\n", MetaFormat::Bukkit);
        assert_eq!(m.authors, vec!["It's me"]);
    }

    #[test]
    fn scan_reads_metadata_and_sorts() {
        let t = TempDir::new("scan");
        let plugins = t.0.join("plugins");
        let mods = t.0.join("mods");
        make_jar(
            &plugins.join("zeta.jar"),
            &[("plugin.yml", "name: Alpha\nversion: '2.1'\nmain: x.Alpha\nauthors: [A, B]\ndescription: Does stuff\n")],
        );
        make_jar(&plugins.join("beta-1.0.jar.disabled"), &[("plugin.yml", "name: beta\nversion: 1.0\nauthor: X\n")]);
        make_jar(
            &plugins.join("Gamma.jar"),
            &[("paper-plugin.yml", "name: Gamma\nversion: 3\nmain: g.G\n"), ("plugin.yml", "name: Ignored\n")],
        );
        std::fs::write(plugins.join("broken.jar"), b"not a zip").unwrap();
        std::fs::write(plugins.join("readme.txt"), b"x").unwrap();
        std::fs::create_dir_all(plugins.join("Alpha")).unwrap();
        make_jar(&plugins.join("Alpha").join("inner.jar"), &[("plugin.yml", "name: Inner\n")]);
        make_jar(&plugins.join(OLD_DIR).join("old.jar"), &[("plugin.yml", "name: Old\n")]);
        make_jar(
            &mods.join("fabric-api.jar"),
            &[(
                "fabric.mod.json",
                r#"{"schemaVersion":1,"id":"fabric-api","version":"0.100.0","name":"Fabric API",
                   "description":"Core","authors":["FabricMC",{"name":"Someone"}]}"#,
            )],
        );
        make_jar(
            &mods.join("forgemod.jar"),
            &[
                (
                    "META-INF/mods.toml",
                    "modLoader=\"javafml\"\n[[mods]]\nmodId=\"fm\"\nversion=\"${file.jarVersion}\"\n\
                     displayName=\"Forge Mod\"\nauthors=\"P, Q\"\ndescription='''\nHi\n'''\n",
                ),
                ("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\nImplementation-Version: 4.5.6\n"),
            ],
        );

        let list = scan(&t.0);
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "beta", "broken", "Fabric API", "Forge Mod", "Gamma"]);

        let alpha = &list[0];
        assert_eq!(alpha.file_name, "zeta.jar");
        assert!(alpha.enabled);
        assert_eq!(alpha.kind, PluginKind::Plugin);
        assert_eq!(alpha.format, MetaFormat::Bukkit);
        assert_eq!(alpha.version.as_deref(), Some("2.1"));
        assert_eq!(alpha.description.as_deref(), Some("Does stuff"));
        assert_eq!(alpha.authors, ["A", "B"]);
        assert_eq!(alpha.sha1, sha1_file(&plugins.join("zeta.jar")).unwrap());
        assert_eq!(alpha.sha1.len(), 40);

        let beta = &list[1];
        assert!(!beta.enabled);
        assert_eq!(beta.authors, ["X"]);
        assert_eq!(beta.version.as_deref(), Some("1.0"));

        assert_eq!(list[2].format, MetaFormat::Unknown);
        assert_eq!(list[2].version, None);

        let fabric = &list[3];
        assert_eq!(fabric.kind, PluginKind::Mod);
        assert_eq!(fabric.format, MetaFormat::Fabric);
        assert_eq!(fabric.authors, ["FabricMC", "Someone"]);
        assert_eq!(fabric.loaders(), ["fabric"]);

        let forge = &list[4];
        assert_eq!(forge.format, MetaFormat::Forge);
        assert_eq!(forge.version.as_deref(), Some("4.5.6"));
        assert_eq!(forge.authors, ["P", "Q"]);
        assert_eq!(forge.description.as_deref(), Some("Hi"));
        assert_eq!(forge.loaders(), ["forge"]);

        assert_eq!(list[5].format, MetaFormat::Paper);
        assert_eq!(list[5].loaders(), PLUGIN_LOADERS);
    }

    #[test]
    fn set_enabled_round_trip() {
        let t = TempDir::new("toggle");
        make_jar(&t.0.join("plugins").join("Foo.jar"), &[("plugin.yml", "name: Foo\n")]);
        let p = scan(&t.0).remove(0);
        assert!(p.enabled);
        let off = set_enabled(&p, false).unwrap();
        assert_eq!(off.file_name().unwrap(), "Foo.jar.disabled");
        let p = scan(&t.0).remove(0);
        assert!(!p.enabled);
        assert_eq!(p.name, "Foo");
        assert_eq!(set_enabled(&p, false).unwrap(), p.path);
        let on = set_enabled(&p, true).unwrap();
        assert_eq!(on.file_name().unwrap(), "Foo.jar");
        assert!(scan(&t.0)[0].enabled);
    }

    /// Урезанный реальный ответ `POST /v2/version_files/update` (Chunky, ноябрь 2026).
    const UPDATE_FIXTURE: &str = r#"{
      "7c196c5bebd6017880f3c2a38fc297b3b99f5f93": {
        "game_versions": ["26.2", "26.3"], "loaders": ["folia", "paper"], "environment": "unknown",
        "id": "RBuKjN4k", "project_id": "fALzjamp", "author_id": "nfBsvT1J", "featured": false,
        "name": "Chunky 1.5.66", "version_number": "1.5.66", "changelog": "", "changelog_url": null,
        "date_published": "2026-11-04T03:00:18.722959Z", "downloads": 1000, "version_type": "release",
        "status": "listed", "requested_status": null,
        "files": [{
          "id": "abc", "hashes": {"sha1": "1ad238cf2e923a7dac91c93ae5c7681bcb96abda", "sha512": "00"},
          "url": "https://cdn.modrinth.com/data/fALzjamp/versions/RBuKjN4k/Chunky-Paper-1.5.66.jar",
          "filename": "Chunky-Paper-1.5.66.jar", "primary": true, "size": 300000, "file_type": null
        }],
        "dependencies": []
      }
    }"#;

    const CURRENT_FIXTURE: &str = r#"{
      "7c196c5bebd6017880f3c2a38fc297b3b99f5f93": {
        "id": "P3y2MXnd", "project_id": "fALzjamp", "version_number": "1.4.40",
        "date_published": "2025-06-21T08:01:39.060370Z",
        "files": [{"hashes": {"sha1": "7c196c5bebd6017880f3c2a38fc297b3b99f5f93"},
          "url": "https://cdn.modrinth.com/data/fALzjamp/versions/P3y2MXnd/Chunky-Bukkit-1.4.40.jar",
          "filename": "Chunky-Bukkit-1.4.40.jar", "primary": true}]
      }
    }"#;

    #[test]
    fn parses_modrinth_response() {
        let hash = "7c196c5bebd6017880f3c2a38fc297b3b99f5f93";
        let latest = parse_versions_response(UPDATE_FIXTURE).unwrap();
        let current = parse_versions_response(CURRENT_FIXTURE).unwrap();
        let u = pick_update(hash, &latest[hash], current.get(hash)).unwrap();
        assert_eq!(u.version, "1.5.66");
        assert_eq!(u.file_name, "Chunky-Paper-1.5.66.jar");
        assert_eq!(u.sha1.as_deref(), Some("1ad238cf2e923a7dac91c93ae5c7681bcb96abda"));
        assert!(u.url.starts_with("https://cdn.modrinth.com/"));
        assert_eq!(u.page, "https://modrinth.com/project/fALzjamp/version/RBuKjN4k");

        // Уже последняя: хэш совпадает.
        assert!(pick_update("1ad238cf2e923a7dac91c93ae5c7681bcb96abda", &latest[hash], None).is_none());
        // «Обновление» на более старую версию (фильтр по версии игры) не предлагается.
        assert!(pick_update(hash, &current[hash], Some(&latest[hash])).is_none());
        assert!(parse_versions_response("[]").is_err());
        assert!(parse_versions_response("{}").unwrap().is_empty());
    }

    #[test]
    fn apply_update_offline() {
        let t = TempDir::new("apply");
        let plugins = t.0.join("plugins");
        make_jar(&plugins.join("Foo-1.0.jar.disabled"), &[("plugin.yml", "name: Foo\nversion: 1.0\n")]);
        let src = t.0.join("Foo-2.0.jar");
        make_jar(&src, &[("plugin.yml", "name: Foo\nversion: 2.0\n")]);
        let p = scan(&t.0).remove(0);
        let fetch = |_: &str, dest: &Path| -> Result<()> {
            std::fs::copy(&src, dest)?;
            Ok(())
        };
        let mut u = UpdateInfo {
            version: "2.0".into(),
            file_name: "Foo-2.0.jar".into(),
            url: "https://example.invalid/Foo-2.0.jar".into(),
            sha1: Some("0".repeat(40)),
            page: String::new(),
        };
        assert!(apply_update_with(&p, &u, fetch).is_err());
        assert!(p.path.exists(), "old jar must stay on checksum mismatch");

        u.file_name = "../evil.jar".into();
        assert!(apply_update_with(&p, &u, fetch).is_err());

        u.file_name = "Foo-2.0.jar".into();
        u.sha1 = Some(sha1_file(&src).unwrap().to_uppercase());
        let new = apply_update_with(&p, &u, fetch).unwrap();
        assert_eq!(new, plugins.join("Foo-2.0.jar.disabled"));
        assert!(!p.path.exists());
        assert!(plugins.join(OLD_DIR).join("Foo-1.0.jar.disabled").exists());
        let list = scan(&t.0);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].version.as_deref(), Some("2.0"));
        assert!(!list[0].enabled);
        let leftovers: Vec<_> = std::fs::read_dir(&plugins)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty());
    }

    /// `cargo test -- --ignored plugins`: устаревший Chunky с Modrinth → находит обновление и ставит его.
    #[test]
    #[ignore = "network"]
    fn plugins_network_update() {
        let t = TempDir::new("net");
        let plugins = t.0.join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let old = plugins.join("Chunky-Bukkit-1.4.40.jar");
        download("https://cdn.modrinth.com/data/fALzjamp/versions/P3y2MXnd/Chunky-Bukkit-1.4.40.jar", &old).unwrap();
        let list = scan(&t.0);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Chunky");
        assert_eq!(list[0].version.as_deref(), Some("1.4.40"));
        assert_eq!(list[0].sha1, "7c196c5bebd6017880f3c2a38fc297b3b99f5f93");

        let updates = check_updates(&list, None).unwrap();
        let u = updates.get(&old).expect("update for Chunky 1.4.40");
        assert_ne!(u.version, "1.4.40");
        eprintln!("update: {u:?}");

        // С фильтром по старой версии игры обновления «назад» не предлагаются.
        assert!(check_updates(&list, Some("1.19.2")).unwrap().is_empty());

        let new = apply_update(&list[0], u).unwrap();
        assert!(new.exists());
        assert!(!old.exists());
        assert!(plugins.join(OLD_DIR).join("Chunky-Bukkit-1.4.40.jar").exists());
        let after = scan(&t.0);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].version.as_deref(), Some(u.version.as_str()));
        assert!(check_updates(&after, None).unwrap().is_empty());
    }
}
