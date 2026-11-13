//! Поиск и установка новых плагинов / модов: Modrinth (`/v2/search`) и Hangar (PaperMC, только для Paper / Purpur).
//!
//! Поток для интерфейса: [`Target::detect`] → [`search`] (страницами по [`PAGE_SIZE`]) → [`resolve`] выбранного
//! результата (новейшая совместимая версия + обязательные зависимости, уже установленные пропускаются) →
//! подтверждение пользователем → [`install`]. Для «готовых рецептов» (например, Chunky для прогрузки мира) —
//! [`plan_by_slug`].
//!
//! Все функции блокирующие (системный `curl`, как в `plugins.rs`): вызывать через `spawn_blocking`.
//! Установленное начинает работать только после перезапуска сервера.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use sha2::{Sha256, Sha512};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::plugins::{self, MrDependency, MrVersion, PluginKind};
use crate::tr;

pub(crate) const HANGAR_API: &str = "https://hangar.papermc.io/api/v1";
/// Результатов на страницу из каждого источника.
pub const PAGE_SIZE: usize = 20;
/// Сколько версий проекта смотреть при выборе совместимой.
const VERSIONS_LIMIT: usize = 25;
/// Глубина рекурсии по зависимостям (защита от странных графов).
const MAX_DEPTH: usize = 8;

// ---------------------------------------------------------------------------------------------------------------
// Цель: под какой сервер искать

/// Ядро сервера — от него зависят загрузчики Modrinth, папка (`plugins/` или `mods/`) и доступность Hangar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Paper,
    Purpur,
    Folia,
    Spigot,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
    /// Датапаки мира (Modrinth, загрузчик `datapack`); `detect` его не возвращает — выбирается вкладкой «Датапаки».
    Datapack,
}

impl Platform {
    pub fn label(self) -> &'static str {
        match self {
            Platform::Paper => "Paper",
            Platform::Purpur => "Purpur",
            Platform::Folia => "Folia",
            Platform::Spigot => "Spigot",
            Platform::Fabric => "Fabric",
            Platform::Quilt => "Quilt",
            Platform::Forge => "Forge",
            Platform::NeoForge => "NeoForge",
            Platform::Datapack => "Minecraft",
        }
    }

    pub fn kind(self) -> PluginKind {
        match self {
            Platform::Paper | Platform::Purpur | Platform::Folia | Platform::Spigot => PluginKind::Plugin,
            Platform::Fabric | Platform::Quilt | Platform::Forge | Platform::NeoForge => PluginKind::Mod,
            Platform::Datapack => PluginKind::Datapack,
        }
    }

    /// Загрузчики Modrinth, сборки под которые запускаются на этом ядре.
    /// Folia-only сборки на Paper не предлагаются (и наоборот: Folia грузит только помеченные под неё).
    pub fn loaders(self) -> &'static [&'static str] {
        match self {
            Platform::Paper => &["paper", "spigot", "bukkit"],
            Platform::Purpur => &["purpur", "paper", "spigot", "bukkit"],
            Platform::Folia => &["folia"],
            Platform::Spigot => &["spigot", "bukkit"],
            Platform::Fabric => plugins::FABRIC_LOADERS,
            Platform::Quilt => plugins::QUILT_LOADERS,
            Platform::Forge => plugins::FORGE_LOADERS,
            Platform::NeoForge => plugins::NEOFORGE_LOADERS,
            Platform::Datapack => crate::datapacks::DATAPACK_LOADERS,
        }
    }

    /// Платформа Hangar (там только Paper-плагины; Purpur их тоже грузит).
    fn hangar(self) -> Option<&'static str> {
        matches!(self, Platform::Paper | Platform::Purpur).then_some("PAPER")
    }

    /// Угадывает ядро по папке сервера: имена jar в корне (и `hint` — например, команда запуска),
    /// служебные файлы ядер (`purpur.yml`, `config/paper-global.yml`, `.fabric/`, `libraries/net/neoforged`…),
    /// в крайнем случае — по наличию `plugins/` или `mods/`.
    pub fn detect(server_dir: &Path, hint: &str) -> Option<Platform> {
        let mut names: Vec<String> = std::fs::read_dir(server_dir)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect())
            .unwrap_or_default();
        names.retain(|n| n.ends_with(".jar"));
        names.push(hint.to_lowercase());
        let has = |k: &str| names.iter().any(|n| n.contains(k));
        let exists = |p: &str| server_dir.join(p).exists();
        if has("purpur") || exists("purpur.yml") {
            return Some(Platform::Purpur);
        }
        if has("folia") {
            return Some(Platform::Folia);
        }
        if has("paper") || exists("config/paper-global.yml") || exists("paper.yml") {
            return Some(Platform::Paper);
        }
        if has("neoforge") || exists("libraries/net/neoforged") {
            return Some(Platform::NeoForge);
        }
        if has("forge") || exists("libraries/net/minecraftforge") {
            return Some(Platform::Forge);
        }
        if has("quilt") || exists(".quilt") {
            return Some(Platform::Quilt);
        }
        if has("fabric") || exists(".fabric") {
            return Some(Platform::Fabric);
        }
        if has("spigot") || has("craftbukkit") || exists("spigot.yml") {
            return Some(Platform::Spigot);
        }
        if exists("plugins") {
            return Some(Platform::Paper);
        }
        if exists("mods") {
            return Some(Platform::Fabric);
        }
        None
    }
}

/// Сервер, под который ищем: ядро + версия Minecraft (`None` — без фильтра по версии).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub platform: Platform,
    pub game_version: Option<String>,
}

impl Target {
    pub fn new(platform: Platform, game_version: Option<&str>) -> Self {
        let game_version = game_version.map(str::trim).filter(|g| !g.is_empty()).map(str::to_string);
        Target { platform, game_version }
    }

    /// [`Platform::detect`] + версия игры (обычно `ServerState::version` из лога запуска).
    pub fn detect(server_dir: &Path, hint: &str, game_version: Option<&str>) -> Option<Self> {
        Platform::detect(server_dir, hint).map(|p| Target::new(p, game_version))
    }

    pub fn kind(&self) -> PluginKind {
        self.platform.kind()
    }

    fn gv(&self) -> Option<&str> {
        self.game_version.as_deref()
    }
}

/// Папка внутри сервера: `plugins` или `mods`.
pub fn folder(kind: PluginKind) -> &'static str {
    match kind {
        PluginKind::Plugin => "plugins",
        PluginKind::Mod => "mods",
        PluginKind::Datapack => crate::datapacks::ENABLED_DIR,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Сеть (подменяется в тестах)

pub(crate) trait Http {
    /// GET; `Ok(None)` — 404.
    fn get(&self, url: &str) -> Result<Option<String>>;
    fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<String>;
}

pub(crate) struct Curl;

fn source_name(url: &str) -> &'static str {
    if url.starts_with(HANGAR_API) { "Hangar" } else { "Modrinth" }
}

impl Http for Curl {
    fn get(&self, url: &str) -> Result<Option<String>> {
        let out = plugins::curl()
            .args(["--max-time", "30", "-w", "\n%{http_code}"])
            .arg(url)
            .stdin(Stdio::null())
            .output()
            .context(tr!("не удалось запустить curl", "failed to run curl"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        if code.trim() == "404" {
            return Ok(None);
        }
        if !out.status.success() {
            let src = source_name(url);
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!(tr!(format!("{src} недоступен ({err})"), format!("{src} is unreachable ({err})")));
        }
        Ok(Some(body.to_string()))
    }

    fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<String> {
        plugins::post_json(url, body)
    }
}

fn parse<T: serde::de::DeserializeOwned>(json: &str, src: &str) -> Result<T> {
    serde_json::from_str(json)
        .with_context(|| tr!(format!("непонятный ответ {src}"), format!("unexpected response from {src}")))
}

/// Процентное кодирование для query-параметров.
pub(crate) fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Нормализованное имя для сравнения: только буквы/цифры в нижнем регистре («Fabric API» = «fabric-api»).
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Ключ сортировки релизной версии игры («1.21.10» > «1.21.4»); снапшоты — None.
fn game_version_key(v: &str) -> Option<Vec<u32>> {
    v.split('.').map(|p| p.parse().ok()).collect()
}

pub(crate) fn newest_game_version<'a>(vs: impl IntoIterator<Item = &'a String>) -> Option<String> {
    vs.into_iter().filter_map(|v| Some((game_version_key(v)?, v))).max().map(|(_, v)| v.clone())
}

// ---------------------------------------------------------------------------------------------------------------
// Поиск

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Modrinth,
    Hangar,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub source: Source,
    /// Modrinth: project id (`fALzjamp`); Hangar: числовой id проекта.
    pub id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub author: String,
    pub downloads: u64,
    /// Новейшая поддерживаемая версия Minecraft (по данным поиска), если известна.
    pub latest_game_version: Option<String>,
    /// Страница проекта в браузере.
    pub page: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    /// Недоступный источник при частичном успехе (двуязычный текст для тоста).
    pub warnings: Vec<String>,
    /// Есть ли следующая страница (`offset + PAGE_SIZE`) хотя бы в одном источнике.
    pub more: bool,
}

/// Фасеты Modrinth: тип проекта, загрузчики (ИЛИ), версия игры, без чисто клиентских.
fn modrinth_facets(target: &Target) -> serde_json::Value {
    let ptype = match target.kind() {
        PluginKind::Plugin => "plugin",
        PluginKind::Mod => "mod",
        PluginKind::Datapack => "datapack",
    };
    let mut facets = vec![
        vec![format!("project_type:{ptype}")],
        target.platform.loaders().iter().map(|l| format!("categories:{l}")).collect(),
    ];
    if let Some(gv) = target.gv() {
        facets.push(vec![format!("versions:{gv}")]);
    }
    facets.push(vec!["server_side!=unsupported".to_string()]);
    serde_json::json!(facets)
}

fn modrinth_search_url(query: &str, target: &Target, offset: usize) -> String {
    format!(
        "{}/search?query={}&limit={PAGE_SIZE}&offset={offset}&index=relevance&facets={}",
        plugins::MODRINTH_API,
        enc(query.trim()),
        enc(&modrinth_facets(target).to_string())
    )
}

fn hangar_search_url(query: &str, platform: &str, target: &Target, offset: usize) -> String {
    let mut url = format!("{HANGAR_API}/projects?limit={PAGE_SIZE}&offset={offset}&platform={platform}");
    if !query.trim().is_empty() {
        url.push_str(&format!("&q={}", enc(query.trim())));
    }
    if let Some(gv) = target.gv() {
        url.push_str(&format!("&version={}", enc(gv)));
    }
    url
}

#[derive(Debug, Deserialize)]
struct MrSearch {
    hits: Vec<MrHit>,
    #[serde(default)]
    total_hits: usize,
}

#[derive(Debug, Deserialize)]
struct MrHit {
    project_id: String,
    slug: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    project_type: String,
    #[serde(default)]
    versions: Vec<String>,
}

impl MrHit {
    fn into_hit(self) -> SearchHit {
        let ptype = if self.project_type.is_empty() { "project".to_string() } else { self.project_type };
        SearchHit {
            source: Source::Modrinth,
            page: format!("https://modrinth.com/{ptype}/{}", self.slug),
            latest_game_version: newest_game_version(&self.versions),
            id: self.project_id,
            slug: self.slug,
            title: self.title,
            description: self.description,
            author: self.author,
            downloads: self.downloads,
        }
    }
}

fn parse_modrinth_search(json: &str) -> Result<(Vec<SearchHit>, usize)> {
    let r: MrSearch = parse(json, "Modrinth")?;
    Ok((r.hits.into_iter().map(MrHit::into_hit).collect(), r.total_hits))
}

#[derive(Debug, Deserialize)]
struct HgSearch {
    #[serde(default)]
    pagination: HgPagination,
    result: Vec<HgProject>,
}

#[derive(Debug, Default, Deserialize)]
struct HgPagination {
    #[serde(default)]
    count: usize,
}

#[derive(Debug, Deserialize)]
struct HgProject {
    id: u64,
    name: String,
    namespace: HgNamespace,
    #[serde(default)]
    description: String,
    #[serde(default)]
    stats: HgStats,
    #[serde(default, rename = "supportedPlatforms")]
    supported_platforms: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct HgNamespace {
    owner: String,
    slug: String,
}

#[derive(Debug, Default, Deserialize)]
struct HgStats {
    #[serde(default)]
    downloads: u64,
}

impl HgProject {
    fn into_hit(self, platform: &str) -> SearchHit {
        SearchHit {
            source: Source::Hangar,
            id: self.id.to_string(),
            page: format!("https://hangar.papermc.io/{}/{}", self.namespace.owner, self.namespace.slug),
            latest_game_version: self.supported_platforms.get(platform).and_then(newest_game_version),
            slug: self.namespace.slug,
            title: self.name,
            description: self.description,
            author: self.namespace.owner,
            downloads: self.stats.downloads,
        }
    }
}

fn parse_hangar_search(json: &str, platform: &str) -> Result<(Vec<SearchHit>, usize)> {
    let r: HgSearch = parse(json, "Hangar")?;
    Ok((r.result.into_iter().map(|p| p.into_hit(platform)).collect(), r.pagination.count))
}

/// Склеивает выдачу: сначала Modrinth, затем проекты Hangar, которых нет на Modrinth (по имени или slug).
fn merge_hits(modrinth: Vec<SearchHit>, hangar: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut seen: HashSet<String> = HashSet::new();
    for h in &modrinth {
        seen.insert(norm(&h.title));
        seen.insert(norm(&h.slug));
    }
    let mut out = modrinth;
    for h in hangar {
        let (t, s) = (norm(&h.title), norm(&h.slug));
        if seen.contains(&t) || seen.contains(&s) {
            continue;
        }
        seen.insert(t);
        seen.insert(s);
        out.push(h);
    }
    out
}

/// Ищет плагины/моды под сервер. Пустой запрос — популярное. `offset` — смещение страницы в каждом
/// источнике (0, PAGE_SIZE, 2·PAGE_SIZE…). Если один источник недоступен, результат другого возвращается
/// с предупреждением в `warnings`; ошибка — только если не ответил ни один.
/// Дубли между страницами не убираются — при подгрузке «ещё» сравнивайте `source` + `id`.
pub fn search(query: &str, target: &Target, offset: usize) -> Result<SearchResults> {
    search_with(&Curl, query, target, offset)
}

pub(crate) fn search_with(http: &dyn Http, query: &str, target: &Target, offset: usize) -> Result<SearchResults> {
    let mut res = SearchResults::default();
    let mut errors = Vec::new();

    let modrinth = (|| -> Result<Vec<SearchHit>> {
        let body = http.get(&modrinth_search_url(query, target, offset))?.unwrap_or_default();
        let (hits, total) = parse_modrinth_search(&body)?;
        res.more |= offset + PAGE_SIZE < total;
        Ok(hits)
    })();
    let modrinth = modrinth.unwrap_or_else(|e| {
        errors.push(format!("Modrinth: {e:#}"));
        Vec::new()
    });

    let mut hangar = Vec::new();
    if let Some(platform) = target.platform.hangar() {
        let r = (|| -> Result<Vec<SearchHit>> {
            let body = http.get(&hangar_search_url(query, platform, target, offset))?.unwrap_or_default();
            let (hits, total) = parse_hangar_search(&body, platform)?;
            res.more |= offset + PAGE_SIZE < total;
            Ok(hits)
        })();
        match r {
            Ok(h) => hangar = h,
            Err(e) => errors.push(format!("Hangar: {e:#}")),
        }
    }

    let sources = if target.platform.hangar().is_some() { 2 } else { 1 };
    if errors.len() == sources {
        bail!(tr!(format!("поиск не удался: {}", errors.join("; ")), format!("search failed: {}", errors.join("; "))));
    }
    res.warnings = errors
        .into_iter()
        .map(|e| tr!(format!("часть результатов не загрузилась — {e}"), format!("some results are missing — {e}")))
        .collect();
    res.hits = merge_hits(modrinth, hangar);
    Ok(res)
}

// ---------------------------------------------------------------------------------------------------------------
// План установки

/// Один файл к скачиванию.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFile {
    pub source: Source,
    pub project_id: String,
    pub title: String,
    pub version: String,
    pub file_name: String,
    pub url: String,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstallPlan {
    /// Папка установки: `plugins/` или `mods/`.
    pub kind: PluginKind,
    pub main: PlannedFile,
    /// Обязательные зависимости к установке (рекурсивно, без уже установленных).
    pub deps: Vec<PlannedFile>,
    /// Обязательные зависимости, которые уже стоят (имена для показа; выключенные помечены).
    pub already_installed: Vec<String>,
    /// Обязательные зависимости, которые не удалось подобрать автоматически (имя + причина/ссылка).
    pub unresolved: Vec<String>,
    /// Если сам проект уже установлен — путь к его файлу (интерфейсу стоит предупредить / предложить обновление).
    pub main_installed: Option<PathBuf>,
}

impl InstallPlan {
    /// Все файлы: сначала зависимости, затем основной.
    pub fn files(&self) -> impl Iterator<Item = &PlannedFile> {
        self.deps.iter().chain(std::iter::once(&self.main))
    }

    /// Суммарный размер скачивания (известных размеров).
    pub fn total_size(&self) -> u64 {
        self.files().filter_map(|f| f.size).sum()
    }
}

/// Что уже стоит на сервере: нормализованные имена (из метаданных jar и имени файла) и id проектов Modrinth.
#[derive(Debug, Default, Clone)]
pub(crate) struct InstalledIndex {
    /// norm(имя) → (путь, включён)
    names: HashMap<String, (PathBuf, bool)>,
    /// project_id Modrinth → (путь, включён)
    projects: HashMap<String, (PathBuf, bool)>,
}

impl InstalledIndex {
    /// Сканирует папку нужного типа; id проектов Modrinth — по SHA-1 (`POST /version_files`). Сбой сети не фатален.
    /// Для датапаков `server_dir` — папка мира (`datapacks::level_dir`), сканируются `datapacks/` и `datapacks.disabled/`.
    fn load(http: &dyn Http, server_dir: &Path, kind: PluginKind) -> Self {
        // (путь, имя, имя файла, включён, sha1)
        let list: Vec<(PathBuf, String, String, bool, String)> = if kind == PluginKind::Datapack {
            crate::datapacks::scan(server_dir)
                .into_iter()
                .map(|d| (d.path, d.name, d.file_name, d.enabled, d.sha1))
                .collect()
        } else {
            plugins::scan(server_dir)
                .into_iter()
                .filter(|p| p.kind == kind)
                .map(|p| (p.path, p.name, p.file_name, p.enabled, p.sha1))
                .collect()
        };
        let mut idx = InstalledIndex::default();
        for (path, name, file_name, enabled, _) in &list {
            let stem = plugins::split_jar_name(file_name).map(|(s, _)| s).unwrap_or(file_name);
            let stem = stem.strip_suffix(".zip").unwrap_or(stem);
            for n in [norm(name), norm(stem)] {
                if !n.is_empty() {
                    idx.names.entry(n).or_insert((path.clone(), *enabled));
                }
            }
        }
        let hashes: Vec<&str> = list.iter().map(|p| p.4.as_str()).filter(|h| !h.is_empty()).collect();
        if !hashes.is_empty() {
            let body = serde_json::json!({ "hashes": hashes, "algorithm": "sha1" });
            let url = format!("{}/version_files", plugins::MODRINTH_API);
            if let Ok(map) = http.post_json(&url, &body).and_then(|r| plugins::parse_versions_response(&r)) {
                for (hash, v) in map {
                    if let Some(p) = list.iter().find(|p| p.4.eq_ignore_ascii_case(&hash)) {
                        idx.projects.entry(v.project_id).or_insert((p.0.clone(), p.3));
                    }
                }
            }
        }
        idx
    }

    fn find(&self, project_id: Option<&str>, names: &[&str]) -> Option<&(PathBuf, bool)> {
        project_id
            .and_then(|id| self.projects.get(id))
            .or_else(|| names.iter().map(|n| norm(n)).filter(|n| !n.is_empty()).find_map(|n| self.names.get(&n)))
    }
}

fn label_installed(title: &str, enabled: bool) -> String {
    if enabled { title.to_string() } else { tr!(format!("{title} (выключен)"), format!("{title} (disabled)")) }
}

#[derive(Debug, Deserialize)]
struct MrProject {
    id: String,
    slug: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    project_type: String,
    #[serde(default)]
    game_versions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct HgVersions {
    #[serde(default)]
    result: Vec<HgVersion>,
}

#[derive(Debug, Clone, Deserialize)]
struct HgVersion {
    name: String,
    #[serde(default, rename = "createdAt")]
    created_at: String,
    #[serde(default)]
    channel: Option<HgChannel>,
    #[serde(default)]
    downloads: HashMap<String, HgDownload>,
    #[serde(default, rename = "pluginDependencies")]
    plugin_dependencies: HashMap<String, Vec<HgDependency>>,
}

#[derive(Debug, Clone, Deserialize)]
struct HgChannel {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HgDownload {
    #[serde(default, rename = "fileInfo")]
    file_info: Option<HgFileInfo>,
    #[serde(default, rename = "externalUrl")]
    external_url: Option<String>,
    #[serde(default, rename = "downloadUrl")]
    download_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct HgFileInfo {
    name: String,
    #[serde(default, rename = "sizeBytes")]
    size_bytes: Option<u64>,
    #[serde(default, rename = "sha256Hash")]
    sha256_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct HgDependency {
    name: String,
    #[serde(default, rename = "projectId")]
    project_id: Option<u64>,
    #[serde(default)]
    required: bool,
    #[serde(default, rename = "externalUrl")]
    external_url: Option<String>,
}

fn mr_versions_url(project: &str, target: &Target) -> String {
    let loaders = serde_json::json!(target.platform.loaders()).to_string();
    let mut url = format!(
        "{}/project/{}/version?include_changelog=false&loaders={}",
        plugins::MODRINTH_API,
        enc(project),
        enc(&loaders)
    );
    if let Some(gv) = target.gv() {
        url.push_str(&format!("&game_versions={}", enc(&serde_json::json!([gv]).to_string())));
    }
    url
}

fn hg_versions_url(project: &str, platform: &str, target: &Target) -> String {
    let mut url =
        format!("{HANGAR_API}/projects/{}/versions?limit={VERSIONS_LIMIT}&offset=0&platform={platform}", enc(project));
    if let Some(gv) = target.gv() {
        url.push_str(&format!("&platformVersion={}", enc(gv)));
    }
    url
}

fn mr_compatible(v: &MrVersion, target: &Target) -> bool {
    v.loaders.is_empty() || v.loaders.iter().any(|l| target.platform.loaders().contains(&l.as_str()))
}

/// Расширение основного файла версии: `.zip` у датапаков, `.jar` у остальных.
fn file_ext(kind: PluginKind) -> &'static str {
    if kind == PluginKind::Datapack { ".zip" } else { ".jar" }
}

fn mr_file(v: &MrVersion, kind: PluginKind) -> Option<&plugins::MrFile> {
    v.primary_file().filter(|f| {
        f.filename.to_ascii_lowercase().ends_with(file_ext(kind))
            && (kind != PluginKind::Datapack || plugins::safe_file_name(&f.filename, ".zip").is_ok())
    })
}

/// Новейшая подходящая версия Modrinth: с jar- (у датапаков zip-) файлом, совместимым загрузчиком; релиз предпочтительнее беты/альфы.
fn pick_modrinth(mut versions: Vec<MrVersion>, target: &Target) -> Option<MrVersion> {
    versions.retain(|v| mr_compatible(v, target) && mr_file(v, target.kind()).is_some());
    versions.sort_by_key(|v| std::cmp::Reverse(v.published()));
    let idx = versions.iter().position(|v| v.version_type == "release").unwrap_or(0);
    (idx < versions.len()).then(|| versions.swap_remove(idx))
}

fn mr_planned(v: &MrVersion, title: &str) -> Option<PlannedFile> {
    let f = v.primary_file()?; // имя уже проверено `mr_file` в `pick_modrinth`
    let h = |k: &str| f.hashes.get(k).map(|x| x.to_ascii_lowercase());
    Some(PlannedFile {
        source: Source::Modrinth,
        project_id: v.project_id.clone(),
        title: title.to_string(),
        version: v.version_number.clone(),
        file_name: f.filename.clone(),
        url: f.url.clone(),
        sha1: h("sha1"),
        sha512: h("sha512"),
        sha256: None,
        size: f.size,
    })
}

/// Новейшая подходящая версия Hangar: со скачиваемым файлом для платформы; канал Release предпочтительнее.
/// `Err` — если совместимые версии есть, но только внешней ссылкой (скачать автоматически нельзя).
fn pick_hangar(mut versions: Vec<HgVersion>, platform: &str, title: &str) -> Result<Option<HgVersion>> {
    versions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let downloadable = |v: &HgVersion| {
        v.downloads.get(platform).is_some_and(|d| {
            d.download_url.as_deref().is_some_and(|u| u.starts_with("https://"))
                && d.file_info.as_ref().is_some_and(|f| f.sha256_hash.is_some())
        })
    };
    let external = versions.iter().find_map(|v| v.downloads.get(platform)?.external_url.clone());
    let candidates: Vec<HgVersion> = versions.into_iter().filter(downloadable).collect();
    if candidates.is_empty() {
        if let Some(url) = external {
            bail!(tr!(
                format!("{title} публикуется только внешней ссылкой: {url}"),
                format!("{title} is only published as an external link: {url}")
            ));
        }
        return Ok(None);
    }
    let is_release = |v: &HgVersion| v.channel.as_ref().is_some_and(|c| c.name.eq_ignore_ascii_case("release"));
    let idx = candidates.iter().position(is_release).unwrap_or(0);
    Ok(candidates.into_iter().nth(idx))
}

fn hg_planned(v: &HgVersion, platform: &str, project_id: &str, title: &str) -> Option<PlannedFile> {
    let d = v.downloads.get(platform)?;
    let info = d.file_info.as_ref()?;
    Some(PlannedFile {
        source: Source::Hangar,
        project_id: project_id.to_string(),
        title: title.to_string(),
        version: v.name.clone(),
        file_name: info.name.clone(),
        url: d.download_url.clone()?,
        sha1: None,
        sha512: None,
        sha256: info.sha256_hash.as_ref().map(|h| h.to_ascii_lowercase()),
        size: info.size_bytes,
    })
}

struct Resolver<'a> {
    http: &'a dyn Http,
    target: &'a Target,
    installed: &'a InstalledIndex,
    visited: HashSet<String>,
    deps: Vec<PlannedFile>,
    already: Vec<String>,
    unresolved: Vec<String>,
}

impl Resolver<'_> {
    fn mr_project(&self, id: &str) -> Result<Option<MrProject>> {
        let url = format!("{}/project/{}", plugins::MODRINTH_API, enc(id));
        self.http.get(&url)?.map(|b| parse(&b, "Modrinth")).transpose()
    }

    fn mr_versions(&self, project: &str) -> Result<Vec<MrVersion>> {
        Ok(match self.http.get(&mr_versions_url(project, self.target))? {
            Some(b) => parse(&b, "Modrinth")?,
            None => Vec::new(),
        })
    }

    fn hg_versions(&self, project: &str, platform: &str) -> Result<Vec<HgVersion>> {
        Ok(match self.http.get(&hg_versions_url(project, platform, self.target))? {
            Some(b) => parse::<HgVersions>(&b, "Hangar")?.result,
            None => Vec::new(),
        })
    }

    fn no_version(title: &str) -> String {
        tr!(format!("{title} — нет совместимой версии"), format!("{title} — no compatible version"))
    }

    fn mr_deps(&mut self, deps: &[MrDependency], depth: usize) -> Result<()> {
        for d in deps.iter().filter(|d| d.dependency_type == "required") {
            self.mr_dep(d, depth)?;
        }
        Ok(())
    }

    fn mr_dep(&mut self, d: &MrDependency, depth: usize) -> Result<()> {
        let pinned: Option<MrVersion> = match &d.version_id {
            Some(vid) => {
                let url = format!("{}/version/{}", plugins::MODRINTH_API, enc(vid));
                self.http.get(&url)?.map(|b| parse(&b, "Modrinth")).transpose()?
            }
            None => None,
        };
        let Some(pid) = d.project_id.clone().or_else(|| pinned.as_ref().map(|v| v.project_id.clone())) else {
            let name = d.file_name.clone().unwrap_or_else(|| "?".into());
            self.unresolved.push(tr!(
                format!("{name} — не опубликован на Modrinth"),
                format!("{name} — not published on Modrinth")
            ));
            return Ok(());
        };
        if !self.visited.insert(format!("mr:{pid}")) {
            return Ok(());
        }
        let Some(project) = self.mr_project(&pid)? else {
            self.unresolved.push(Self::no_version(&pid));
            return Ok(());
        };
        if let Some((_, enabled)) = self.installed.find(Some(&pid), &[&project.title, &project.slug]) {
            self.already.push(label_installed(&project.title, *enabled));
            return Ok(());
        }
        if depth > MAX_DEPTH {
            self.unresolved.push(Self::no_version(&project.title));
            return Ok(());
        }
        let version = match pinned.filter(|v| mr_compatible(v, self.target) && mr_file(v, self.target.kind()).is_some())
        {
            Some(v) => Some(v),
            None => pick_modrinth(self.mr_versions(&pid)?, self.target),
        };
        match version.as_ref().and_then(|v| Some((v, mr_planned(v, &project.title)?))) {
            Some((v, file)) => {
                self.deps.push(file);
                self.mr_deps(&v.dependencies, depth + 1)
            }
            None => {
                self.unresolved.push(Self::no_version(&project.title));
                Ok(())
            }
        }
    }

    fn hg_deps(&mut self, v: &HgVersion, platform: &str, depth: usize) -> Result<()> {
        let deps = v.plugin_dependencies.get(platform).cloned().unwrap_or_default();
        for d in deps.iter().filter(|d| d.required) {
            self.hg_dep(d, platform, depth)?;
        }
        Ok(())
    }

    fn hg_dep(&mut self, d: &HgDependency, platform: &str, depth: usize) -> Result<()> {
        let key = match d.project_id {
            Some(id) => format!("hg:{id}"),
            None => format!("hg-name:{}", norm(&d.name)),
        };
        if !self.visited.insert(key) {
            return Ok(());
        }
        if let Some((_, enabled)) = self.installed.find(None, &[&d.name]) {
            self.already.push(label_installed(&d.name, *enabled));
            return Ok(());
        }
        let Some(id) = d.project_id.filter(|_| depth <= MAX_DEPTH) else {
            let tail = d.external_url.as_deref().map(|u| format!(": {u}")).unwrap_or_default();
            self.unresolved.push(tr!(
                format!("{} — установите вручную{tail}", d.name),
                format!("{} — install it manually{tail}", d.name)
            ));
            return Ok(());
        };
        let id = id.to_string();
        let picked = match pick_hangar(self.hg_versions(&id, platform)?, platform, &d.name) {
            Ok(v) => v,
            Err(e) => {
                self.unresolved.push(format!("{e:#}"));
                return Ok(());
            }
        };
        match picked.as_ref().and_then(|v| Some((v, hg_planned(v, platform, &id, &d.name)?))) {
            Some((v, file)) => {
                self.deps.push(file);
                self.hg_deps(v, platform, depth + 1)
            }
            None => {
                self.unresolved.push(Self::no_version(&d.name));
                Ok(())
            }
        }
    }
}

/// Подбирает новейшую совместимую с сервером версию результата поиска и его обязательные зависимости
/// (рекурсивно; уже установленные в `plugins/`/`mods/` пропускаются — по id проекта Modrinth через SHA-1
/// или по имени). `Ok(None)` — совместимой версии нет. Ничего не скачивает.
/// Для датапаков `server_dir` — папка мира (`datapacks::level_dir`).
pub fn resolve(hit: &SearchHit, target: &Target, server_dir: &Path) -> Result<Option<InstallPlan>> {
    let installed = InstalledIndex::load(&Curl, server_dir, target.kind());
    resolve_with(&Curl, hit, target, &installed)
}

pub(crate) fn resolve_with(
    http: &dyn Http,
    hit: &SearchHit,
    target: &Target,
    installed: &InstalledIndex,
) -> Result<Option<InstallPlan>> {
    let mut r = Resolver {
        http,
        target,
        installed,
        visited: HashSet::new(),
        deps: Vec::new(),
        already: Vec::new(),
        unresolved: Vec::new(),
    };
    let main_installed;
    let main = match hit.source {
        Source::Modrinth => {
            r.visited.insert(format!("mr:{}", hit.id));
            main_installed = installed.find(Some(&hit.id), &[&hit.title, &hit.slug]).map(|(p, _)| p.clone());
            let Some(v) = pick_modrinth(r.mr_versions(&hit.id)?, target) else { return Ok(None) };
            let Some(file) = mr_planned(&v, &hit.title) else { return Ok(None) };
            r.mr_deps(&v.dependencies, 1)?;
            file
        }
        Source::Hangar => {
            let Some(platform) = target.platform.hangar() else {
                bail!(tr!(
                    format!("Hangar — только для Paper / Purpur, а сервер на {}", target.platform.label()),
                    format!("Hangar is for Paper / Purpur only, but the server runs {}", target.platform.label())
                ));
            };
            r.visited.insert(format!("hg:{}", hit.id));
            main_installed = installed.find(None, &[&hit.title, &hit.slug]).map(|(p, _)| p.clone());
            let Some(v) = pick_hangar(r.hg_versions(&hit.slug, platform)?, platform, &hit.title)? else {
                return Ok(None);
            };
            let Some(file) = hg_planned(&v, platform, &hit.id, &hit.title) else { return Ok(None) };
            r.hg_deps(&v, platform, 1)?;
            file
        }
    };
    Ok(Some(InstallPlan {
        kind: target.kind(),
        main,
        deps: r.deps,
        already_installed: r.already,
        unresolved: r.unresolved,
        main_installed,
    }))
}

/// План установки известного проекта по slug: сначала Modrinth, затем (для Paper / Purpur) Hangar.
/// Например, `plan_by_slug("chunky", …)` — для прогрузки мира. `Ok(None)` — проекта нет или нет совместимой версии.
pub fn plan_by_slug(slug: &str, target: &Target, server_dir: &Path) -> Result<Option<InstallPlan>> {
    let installed = InstalledIndex::load(&Curl, server_dir, target.kind());
    plan_by_slug_with(&Curl, slug, target, &installed)
}

pub(crate) fn plan_by_slug_with(
    http: &dyn Http,
    slug: &str,
    target: &Target,
    installed: &InstalledIndex,
) -> Result<Option<InstallPlan>> {
    let url = format!("{}/project/{}", plugins::MODRINTH_API, enc(slug));
    let modrinth_err = match http.get(&url) {
        Ok(Some(body)) => {
            let p: MrProject = parse(&body, "Modrinth")?;
            let ptype = if p.project_type.is_empty() { "project".to_string() } else { p.project_type };
            let hit = SearchHit {
                source: Source::Modrinth,
                page: format!("https://modrinth.com/{ptype}/{}", p.slug),
                latest_game_version: newest_game_version(&p.game_versions),
                id: p.id,
                slug: p.slug,
                title: p.title,
                description: p.description,
                author: String::new(),
                downloads: p.downloads,
            };
            match resolve_with(http, &hit, target, installed)? {
                Some(plan) => return Ok(Some(plan)),
                None => None,
            }
        }
        Ok(None) => None,
        Err(e) => Some(e),
    };
    if let Some(platform) = target.platform.hangar() {
        let url = format!("{HANGAR_API}/projects/{}", enc(slug));
        let res = http.get(&url).and_then(|b| b.map(|b| parse::<HgProject>(&b, "Hangar")).transpose());
        match res {
            Ok(Some(p)) => return resolve_with(http, &p.into_hit(platform), target, installed),
            Ok(None) => {}
            Err(e) if modrinth_err.is_some() => return Err(e),
            Err(_) => {}
        }
    }
    match modrinth_err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Установка

#[derive(Debug, Clone, PartialEq)]
pub struct InstallReport {
    /// Новые файлы.
    pub installed: Vec<PathBuf>,
    /// Файлы, которые уже лежали в папке с тем же содержимым.
    pub skipped: Vec<String>,
    /// Напоминание о перезапуске (двуязычное).
    pub note: String,
}

fn digest_file<D: Digest>(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = D::new();
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

/// Самый сильный из известных хэшей файла: (алгоритм, hex).
fn expected_hash(f: &PlannedFile) -> Option<(&'static str, &str)> {
    f.sha512
        .as_deref()
        .map(|h| ("sha512", h))
        .or_else(|| f.sha256.as_deref().map(|h| ("sha256", h)))
        .or_else(|| f.sha1.as_deref().map(|h| ("sha1", h)))
}

fn actual_hash(algo: &str, path: &Path) -> Result<String> {
    match algo {
        "sha512" => digest_file::<Sha512>(path),
        "sha256" => digest_file::<Sha256>(path),
        _ => digest_file::<Sha1>(path),
    }
}

fn verify(f: &PlannedFile, path: &Path, kind: PluginKind) -> Result<()> {
    let Some((algo, expected)) = expected_hash(f) else {
        bail!(tr!(
            format!("{}: у файла нет контрольной суммы", f.file_name),
            format!("{}: the file has no checksum", f.file_name)
        ));
    };
    if let Some(size) = f.size {
        let actual = std::fs::metadata(path)?.len();
        if actual != size {
            bail!(tr!(
                format!("{}: размер не совпал (ожидалось {size}, получено {actual})", f.file_name),
                format!("{}: size mismatch (expected {size}, got {actual})", f.file_name)
            ));
        }
    }
    let actual = actual_hash(algo, path)?;
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!(tr!(
            format!("{}: контрольная сумма не совпала ({algo})", f.file_name),
            format!("{}: checksum mismatch ({algo})", f.file_name)
        ));
    }
    zip::ZipArchive::new(std::fs::File::open(path)?).with_context(|| {
        if kind == PluginKind::Datapack {
            tr!(
                format!("{}: скачанный файл — не zip-архив", f.file_name),
                format!("{}: downloaded file is not a zip archive", f.file_name)
            )
        } else {
            tr!(
                format!("{}: скачанный файл — не jar", f.file_name),
                format!("{}: downloaded file is not a jar", f.file_name)
            )
        }
    })?;
    Ok(())
}

fn same_content(f: &PlannedFile, path: &Path) -> bool {
    expected_hash(f).is_some_and(|(algo, h)| actual_hash(algo, path).is_ok_and(|a| a.eq_ignore_ascii_case(h)))
}

/// Скачивает файлы плана в `<server_dir>/plugins` или `/mods` (для датапаков `server_dir` — папка мира,
/// файлы идут в `<мир>/datapacks`, а временные `.part` — в `<мир>/datapacks.disabled`): каждый — во временный `.part`, проверка размера,
/// хэша (sha512 / sha256 / sha1) и того, что это jar; затем все переносятся на место. Существующий файл
/// с тем же именем и другим содержимым (или его `.jar.disabled` / запись в `datapacks.disabled`) — ошибка до начала скачивания;
/// с тем же содержимым — пропускается. При любой ошибке ничего не остаётся. Действует после перезапуска.
pub fn install(plan: &InstallPlan, server_dir: &Path) -> Result<InstallReport> {
    install_with(plan, server_dir, plugins::download)
}

pub(crate) fn install_with(
    plan: &InstallPlan,
    server_dir: &Path,
    fetch: impl Fn(&str, &Path) -> Result<()>,
) -> Result<InstallReport> {
    let dir = server_dir.join(folder(plan.kind));
    let datapack = plan.kind == PluginKind::Datapack;
    let off_dir = server_dir.join(crate::datapacks::DISABLED_DIR);
    let mut todo: Vec<(&PlannedFile, PathBuf)> = Vec::new();
    let mut skipped = Vec::new();
    let mut names = HashSet::new();
    for f in plan.files() {
        let name = plugins::safe_file_name(&f.file_name, file_ext(plan.kind))?;
        if !names.insert(name.to_ascii_lowercase()) {
            continue;
        }
        let path = dir.join(name);
        if path.exists() {
            if same_content(f, &path) {
                skipped.push(name.to_string());
                continue;
            }
            bail!(tr!(
                format!("файл {name} уже существует (другая версия?)"),
                format!("file {name} already exists (another version?)")
            ));
        }
        if datapack {
            if off_dir.join(name).exists() {
                bail!(tr!(
                    format!("{name} уже лежит выключенным (datapacks.disabled)"),
                    format!("{name} is already present but disabled (datapacks.disabled)")
                ));
            }
        } else if dir.join(format!("{name}{}", plugins::DISABLED_SUFFIX)).exists() {
            bail!(tr!(
                format!("{name} уже лежит выключенным (.disabled)"),
                format!("{name} is already present but disabled (.disabled)")
            ));
        }
        todo.push((f, path));
    }
    std::fs::create_dir_all(&dir).with_context(|| {
        tr!(format!("не удалось создать {}", dir.display()), format!("failed to create {}", dir.display()))
    })?;

    // Датапак в `datapacks/` — только готовые паки: Minecraft пишет в лог каждую лишнюю запись, поэтому
    // временные файлы лежат в `datapacks.disabled/` (та же файловая система — `rename` на место).
    let tmp_dir = if datapack {
        std::fs::create_dir_all(&off_dir).with_context(|| {
            tr!(format!("не удалось создать {}", off_dir.display()), format!("failed to create {}", off_dir.display()))
        })?;
        &off_dir
    } else {
        &dir
    };
    // Без расширения .jar/.zip — сервер не подхватит недокачанный файл.
    let tmps: Vec<PathBuf> =
        (0..todo.len()).map(|i| tmp_dir.join(format!(".pechka-download-{}-{i}.part", std::process::id()))).collect();
    let cleanup = || {
        for t in &tmps {
            let _ = std::fs::remove_file(t);
        }
    };
    for ((f, _), tmp) in todo.iter().zip(&tmps) {
        if let Err(e) = fetch(&f.url, tmp).and_then(|_| verify(f, tmp, plan.kind)) {
            cleanup();
            return Err(e);
        }
    }
    let mut installed = Vec::new();
    for ((f, path), tmp) in todo.iter().zip(&tmps) {
        // Между проверкой и переносом файл мог появиться — не затираем.
        let res = if path.exists() {
            Err(anyhow::anyhow!(tr!(
                format!("файл {} уже существует", f.file_name),
                format!("file {} already exists", f.file_name)
            )))
        } else {
            std::fs::rename(tmp, path).with_context(|| {
                tr!(format!("не удалось записать {}", f.file_name), format!("failed to write {}", f.file_name))
            })
        };
        if let Err(e) = res {
            for p in &installed {
                let _ = std::fs::remove_file(p);
            }
            cleanup();
            return Err(e);
        }
        installed.push(path.clone());
    }
    let note = match plan.kind {
        PluginKind::Plugin => {
            tr!("Перезапустите сервер, чтобы плагины загрузились.", "Restart the server to load the new plugins.")
        }
        PluginKind::Mod => {
            tr!("Перезапустите сервер, чтобы моды загрузились.", "Restart the server to load the new mods.")
        }
        PluginKind::Datapack => tr!(
            "Датапак подхватится после перезагрузки датапаков или перезапуска.",
            "The datapack loads after a datapack reload or a restart."
        ),
    };
    Ok(InstallReport { installed, skipped, note: note.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Write;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "pechka-search-{tag}-{}-{}",
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
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    /// Поддельная сеть: первый маршрут, чья подстрока входит в URL. `None` в ответе — 404, `Err` — сбой.
    #[derive(Default)]
    struct FakeHttp {
        routes: Vec<(&'static str, Result<Option<String>, String>)>,
        log: RefCell<Vec<String>>,
    }
    impl FakeHttp {
        fn route(mut self, pat: &'static str, body: &str) -> Self {
            self.routes.push((pat, Ok(Some(body.to_string()))));
            self
        }
        fn missing(mut self, pat: &'static str) -> Self {
            self.routes.push((pat, Ok(None)));
            self
        }
        fn fail(mut self, pat: &'static str) -> Self {
            self.routes.push((pat, Err("boom".into())));
            self
        }
    }
    impl Http for FakeHttp {
        fn get(&self, url: &str) -> Result<Option<String>> {
            self.log.borrow_mut().push(url.to_string());
            match self.routes.iter().find(|(p, _)| url.contains(p)) {
                Some((_, Ok(b))) => Ok(b.clone()),
                Some((_, Err(e))) => bail!("{e}"),
                None => panic!("unexpected GET {url}"),
            }
        }
        fn post_json(&self, url: &str, _: &serde_json::Value) -> Result<String> {
            self.get(url).map(|b| b.unwrap_or_default())
        }
    }

    fn paper(gv: Option<&str>) -> Target {
        Target::new(Platform::Paper, gv)
    }

    #[test]
    fn facets_and_urls() {
        let f = modrinth_facets(&paper(Some("1.21.4")));
        assert_eq!(
            f,
            serde_json::json!([
                ["project_type:plugin"],
                ["categories:paper", "categories:spigot", "categories:bukkit"],
                ["versions:1.21.4"],
                ["server_side!=unsupported"]
            ])
        );
        let f = modrinth_facets(&Target::new(Platform::Quilt, Some("  ")));
        assert_eq!(
            f,
            serde_json::json!([
                ["project_type:mod"],
                ["categories:quilt", "categories:fabric"],
                ["server_side!=unsupported"]
            ])
        );
        let url = modrinth_search_url("world edit", &paper(None), 20);
        assert!(url.starts_with("https://api.modrinth.com/v2/search?query=world%20edit&limit=20&offset=20&"));
        assert!(url.contains("facets=%5B%5B%22project_type%3Aplugin%22%5D"), "{url}");
        assert_eq!(
            hangar_search_url("chunky", "PAPER", &paper(Some("26.3")), 0),
            "https://hangar.papermc.io/api/v1/projects?limit=20&offset=0&platform=PAPER&q=chunky&version=26.3"
        );
        assert!(!hangar_search_url(" ", "PAPER", &paper(None), 0).contains("q="));
        assert_eq!(enc("a+b/ж"), "a%2Bb%2F%D0%B6");
        assert_eq!(norm("Fabric API"), norm("fabric-api"));
        assert_eq!(
            newest_game_version(&["1.21.4".into(), "1.21.10".into(), "25w14a".into(), "1.9".into()]),
            Some("1.21.10".into())
        );
        assert_eq!(Platform::Spigot.hangar(), None);
        assert_eq!(Platform::Purpur.hangar(), Some("PAPER"));
        assert_eq!(Platform::Fabric.kind(), PluginKind::Mod);
    }

    /// Урезанный реальный ответ `GET /v2/search` (ноябрь 2026).
    const MR_SEARCH: &str = r#"{"hits":[
      {"project_id":"fALzjamp","project_type":"mod","all_project_types":["mod","plugin"],"slug":"chunky",
       "author":"pop4959","title":"Chunky","description":"Pre-generates chunks, quickly and efficiently",
       "categories":["bukkit","fabric","paper"],"versions":["1.21.10","1.21.2","26.3","26.2"],"downloads":20216856,
       "icon_url":"https://cdn.modrinth.com/x.webp","latest_version":"RBuKjN4k","server_side":"optional"},
      {"project_id":"s86X568j","project_type":"mod","slug":"chunkyborder","author":"pop4959","title":"Chunky Border",
       "description":"An add-on for Chunky","versions":["26.3"],"downloads":393121}
    ],"offset":0,"limit":20,"total_hits":9}"#;

    /// Урезанный реальный ответ `GET /api/v1/projects` Hangar (ноябрь 2026).
    const HG_SEARCH: &str = r#"{"pagination":{"count":283,"limit":20,"offset":0},"result":[
      {"createdAt":"2022-12-22T20:52:18.999416Z","id":4,"name":"Chunky","namespace":{"owner":"pop4959","slug":"Chunky"},
       "stats":{"views":267320,"downloads":81431,"recentViews":8349,"recentDownloads":2737,"stars":310,"watchers":158},
       "category":"world_management","description":"Pre-generates chunks, quickly and efficiently",
       "lastUpdated":"2026-11-04T03:08:16.596507Z","visibility":"public","userActions":null,"settings":{"tags":[]},
       "supportedPlatforms":{"PAPER":["1.18","1.21.10","26.3"]},"avatarUrl":"https://hangarcdn.papermc.io/a.webp"},
      {"id":12,"name":"ViaBackwards","namespace":{"owner":"ViaVersion","slug":"ViaBackwards"},
       "stats":{"downloads":500000},"description":"Allows older clients to connect","supportedPlatforms":{}}
    ]}"#;

    #[test]
    fn parses_search_responses() {
        let (hits, total) = parse_modrinth_search(MR_SEARCH).unwrap();
        assert_eq!(total, 9);
        assert_eq!(hits.len(), 2);
        let h = &hits[0];
        assert_eq!(h.source, Source::Modrinth);
        assert_eq!((h.id.as_str(), h.slug.as_str(), h.title.as_str()), ("fALzjamp", "chunky", "Chunky"));
        assert_eq!(h.author, "pop4959");
        assert_eq!(h.downloads, 20216856);
        assert_eq!(h.latest_game_version.as_deref(), Some("26.3"));
        assert_eq!(h.page, "https://modrinth.com/mod/chunky");

        let (hits, total) = parse_hangar_search(HG_SEARCH, "PAPER").unwrap();
        assert_eq!(total, 283);
        let h = &hits[0];
        assert_eq!(h.source, Source::Hangar);
        assert_eq!((h.id.as_str(), h.slug.as_str(), h.author.as_str()), ("4", "Chunky", "pop4959"));
        assert_eq!(h.downloads, 81431);
        assert_eq!(h.latest_game_version.as_deref(), Some("26.3"));
        assert_eq!(h.page, "https://hangar.papermc.io/pop4959/Chunky");
        assert_eq!(hits[1].latest_game_version, None);
        assert!(parse_modrinth_search("[]").is_err());
        assert!(parse_modrinth_search("").is_err());
        assert!(parse_hangar_search("{}", "PAPER").is_err());
    }

    #[test]
    fn merge_prefers_modrinth() {
        let (mr, _) = parse_modrinth_search(MR_SEARCH).unwrap();
        let (hg, _) = parse_hangar_search(HG_SEARCH, "PAPER").unwrap();
        let merged = merge_hits(mr, hg);
        let ids: Vec<(Source, &str)> = merged.iter().map(|h| (h.source, h.title.as_str())).collect();
        assert_eq!(
            ids,
            [(Source::Modrinth, "Chunky"), (Source::Modrinth, "Chunky Border"), (Source::Hangar, "ViaBackwards")]
        );
    }

    #[test]
    fn search_partial_failure() {
        let http = FakeHttp::default().route("api.modrinth.com/v2/search", MR_SEARCH).fail("hangar.papermc.io");
        let r = search_with(&http, "chunky", &paper(Some("26.3")), 0).unwrap();
        assert_eq!(r.hits.len(), 2);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("Hangar"), "{:?}", r.warnings);
        assert!(!r.more);

        let http = FakeHttp::default().fail("api.modrinth.com").route("hangar.papermc.io", HG_SEARCH);
        let r = search_with(&http, "chunky", &paper(None), 0).unwrap();
        assert_eq!(r.hits.len(), 2);
        assert!(r.more, "Hangar has 283 results");
        assert!(r.warnings[0].contains("Modrinth"));

        let http = FakeHttp::default().fail("api.modrinth.com").fail("hangar.papermc.io");
        assert!(search_with(&http, "x", &paper(None), 0).is_err());

        // Fabric: Hangar не спрашиваем, а сбой Modrinth — ошибка.
        let http = FakeHttp::default().route("api.modrinth.com/v2/search", MR_SEARCH);
        let r = search_with(&http, "", &Target::new(Platform::Fabric, None), 0).unwrap();
        assert!(r.warnings.is_empty());
        assert!(http.log.borrow().iter().all(|u| !u.contains("hangar")));
        let http = FakeHttp::default().fail("api.modrinth.com");
        assert!(search_with(&http, "", &Target::new(Platform::Fabric, None), 0).is_err());
    }

    fn mr_version(id: &str, project: &str, date: &str, vtype: &str, loaders: &[&str], deps: &str) -> String {
        format!(
            r#"{{"id":"{id}","project_id":"{project}","version_number":"{id}-ver","date_published":"{date}",
               "version_type":"{vtype}","loaders":{},"game_versions":["1.21.4"],
               "files":[{{"hashes":{{"sha1":"AA{id}","sha512":"BB{id}"}},"url":"https://cdn.modrinth.com/{id}.jar",
                         "filename":"{id}.jar","primary":true,"size":123}}],
               "dependencies":{deps}}}"#,
            serde_json::json!(loaders)
        )
    }

    #[test]
    fn picks_modrinth_version() {
        let t = paper(None);
        let list = format!(
            "[{},{},{}]",
            mr_version("beta2", "p", "2026-03-01T00:00:00Z", "beta", &["paper"], "[]"),
            mr_version("rel1", "p", "2026-02-01T00:00:00Z", "release", &["paper"], "[]"),
            mr_version("folia", "p", "2026-04-01T00:00:00Z", "release", &["folia"], "[]"),
        );
        let vs: Vec<MrVersion> = serde_json::from_str(&list).unwrap();
        let v = pick_modrinth(vs.clone(), &t).unwrap();
        assert_eq!(v.id, "rel1", "release preferred, folia-only skipped");
        let f = mr_planned(&v, "P").unwrap();
        assert_eq!(f.sha1.as_deref(), Some("aarel1"));
        assert_eq!(f.sha512.as_deref(), Some("bbrel1"));
        assert_eq!(f.size, Some(123));
        let only_beta: Vec<MrVersion> = vs.into_iter().filter(|v| v.id == "beta2").collect();
        assert_eq!(pick_modrinth(only_beta, &t).unwrap().id, "beta2");
        assert!(pick_modrinth(Vec::new(), &t).is_none());
    }

    /// Урезанный реальный ответ `GET /api/v1/projects/{slug}/versions` Hangar.
    const HG_VERSIONS: &str = r#"{"pagination":{"count":3,"limit":25,"offset":0},"result":[
      {"createdAt":"2026-11-07T09:38:30Z","id":3,"projectId":12,"name":"5.1-SNAPSHOT","channel":{"name":"Snapshot"},
       "downloads":{"PAPER":{"fileInfo":{"name":"ViaBackwards-5.1-SNAPSHOT.jar","sizeBytes":10,"sha256Hash":"cc"},
         "externalUrl":null,"downloadUrl":"https://hangarcdn.papermc.io/snap.jar"}},
       "pluginDependencies":{"PAPER":[{"name":"ViaVersion","projectId":31,"required":true,"externalUrl":null,"platform":"PAPER"}]},
       "platformDependencies":{"PAPER":["26.3"]}},
      {"createdAt":"2026-10-27T19:47:24Z","id":2,"projectId":12,"name":"5.0","channel":{"name":"Release"},
       "downloads":{"PAPER":{"fileInfo":{"name":"ViaBackwards-5.0.jar","sizeBytes":20,"sha256Hash":"DD"},
         "externalUrl":null,"downloadUrl":"https://hangarcdn.papermc.io/rel.jar"}},
       "pluginDependencies":{"PAPER":[
         {"name":"ViaVersion","projectId":31,"required":true,"externalUrl":null,"platform":"PAPER"},
         {"name":"ViaRewind","projectId":112,"required":false,"externalUrl":null,"platform":"PAPER"},
         {"name":"Vault","projectId":null,"required":true,"externalUrl":"https://www.spigotmc.org/resources/vault.34315/","platform":"PAPER"}]}},
      {"createdAt":"2026-01-01T00:00:00Z","id":1,"projectId":12,"name":"4.0","channel":{"name":"Release"},
       "downloads":{"PAPER":{"fileInfo":null,"externalUrl":"https://example.org/dl","downloadUrl":null}}}
    ]}"#;

    #[test]
    fn picks_hangar_version() {
        let vs: HgVersions = serde_json::from_str(HG_VERSIONS).unwrap();
        let v = pick_hangar(vs.result.clone(), "PAPER", "VB").unwrap().unwrap();
        assert_eq!(v.name, "5.0", "Release channel preferred over a newer snapshot");
        let f = hg_planned(&v, "PAPER", "12", "ViaBackwards").unwrap();
        assert_eq!(f.file_name, "ViaBackwards-5.0.jar");
        assert_eq!(f.sha256.as_deref(), Some("dd"));
        assert_eq!(f.size, Some(20));
        assert_eq!(f.url, "https://hangarcdn.papermc.io/rel.jar");

        let external: Vec<HgVersion> = vs.result.iter().filter(|v| v.name == "4.0").cloned().collect();
        let err = pick_hangar(external, "PAPER", "VB").unwrap_err();
        assert!(format!("{err:#}").contains("https://example.org/dl"));
        assert!(pick_hangar(Vec::new(), "PAPER", "VB").unwrap().is_none());
        assert!(pick_hangar(vs.result, "VELOCITY", "VB").unwrap().is_none());
    }

    fn mr_project(id: &str, slug: &str, title: &str) -> String {
        format!(r#"{{"id":"{id}","slug":"{slug}","title":"{title}","project_type":"mod","downloads":1}}"#)
    }

    #[test]
    fn resolves_modrinth_dependencies() {
        // root → A (нужен) → B (установлен по имени), C (не опубликован), root (цикл); root → opt (необязательный).
        let dep =
            |pid: &str, kind: &str| format!(r#"{{"project_id":"{pid}","version_id":null,"dependency_type":"{kind}"}}"#);
        let root_deps = format!("[{},{}]", dep("AAA", "required"), dep("OPT", "optional"));
        let a_deps = format!(
            r#"[{},{},{},{{"project_id":null,"version_id":null,"file_name":"lib.jar","dependency_type":"required"}}]"#,
            dep("BBB", "required"),
            dep("CCC", "required"),
            dep("ROOT", "required")
        );
        let http = FakeHttp::default()
            .route(
                "/project/ROOT/version",
                &format!(
                    "[{}]",
                    mr_version("root1", "ROOT", "2026-01-01T00:00:00Z", "release", &["paper"], &root_deps)
                ),
            )
            .route(
                "/project/AAA/version",
                &format!("[{}]", mr_version("a1", "AAA", "2026-01-01T00:00:00Z", "release", &["spigot"], &a_deps)),
            )
            .route("/project/CCC/version", "[]")
            .route("/project/AAA", &mr_project("AAA", "lib-a", "Lib A"))
            .route("/project/BBB", &mr_project("BBB", "fabric-api", "Fabric API"))
            .route("/project/CCC", &mr_project("CCC", "lib-c", "Lib C"));
        let mut installed = InstalledIndex::default();
        installed.names.insert(norm("FabricAPI"), (PathBuf::from("/x/fabric-api.jar.disabled"), false));
        let hit = SearchHit {
            source: Source::Modrinth,
            id: "ROOT".into(),
            slug: "root".into(),
            title: "Root".into(),
            description: String::new(),
            author: String::new(),
            downloads: 0,
            latest_game_version: None,
            page: String::new(),
        };
        let plan = resolve_with(&http, &hit, &paper(Some("1.21.4")), &installed).unwrap().unwrap();
        assert_eq!(plan.main.file_name, "root1.jar");
        assert_eq!(plan.main.title, "Root");
        assert_eq!(plan.deps.iter().map(|f| f.title.as_str()).collect::<Vec<_>>(), ["Lib A"]);
        assert_eq!(plan.already_installed.len(), 1);
        assert!(plan.already_installed[0].starts_with("Fabric API ("), "disabled is marked");
        assert_eq!(plan.unresolved.len(), 2, "{:?}", plan.unresolved);
        assert!(plan.unresolved[0].starts_with("Lib C"));
        assert!(plan.unresolved[1].starts_with("lib.jar"));
        assert_eq!(plan.main_installed, None);
        assert_eq!(plan.total_size(), 246);
        assert_eq!(plan.files().last().unwrap().title, "Root");
        let log = http.log.borrow();
        assert!(log.iter().all(|u| !u.contains("OPT")), "optional deps are not fetched");
        assert!(log[0].contains("loaders=%5B%22paper%22%2C%22spigot%22%2C%22bukkit%22%5D"), "{}", log[0]);
        assert!(log[0].contains("game_versions=%5B%221.21.4%22%5D"));

        // Нет совместимой версии → None.
        let http = FakeHttp::default().route("/project/ROOT/version", "[]");
        assert!(resolve_with(&http, &hit, &paper(None), &installed).unwrap().is_none());
        // Сам проект уже стоит (по id Modrinth).
        installed.projects.insert("ROOT".into(), (PathBuf::from("/x/root.jar"), true));
        let http = FakeHttp::default().route(
            "/project/ROOT/version",
            &format!("[{}]", mr_version("root1", "ROOT", "2026-01-01T00:00:00Z", "release", &["paper"], "[]")),
        );
        let plan = resolve_with(&http, &hit, &paper(None), &installed).unwrap().unwrap();
        assert_eq!(plan.main_installed, Some(PathBuf::from("/x/root.jar")));
    }

    #[test]
    fn resolves_hangar_dependencies() {
        let via_versions = r#"{"result":[{"createdAt":"2026-10-27T00:00:00Z","name":"5.12.2","channel":{"name":"Release"},
            "downloads":{"PAPER":{"fileInfo":{"name":"ViaVersion-5.12.2.jar","sizeBytes":5,"sha256Hash":"ee"},
            "downloadUrl":"https://hangarcdn.papermc.io/vv.jar"}},
            "pluginDependencies":{"PAPER":[{"name":"ViaBackwards","projectId":12,"required":true}]}}]}"#;
        let http = FakeHttp::default()
            .route("/projects/ViaBackwards/versions", HG_VERSIONS)
            .route("/projects/31/versions", via_versions);
        let hit = SearchHit {
            source: Source::Hangar,
            id: "12".into(),
            slug: "ViaBackwards".into(),
            title: "ViaBackwards".into(),
            description: String::new(),
            author: "ViaVersion".into(),
            downloads: 0,
            latest_game_version: None,
            page: String::new(),
        };
        let plan = resolve_with(&http, &hit, &paper(Some("26.3")), &InstalledIndex::default()).unwrap().unwrap();
        assert_eq!(plan.main.file_name, "ViaBackwards-5.0.jar");
        assert_eq!(plan.deps.len(), 1);
        assert_eq!(plan.deps[0].file_name, "ViaVersion-5.12.2.jar");
        assert_eq!(plan.deps[0].project_id, "31");
        assert_eq!(plan.unresolved.len(), 1);
        assert!(plan.unresolved[0].contains("Vault") && plan.unresolved[0].contains("spigotmc.org"));
        assert!(http.log.borrow()[0].contains("platformVersion=26.3"));

        // Зависимость уже установлена → не качаем.
        let mut installed = InstalledIndex::default();
        installed.names.insert(norm("ViaVersion"), (PathBuf::from("/x/ViaVersion.jar"), true));
        let plan = resolve_with(&http, &hit, &paper(None), &installed).unwrap().unwrap();
        assert!(plan.deps.is_empty());
        assert_eq!(plan.already_installed, ["ViaVersion"]);

        // Hangar-результат на Fabric — ошибка.
        assert!(resolve_with(&http, &hit, &Target::new(Platform::Fabric, None), &installed).is_err());
    }

    #[test]
    fn plan_by_slug_falls_back_to_hangar() {
        let hg_project =
            r#"{"id":4,"name":"Chunky","namespace":{"owner":"pop4959","slug":"Chunky"},"stats":{"downloads":1}}"#;
        let hg_versions = r#"{"result":[{"createdAt":"2026-11-04T00:00:00Z","name":"1.5.66","channel":{"name":"Release"},
            "downloads":{"PAPER":{"fileInfo":{"name":"Chunky-Paper-1.5.66.jar","sizeBytes":1,"sha256Hash":"ab"},
            "downloadUrl":"https://hangarcdn.papermc.io/c.jar"}}}]}"#;
        let http = FakeHttp::default()
            .missing("api.modrinth.com/v2/project/chunky")
            .route("/projects/Chunky/versions", hg_versions)
            .route("hangar.papermc.io/api/v1/projects/chunky", hg_project);
        let plan = plan_by_slug_with(&http, "chunky", &paper(None), &InstalledIndex::default()).unwrap().unwrap();
        assert_eq!(plan.main.source, Source::Hangar);
        assert_eq!(plan.main.file_name, "Chunky-Paper-1.5.66.jar");

        let http = FakeHttp::default()
            .route(
                "api.modrinth.com/v2/project/fALzjamp/version",
                &format!("[{}]", mr_version("c1", "fALzjamp", "2026-01-01T00:00:00Z", "release", &["fabric"], "[]")),
            )
            .route("api.modrinth.com/v2/project/chunky", &mr_project("fALzjamp", "chunky", "Chunky"));
        let t = Target::new(Platform::Fabric, Some("1.21.4"));
        let plan = plan_by_slug_with(&http, "chunky", &t, &InstalledIndex::default()).unwrap().unwrap();
        assert_eq!(plan.main.source, Source::Modrinth);
        assert_eq!(plan.kind, PluginKind::Mod);

        let http = FakeHttp::default().missing("api.modrinth.com");
        assert!(plan_by_slug_with(&http, "nope", &t, &InstalledIndex::default()).unwrap().is_none());
        let http = FakeHttp::default().fail("api.modrinth.com");
        assert!(plan_by_slug_with(&http, "nope", &t, &InstalledIndex::default()).is_err());
    }

    fn planned(src: &Path, name: &str) -> PlannedFile {
        PlannedFile {
            source: Source::Modrinth,
            project_id: name.into(),
            title: name.into(),
            version: "1".into(),
            file_name: format!("{name}.jar"),
            url: format!("https://cdn.modrinth.com/{name}.jar"),
            sha1: Some(digest_file::<Sha1>(src).unwrap()),
            sha512: Some(digest_file::<Sha512>(src).unwrap().to_uppercase()),
            sha256: None,
            size: Some(std::fs::metadata(src).unwrap().len()),
        }
    }

    fn zip_version(id: &str, filename: &str, loaders: &[&str]) -> String {
        format!(
            r#"{{"id":"{id}","project_id":"DP","version_number":"{id}-ver","date_published":"2026-01-01T00:00:00Z",
               "version_type":"release","loaders":{},"game_versions":["26.3"],
               "files":[{{"hashes":{{"sha1":"AA{id}","sha512":"BB{id}"}},"url":"https://cdn.modrinth.com/{filename}",
                         "filename":"{filename}","primary":true,"size":123}}],"dependencies":[]}}"#,
            serde_json::json!(loaders)
        )
    }

    fn datapack_hit() -> SearchHit {
        SearchHit {
            source: Source::Modrinth,
            id: "DP".into(),
            slug: "dp".into(),
            title: "DP".into(),
            description: String::new(),
            author: String::new(),
            downloads: 0,
            latest_game_version: None,
            page: String::new(),
        }
    }

    #[test]
    fn datapack_facets_and_no_hangar() {
        let t = Target::new(Platform::Datapack, Some("26.3"));
        let url = modrinth_search_url("", &t, 0);
        let decoded = url.replace("%3A", ":").replace("%22", "\"");
        for want in ["project_type:datapack", "categories:datapack", "versions:26.3"] {
            assert!(decoded.contains(want), "{want} not in {decoded}");
        }
        assert_eq!(
            modrinth_facets(&t).as_array().unwrap().last().unwrap(),
            &serde_json::json!(["server_side!=unsupported"])
        );
        assert_eq!(Platform::Datapack.label(), "Minecraft");
        assert_eq!(Platform::Datapack.kind(), PluginKind::Datapack);
        assert_eq!(Platform::Datapack.loaders(), crate::datapacks::DATAPACK_LOADERS);
        assert_eq!(Platform::Datapack.hangar(), None);
        assert_eq!(folder(PluginKind::Datapack), "datapacks");

        let http = FakeHttp::default().route("api.modrinth.com/v2/search", MR_SEARCH);
        let r = search_with(&http, "", &t, 0).unwrap();
        assert!(r.warnings.is_empty());
        assert_eq!(http.log.borrow().len(), 1);
    }

    #[test]
    fn datapack_resolve_picks_zip() {
        let list =
            format!("[{},{}]", zip_version("a", "x.jar", &["datapack"]), zip_version("b", "x-1.0.zip", &["datapack"]));
        let http = FakeHttp::default().route("/project/DP/version", &list);
        let t = Target::new(Platform::Datapack, Some("26.3"));
        let plan = resolve_with(&http, &datapack_hit(), &t, &InstalledIndex::default()).unwrap().unwrap();
        assert_eq!(plan.main.file_name, "x-1.0.zip");
        assert_eq!(plan.kind, PluginKind::Datapack);
        let url = http.log.borrow()[0].clone();
        assert!(url.contains(&enc(r#"["datapack"]"#)), "{url}");
    }

    fn datapack_plan(src: &Path) -> InstallPlan {
        let mut main = planned(src, "x-1.0");
        main.file_name = "x-1.0.zip".into();
        InstallPlan {
            kind: PluginKind::Datapack,
            main,
            deps: vec![],
            already_installed: vec![],
            unresolved: vec![],
            main_installed: None,
        }
    }

    #[test]
    fn install_datapack_creates_world_dirs() {
        let t = TempDir::new("dp-install");
        let src = t.0.join("src").join("x-1.0.zip");
        make_jar(&src, &[("pack.mcmeta", "{}")]);
        let plan = datapack_plan(&src);
        let world = t.0.join("world");
        let seen = RefCell::new(Vec::new());
        let fetch = |_: &str, dest: &Path| -> Result<()> {
            seen.borrow_mut().push(dest.to_path_buf());
            std::fs::copy(&src, dest)?;
            Ok(())
        };
        let r = install_with(&plan, &world, fetch).unwrap();
        assert_eq!(r.installed, [world.join("datapacks").join("x-1.0.zip")]);
        assert!(world.join("datapacks").join("x-1.0.zip").is_file());
        assert!(r.note.contains("datapack") || r.note.contains("датапак"));
        // Временные файлы — в datapacks.disabled/, а не в datapacks/; после установки их нет нигде.
        let seen = seen.borrow();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with(world.join("datapacks.disabled")), "{:?}", seen[0]);
        for d in ["datapacks", "datapacks.disabled"] {
            let parts = std::fs::read_dir(world.join(d))
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
                .count();
            assert_eq!(parts, 0);
        }
        let names: Vec<_> = std::fs::read_dir(world.join("datapacks")).unwrap().flatten().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn install_datapack_refuses_disabled_twin() {
        let t = TempDir::new("dp-twin");
        let src = t.0.join("src").join("x-1.0.zip");
        make_jar(&src, &[("pack.mcmeta", "{}")]);
        let plan = datapack_plan(&src);
        let world = t.0.join("world");
        std::fs::create_dir_all(world.join("datapacks.disabled").join("x-1.0.zip")).unwrap();
        let fetch = |_: &str, _: &Path| -> Result<()> { panic!("nothing must be downloaded") };
        let e = install_with(&plan, &world, fetch).unwrap_err();
        assert!(format!("{e:#}").contains("datapacks.disabled"), "{e:#}");
        assert!(!world.join("datapacks").join("x-1.0.zip").exists());
    }

    #[test]
    fn install_offline() {
        let t = TempDir::new("install");
        let src_dir = t.0.join("src");
        let a = src_dir.join("a.jar");
        let b = src_dir.join("b.jar");
        make_jar(&a, &[("plugin.yml", "name: A\n")]);
        make_jar(&b, &[("plugin.yml", "name: B\n")]);
        let server = t.0.join("server");
        std::fs::create_dir_all(&server).unwrap();
        let fetch = |url: &str, dest: &Path| -> Result<()> {
            let name = url.rsplit('/').next().unwrap();
            std::fs::copy(src_dir.join(name), dest)?;
            Ok(())
        };
        let mut plan = InstallPlan {
            kind: PluginKind::Plugin,
            main: planned(&a, "a"),
            deps: vec![planned(&b, "b")],
            already_installed: vec![],
            unresolved: vec![],
            main_installed: None,
        };
        let plugins_dir = server.join("plugins");
        let leftovers = || {
            std::fs::read_dir(&plugins_dir)
                .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".part")).count())
                .unwrap_or(0)
        };

        // Неверный хэш зависимости → ничего не установлено.
        plan.deps[0].sha512 = Some("00".into());
        assert!(install_with(&plan, &server, fetch).is_err());
        assert!(!plugins_dir.join("a.jar").exists() && !plugins_dir.join("b.jar").exists());
        assert_eq!(leftovers(), 0);

        // Небезопасное имя.
        let mut bad = plan.clone();
        bad.main.file_name = "../evil.jar".into();
        assert!(install_with(&bad, &server, fetch).is_err());
        assert!(!server.join("evil.jar").exists());

        // Успех: оба файла, сначала зависимость.
        plan.deps[0] = planned(&b, "b");
        let r = install_with(&plan, &server, fetch).unwrap();
        assert_eq!(r.installed, [plugins_dir.join("b.jar"), plugins_dir.join("a.jar")]);
        assert!(r.skipped.is_empty());
        assert!(!r.note.is_empty());
        assert_eq!(plugins::scan(&server).len(), 2);
        assert_eq!(leftovers(), 0);

        // Повтор: те же файлы → пропуск.
        let r = install_with(&plan, &server, fetch).unwrap();
        assert!(r.installed.is_empty());
        assert_eq!(r.skipped, ["b.jar", "a.jar"]);

        // Тот же файл, другое содержимое → отказ, существующий не тронут.
        let mut other = plan.clone();
        other.main = planned(&b, "b");
        other.main.file_name = "a.jar".into();
        other.deps.clear();
        let before = std::fs::read(plugins_dir.join("a.jar")).unwrap();
        assert!(install_with(&other, &server, fetch).is_err());
        assert_eq!(std::fs::read(plugins_dir.join("a.jar")).unwrap(), before);

        // Выключенная копия — тоже отказ.
        std::fs::rename(plugins_dir.join("a.jar"), plugins_dir.join("a.jar.disabled")).unwrap();
        plan.deps.clear();
        assert!(install_with(&plan, &server, fetch).is_err());

        // Без контрольной суммы не ставим.
        let mut nohash = plan.clone();
        nohash.main = planned(&b, "c");
        nohash.main.url = "https://cdn.modrinth.com/b.jar".into();
        nohash.main.sha1 = None;
        nohash.main.sha512 = None;
        assert!(install_with(&nohash, &server, fetch).is_err());
        assert!(!plugins_dir.join("c.jar").exists());
        assert_eq!(leftovers(), 0);
    }

    #[test]
    fn installed_index_by_name_and_hash() {
        let t = TempDir::new("index");
        make_jar(&t.0.join("plugins").join("ViaVersion-5.0.jar"), &[("plugin.yml", "name: ViaVersion\n")]);
        make_jar(&t.0.join("plugins").join("Old.jar.disabled"), &[("plugin.yml", "name: Old Thing\n")]);
        make_jar(&t.0.join("mods").join("sodium.jar"), &[]);
        let sha = plugins::sha1_file(&t.0.join("plugins").join("ViaVersion-5.0.jar")).unwrap();
        let resp = format!(
            r#"{{"{sha}":{{"id":"v","project_id":"P123","version_number":"5.0","files":[{{"url":"https://x/y.jar","filename":"y.jar"}}]}}}}"#
        );
        let http = FakeHttp { routes: vec![("version_files", Ok(Some(resp)))], ..Default::default() };
        let idx = InstalledIndex::load(&http, &t.0, PluginKind::Plugin);
        assert!(idx.find(Some("P123"), &[]).is_some());
        assert!(idx.find(None, &["via-version"]).is_some());
        assert!(idx.find(None, &["ViaVersion-5.0"]).is_some());
        assert_eq!(idx.find(None, &["old thing"]).map(|(_, e)| *e), Some(false));
        assert!(idx.find(None, &["sodium"]).is_none(), "mods are not plugins");
        assert!(idx.find(Some("nope"), &["nope"]).is_none());
        // Сбой сети — только имена.
        let http = FakeHttp::default().fail("version_files");
        let idx = InstalledIndex::load(&http, &t.0, PluginKind::Plugin);
        assert!(idx.projects.is_empty());
        assert!(idx.find(None, &["ViaVersion"]).is_some());
    }

    #[test]
    fn detects_platform() {
        let t = TempDir::new("detect");
        assert_eq!(Platform::detect(&t.0, ""), None);
        std::fs::create_dir_all(t.0.join("mods")).unwrap();
        assert_eq!(Platform::detect(&t.0, ""), Some(Platform::Fabric));
        assert_eq!(Platform::detect(&t.0, "java -jar neoforge-21.1.jar"), Some(Platform::NeoForge));
        std::fs::write(t.0.join("paper-1.21.4-100.jar"), b"").unwrap();
        assert_eq!(Platform::detect(&t.0, ""), Some(Platform::Paper));
        std::fs::write(t.0.join("purpur.yml"), b"").unwrap();
        let tg = Target::detect(&t.0, "", Some(" 1.21.4 ")).unwrap();
        assert_eq!(tg, Target { platform: Platform::Purpur, game_version: Some("1.21.4".into()) });
        assert_eq!(folder(tg.kind()), "plugins");
    }

    /// `cargo test -- --ignored plugin_search`: живые Modrinth и Hangar.
    #[test]
    #[ignore = "network"]
    fn plugin_search_network() {
        let paper = Target::new(Platform::Paper, Some("26.3"));
        let r = search("chunky", &paper, 0).unwrap();
        eprintln!("warnings: {:?}, more: {}", r.warnings, r.more);
        for h in &r.hits {
            eprintln!(
                "{:?} {} [{}] by {} ↓{} mc≤{:?}",
                h.source, h.title, h.slug, h.author, h.downloads, h.latest_game_version
            );
        }
        assert!(r.warnings.is_empty());
        let chunky: Vec<_> = r.hits.iter().filter(|h| norm(&h.title) == "chunky").collect();
        assert_eq!(chunky.len(), 1, "deduped");
        assert_eq!(chunky[0].source, Source::Modrinth);
        assert!(r.hits.iter().any(|h| h.source == Source::Hangar), "Hangar-only results are merged in");
        let page2 = search("chunky", &paper, PAGE_SIZE).unwrap();
        assert!(!page2.hits.is_empty());

        let t = TempDir::new("net");
        let plan = resolve(chunky[0], &paper, &t.0).unwrap().expect("Chunky for Paper 26.3");
        eprintln!("plan: {plan:#?}");
        assert!(plan.main.file_name.to_lowercase().contains("paper") || plan.main.file_name.contains("Bukkit"));
        let rep = install(&plan, &t.0).unwrap();
        assert_eq!(rep.installed.len(), 1);
        let list = plugins::scan(&t.0);
        assert_eq!(list[0].name, "Chunky");
        // Теперь он «уже установлен» (по SHA-1 через Modrinth).
        let again = plan_by_slug("chunky", &paper, &t.0).unwrap().unwrap();
        assert_eq!(again.main_installed.as_deref(), Some(rep.installed[0].as_path()));
        assert!(install(&again, &t.0).unwrap().skipped.len() == 1);

        // Hangar с зависимостью: ViaBackwards → ViaVersion.
        let vb = search("ViaBackwards", &paper, 0).unwrap();
        let hit = vb.hits.iter().find(|h| h.slug.eq_ignore_ascii_case("viabackwards")).expect("ViaBackwards");
        let plan = resolve(hit, &paper, &t.0).unwrap().expect("ViaBackwards plan");
        eprintln!("viabackwards ({:?}): deps {:?}", hit.source, plan.deps.iter().map(|d| &d.title).collect::<Vec<_>>());
        assert!(plan.deps.iter().any(|d| norm(&d.title) == "viaversion"), "{plan:#?}");
        let hg = Target::new(Platform::Paper, None);
        let hg_hit = SearchHit { source: Source::Hangar, id: "12".into(), ..hit.clone() };
        let plan = resolve(&hg_hit, &hg, &t.0).unwrap().expect("ViaBackwards on Hangar");
        assert_eq!(plan.main.source, Source::Hangar);
        assert!(plan.main.sha256.is_some());
        assert!(plan.deps.iter().any(|d| d.source == Source::Hangar && norm(&d.title) == "viaversion"), "{plan:#?}");

        // Мод Fabric с обязательной зависимостью.
        let fabric = Target::new(Platform::Fabric, Some("1.21.4"));
        let r = search("lithium", &fabric, 0).unwrap();
        assert!(!r.hits.is_empty());
        let plan = plan_by_slug("sodium-extra", &fabric, &t.0).unwrap().expect("sodium-extra");
        eprintln!("sodium-extra deps: {:?}", plan.deps.iter().map(|d| (&d.title, &d.file_name)).collect::<Vec<_>>());
        assert!(plan.deps.iter().any(|d| d.title.to_lowercase().contains("sodium")));
        let chunky_fabric = plan_by_slug("chunky", &fabric, &t.0).unwrap().expect("chunky fabric");
        assert!(chunky_fabric.main.file_name.to_lowercase().contains("fabric"));
        eprintln!("chunky fabric: {chunky_fabric:#?}");
    }
}
