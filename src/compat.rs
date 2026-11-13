#![allow(dead_code)] // UI integration pending
//! Совместимость плагинов / модов с другой версией Minecraft — перед обновлением ядра.
//!
//! [`check`] смотрит, у каких включённых jar в `plugins/` (Paper / Purpur) или `mods/` (Fabric) есть сборка
//! под целевую версию:
//! - Modrinth (по SHA-1, пакетно): `version_files/update` с фильтром по версии игры и загрузчикам ядра —
//!   найденные хэши готовы; для остальных `version_files` говорит, знает ли Modrinth файл вообще
//!   (знает — сборки нет, не знает — пробуем Hangar);
//! - Hangar (только плагины Paper-семейства, не больше [`HANGAR_LIMIT`] штук): поиск проекта по имени из
//!   `plugin.yml` / `paper-plugin.yml` (только точное совпадение без учёта регистра) и его версии под PAPER
//!   с фильтром по версии игры.
//!
//! [`install`] скачивает найденные сборки, проверяет контрольные суммы и заменяет jar; старый уходит в
//! `<plugins|mods>/.mctui-old/` (как в `plugins.rs`). Vanilla — пустой отчёт; Velocity — версии прокси не
//! версии Minecraft, все плагины помечаются «неизвестно» с пояснением.
//!
//! Все функции блокирующие (системный `curl`): вызывать через `spawn_blocking`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use sha2::{Sha256, Sha512};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use crate::core_update::{self, Installed};
use crate::create::Core;
use crate::plugin_search::{self, HANGAR_API, Http, Platform};
use crate::plugins::{self, MODRINTH_API, MetaFormat, PluginInfo, PluginKind};
use crate::tr;

/// Сколько плагинов, не найденных на Modrinth, проверять на Hangar (по два запроса на каждый).
pub const HANGAR_LIMIT: usize = 30;
/// Результатов поиска Hangar, среди которых ищем точное совпадение имени.
const HANGAR_SEARCH_LIMIT: usize = 25;
/// Сколько версий проекта Hangar (уже отфильтрованных по версии игры) смотреть.
const HANGAR_VERSIONS_LIMIT: usize = 25;
const HANGAR_PLATFORM: &str = "PAPER";

// ---------------------------------------------------------------------------------------------------------------
// Типы

/// Сборка, которую нужно поставить вместо установленного файла.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub file_name: String,
    pub url: String,
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    /// Hangar даёт только SHA-256.
    pub sha256: Option<String>,
    pub size: Option<u64>,
    /// `"Modrinth"` или `"Hangar"`.
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compat {
    /// Сборка под целевую версию есть. `update` — `None`, если установленный файл уже её поддерживает,
    /// иначе — что скачать вместо него.
    Ready { version: String, update: Option<Update> },
    /// Проект известен, но сборки под целевую версию нет. `latest_supported` — новейшая версия Minecraft,
    /// которую он поддерживает (если удалось узнать без лишних запросов).
    Missing { latest_supported: Option<String> },
    /// Не найден ни на Modrinth, ни на Hangar, сетевая ошибка или нечего скачивать автоматически.
    Unknown { why: String },
}

#[derive(Debug, Clone)]
pub struct CompatItem {
    pub plugin: PluginInfo,
    pub compat: Compat,
    /// Страница проекта или версии (Modrinth / Hangar).
    pub page: Option<String>,
}

impl CompatItem {
    pub fn update(&self) -> Option<&Update> {
        match &self.compat {
            Compat::Ready { update, .. } => update.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CompatReport {
    /// Целевая версия Minecraft.
    pub target: String,
    /// В порядке [`plugins::scan`] (по имени).
    pub items: Vec<CompatItem>,
    /// Пояснение для всего отчёта (Vanilla, Velocity), на языке интерфейса.
    pub note: Option<String>,
}

impl CompatReport {
    pub fn ready(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.compat, Compat::Ready { .. })).count()
    }

    pub fn missing(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.compat, Compat::Missing { .. })).count()
    }

    pub fn unknown(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.compat, Compat::Unknown { .. })).count()
    }

    /// Готовые, которые нужно заменить (есть [`Update`]) — их и передавать в [`install`].
    pub fn updates(&self) -> Vec<&CompatItem> {
        self.items.iter().filter(|i| i.update().is_some()).collect()
    }

    /// Одна строка: «готово 7 · нет сборки 2 · неизвестно 1» (нулевые части опускаются).
    pub fn summary(&self) -> String {
        if self.items.is_empty() {
            return self.note.clone().unwrap_or_else(|| tr!("нечего проверять", "nothing to check").to_string());
        }
        let (r, m, u) = (self.ready(), self.missing(), self.unknown());
        let mut parts = Vec::new();
        if r > 0 {
            parts.push(tr!(format!("готово {r}"), format!("ready {r}")));
        }
        if m > 0 {
            parts.push(tr!(format!("нет сборки {m}"), format!("no build {m}")));
        }
        if u > 0 {
            parts.push(tr!(format!("неизвестно {u}"), format!("unknown {u}")));
        }
        parts.join(" · ")
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Ответы API

/// Версия Modrinth — с `game_versions`, которых нет в [`plugins::MrVersion`].
#[derive(Debug, Clone, Deserialize)]
struct MrVer {
    id: String,
    project_id: String,
    version_number: String,
    #[serde(default)]
    game_versions: Vec<String>,
    #[serde(default)]
    loaders: Vec<String>,
    #[serde(default)]
    files: Vec<plugins::MrFile>,
}

impl MrVer {
    fn has_hash(&self, sha1: &str) -> bool {
        self.files.iter().any(|f| f.hashes.get("sha1").is_some_and(|h| h.eq_ignore_ascii_case(sha1)))
    }

    fn supports(&self, target: &str, loaders: &[&str]) -> bool {
        self.game_versions.iter().any(|g| g == target)
            && (self.loaders.is_empty() || self.loaders.iter().any(|l| loaders.contains(&l.as_str())))
    }

    fn jar(&self) -> Option<&plugins::MrFile> {
        self.files
            .iter()
            .find(|f| f.primary)
            .or_else(|| self.files.first())
            .filter(|f| f.filename.to_ascii_lowercase().ends_with(".jar"))
    }

    fn page(&self) -> String {
        format!("https://modrinth.com/project/{}/version/{}", self.project_id, self.id)
    }
}

/// `{ "<sha1>": Version }` → ключи в нижнем регистре.
fn parse_mr(json: &str) -> Result<HashMap<String, MrVer>> {
    let map: HashMap<String, MrVer> =
        serde_json::from_str(json).context(tr!("непонятный ответ Modrinth", "unexpected Modrinth response"))?;
    Ok(map.into_iter().map(|(h, v)| (h.to_ascii_lowercase(), v)).collect())
}

#[derive(Debug, Deserialize)]
struct HgSearch {
    #[serde(default)]
    result: Vec<HgProject>,
}

#[derive(Debug, Deserialize)]
struct HgProject {
    name: String,
    namespace: HgNamespace,
    #[serde(default, rename = "supportedPlatforms")]
    supported_platforms: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct HgNamespace {
    owner: String,
    slug: String,
}

#[derive(Debug, Deserialize)]
struct HgVersions {
    #[serde(default)]
    result: Vec<HgVersion>,
}

#[derive(Debug, Deserialize)]
struct HgVersion {
    name: String,
    #[serde(default, rename = "createdAt")]
    created_at: String,
    #[serde(default)]
    channel: Option<HgChannel>,
    #[serde(default)]
    downloads: HashMap<String, HgDownload>,
    /// Полный список версий (`["1.21", "1.21.1", …]`), иногда шаблоны (`1.21.x`).
    #[serde(default, rename = "platformDependencies")]
    platform_dependencies: HashMap<String, Vec<String>>,
    /// Сжатый вид (`["1.8-26.3"]`).
    #[serde(default, rename = "platformDependenciesFormatted")]
    platform_dependencies_formatted: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct HgChannel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct HgDownload {
    #[serde(default, rename = "fileInfo")]
    file_info: Option<HgFileInfo>,
    #[serde(default, rename = "externalUrl")]
    external_url: Option<String>,
    #[serde(default, rename = "downloadUrl")]
    download_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HgFileInfo {
    name: String,
    #[serde(default, rename = "sizeBytes")]
    size_bytes: Option<u64>,
    #[serde(default, rename = "sha256Hash")]
    sha256_hash: Option<String>,
}

impl HgVersion {
    fn supports(&self, target: &str) -> bool {
        let lists = [&self.platform_dependencies, &self.platform_dependencies_formatted];
        let specs: Vec<&String> = lists.iter().filter_map(|m| m.get(HANGAR_PLATFORM)).flatten().collect();
        // Без списка — верим фильтру `platformVersion` самого API.
        specs.is_empty() || specs.iter().any(|s| hangar_matches(s, target))
    }

    fn file(&self) -> Option<(&HgDownload, &HgFileInfo)> {
        let d = self.downloads.get(HANGAR_PLATFORM)?;
        Some((d, d.file_info.as_ref()?))
    }

    fn sha256(&self) -> Option<&str> {
        self.file()?.1.sha256_hash.as_deref()
    }

    /// Можно скачать автоматически: https-ссылка и контрольная сумма.
    fn downloadable(&self) -> bool {
        self.file().is_some_and(|(d, f)| {
            d.download_url.as_deref().is_some_and(|u| u.starts_with("https://")) && f.sha256_hash.is_some()
        })
    }

    fn is_release(&self) -> bool {
        self.channel.as_ref().is_some_and(|c| c.name.eq_ignore_ascii_case("release"))
    }

    fn update(&self) -> Option<Update> {
        let (d, f) = self.file()?;
        Some(Update {
            file_name: f.name.clone(),
            url: d.download_url.clone()?,
            sha1: None,
            sha512: None,
            sha256: f.sha256_hash.as_ref().map(|h| h.to_ascii_lowercase()),
            size: f.size_bytes,
            source: "Hangar",
        })
    }
}

/// Подходит ли целевая версия под запись Hangar: точная версия (`1.21.4`), шаблон (`1.21.x`, `1.21.*`)
/// или диапазон (`1.20-1.21.4`, `1.20.5-1.21.x`).
pub(crate) fn hangar_matches(spec: &str, target: &str) -> bool {
    let (spec, target) = (spec.trim(), target.trim());
    if spec.is_empty() || target.is_empty() {
        return false;
    }
    if spec.eq_ignore_ascii_case(target) {
        return true;
    }
    let is_bound = |s: &str| {
        s.starts_with(|c: char| c.is_ascii_digit())
            && s.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'x' || c == 'X' || c == '*')
    };
    if let Some((lo, hi)) = spec.split_once('-')
        && is_bound(lo.trim())
        && is_bound(hi.trim())
    {
        let (lo, hi) = (lo.trim(), hi.trim());
        // `1.21.x-…` снизу — это `1.21`.
        let lo = wildcard_prefix(lo).unwrap_or(lo);
        let lo_ok = core_update::cmp_version(target, lo).is_ge();
        let hi_ok = match wildcard_prefix(hi) {
            Some(p) => prefix_matches(p, target) || core_update::cmp_version(target, p).is_le(),
            None => core_update::cmp_version(target, hi).is_le(),
        };
        return lo_ok && hi_ok;
    }
    wildcard_prefix(spec).is_some_and(|prefix| prefix_matches(prefix, target))
}

/// `1.21.x` / `1.21.*` → `1.21`.
fn wildcard_prefix(s: &str) -> Option<&str> {
    s.strip_suffix(".x").or_else(|| s.strip_suffix(".X")).or_else(|| s.strip_suffix(".*"))
}

/// `1.21` покрывает `1.21` и `1.21.N`.
fn prefix_matches(prefix: &str, target: &str) -> bool {
    target == prefix || target.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.'))
}

// ---------------------------------------------------------------------------------------------------------------
// Запросы

fn update_url() -> String {
    format!("{MODRINTH_API}/version_files/update")
}

fn files_url() -> String {
    format!("{MODRINTH_API}/version_files")
}

/// Тело `POST /version_files/update`: последняя версия под загрузчики (и версию игры, если задана).
fn update_body(hashes: &[String], loaders: &[&str], game_version: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({ "hashes": hashes, "algorithm": "sha1", "loaders": loaders });
    if let Some(gv) = game_version {
        body["game_versions"] = serde_json::json!([gv]);
    }
    body
}

/// Тело `POST /version_files`: версия, которой принадлежит каждый файл.
fn files_body(hashes: &[String]) -> serde_json::Value {
    serde_json::json!({ "hashes": hashes, "algorithm": "sha1" })
}

fn hangar_search_url(name: &str) -> String {
    format!(
        "{HANGAR_API}/projects?q={}&limit={HANGAR_SEARCH_LIMIT}&offset=0&platform={HANGAR_PLATFORM}",
        plugin_search::enc(name.trim())
    )
}

fn hangar_versions_url(slug: &str, target: &str) -> String {
    format!(
        "{HANGAR_API}/projects/{}/versions?limit={HANGAR_VERSIONS_LIMIT}&offset=0&platform={HANGAR_PLATFORM}\
         &platformVersion={}",
        plugin_search::enc(slug),
        plugin_search::enc(target)
    )
}

/// Что проверяем на этом ядре: тип файлов, загрузчики Modrinth, искать ли на Hangar.
/// Folia-only сборки на Paper не запускаются — `folia` не входит (как в [`Platform::loaders`]).
fn scope(core: Core) -> Option<(PluginKind, &'static [&'static str], bool)> {
    match core {
        Core::Paper => Some((PluginKind::Plugin, Platform::Paper.loaders(), true)),
        Core::Purpur => Some((PluginKind::Plugin, Platform::Purpur.loaders(), true)),
        Core::Fabric => Some((PluginKind::Mod, Platform::Fabric.loaders(), false)),
        Core::Vanilla | Core::Velocity => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Проверка

/// Проверяет включённые плагины (Paper / Purpur) или моды (Fabric) сервера `dir` на наличие сборки под
/// `target_version`. Ошибка — только если не ответил Modrinth (обычно это значит «нет сети»); ошибки Hangar
/// превращаются в [`Compat::Unknown`] у отдельных плагинов.
pub fn check(dir: &Path, installed: &Installed, target_version: &str) -> Result<CompatReport> {
    check_with(&plugin_search::Curl, plugins::scan(dir), installed.core, target_version)
}

pub(crate) fn check_with(
    http: &dyn Http,
    scanned: Vec<PluginInfo>,
    core: Core,
    target_version: &str,
) -> Result<CompatReport> {
    let target = target_version.trim();
    if target.is_empty() {
        bail!(tr!("не указана целевая версия", "no target version given"));
    }
    let mut report = CompatReport { target: target.to_string(), ..CompatReport::default() };
    let Some((kind, loaders, use_hangar)) = scope(core) else {
        if core == Core::Velocity {
            let why = tr!(
                "у Velocity версии прокси, а не Minecraft — совместимость плагинов проверьте вручную",
                "Velocity versions are proxy versions, not Minecraft ones — check plugin compatibility manually"
            )
            .to_string();
            report.items = scanned
                .into_iter()
                .filter(|p| p.enabled && p.kind == PluginKind::Plugin)
                .map(|plugin| CompatItem { plugin, compat: Compat::Unknown { why: why.clone() }, page: None })
                .collect();
            report.note = Some(why);
        } else {
            report.note = Some(tr!("у Vanilla нет плагинов и модов", "Vanilla has no plugins or mods").to_string());
        }
        return Ok(report);
    };
    let plugins: Vec<PluginInfo> = scanned.into_iter().filter(|p| p.enabled && p.kind == kind).collect();
    if plugins.is_empty() {
        return Ok(report);
    }

    // Modrinth: три пакетных запроса на все файлы.
    let mut hashes: Vec<String> =
        plugins.iter().filter(|p| !p.sha1.is_empty()).map(|p| p.sha1.to_ascii_lowercase()).collect();
    hashes.sort_unstable();
    hashes.dedup();
    let (mut ready, mut current, mut latest) = (HashMap::new(), HashMap::new(), HashMap::new());
    if !hashes.is_empty() {
        ready = parse_mr(&http.post_json(&update_url(), &update_body(&hashes, loaders, Some(target)))?)?;
        current = parse_mr(&http.post_json(&files_url(), &files_body(&hashes))?)?;
        let missing: Vec<String> =
            hashes.iter().filter(|h| !ready.contains_key(*h) && current.contains_key(*h)).cloned().collect();
        if !missing.is_empty() {
            // Без фильтра по версии игры — чтобы узнать новейшую поддерживаемую.
            latest = parse_mr(&http.post_json(&update_url(), &update_body(&missing, loaders, None))?)?;
        }
    }

    let mut hangar_checked = 0;
    let mut hangar_down: Option<String> = None;
    for plugin in plugins {
        let sha1 = plugin.sha1.to_ascii_lowercase();
        let (compat, page) = if sha1.is_empty() {
            (unknown(tr!("не удалось прочитать файл", "cannot read the file")), None)
        } else if let Some(found) = modrinth_compat(&sha1, target, loaders, &ready, &current, &latest) {
            found
        } else if !use_hangar || !matches!(plugin.format, MetaFormat::Paper | MetaFormat::Bukkit) {
            (unknown(tr!("не найден на Modrinth", "not found on Modrinth")), None)
        } else if let Some(err) = &hangar_down {
            (unknown(err.clone()), None)
        } else if hangar_checked >= HANGAR_LIMIT {
            (
                unknown(tr!(
                    format!("не найден на Modrinth; Hangar проверяется не больше чем для {HANGAR_LIMIT} плагинов"),
                    format!("not found on Modrinth; Hangar is checked for at most {HANGAR_LIMIT} plugins")
                )),
                None,
            )
        } else {
            hangar_checked += 1;
            match hangar_compat(http, &plugin, target) {
                Ok(found) => found,
                Err(e) => {
                    // Скорее всего, Hangar недоступен — не ждём таймаутов на остальных.
                    let why = format!("Hangar: {e:#}");
                    hangar_down = Some(why.clone());
                    (unknown(why), None)
                }
            }
        };
        report.items.push(CompatItem { plugin, compat, page });
    }
    Ok(report)
}

fn unknown(why: impl Into<String>) -> Compat {
    Compat::Unknown { why: why.into() }
}

/// Итог по ответам Modrinth; `None` — Modrinth этот файл не знает.
fn modrinth_compat(
    sha1: &str,
    target: &str,
    loaders: &[&str],
    ready: &HashMap<String, MrVer>,
    current: &HashMap<String, MrVer>,
    latest: &HashMap<String, MrVer>,
) -> Option<(Compat, Option<String>)> {
    let cur = current.get(sha1);
    if let Some(v) = ready.get(sha1) {
        // Установленный файл и есть эта сборка или его версия уже заявляет поддержку цели.
        if v.has_hash(sha1) {
            return Some((Compat::Ready { version: v.version_number.clone(), update: None }, Some(v.page())));
        }
        if let Some(c) = cur.filter(|c| c.supports(target, loaders)) {
            return Some((Compat::Ready { version: c.version_number.clone(), update: None }, Some(c.page())));
        }
        let Some(f) = v.jar() else {
            let why = tr!(
                format!("у сборки {} на Modrinth нет jar-файла", v.version_number),
                format!("Modrinth build {} has no jar file", v.version_number)
            );
            return Some((unknown(why), Some(v.page())));
        };
        let h = |k: &str| f.hashes.get(k).map(|x| x.to_ascii_lowercase());
        let update = Update {
            file_name: f.filename.clone(),
            url: f.url.clone(),
            sha1: h("sha1"),
            sha512: h("sha512"),
            sha256: None,
            size: f.size,
            source: "Modrinth",
        };
        return Some((Compat::Ready { version: v.version_number.clone(), update: Some(update) }, Some(v.page())));
    }
    let c = cur?;
    // Новейшая релизная версия; если последняя сборка только под снапшоты — последний из них.
    let latest_supported = latest
        .get(sha1)
        .and_then(|l| plugin_search::newest_game_version(&l.game_versions).or_else(|| l.game_versions.last().cloned()));
    Some((Compat::Missing { latest_supported }, Some(format!("https://modrinth.com/project/{}", c.project_id))))
}

/// Поиск на Hangar по имени из `plugin.yml`: точное совпадение имени, затем версии под PAPER с фильтром
/// по версии игры. `Err` — сетевая ошибка или непонятный ответ.
fn hangar_compat(http: &dyn Http, plugin: &PluginInfo, target: &str) -> Result<(Compat, Option<String>)> {
    let not_found = || unknown(tr!("не найден на Modrinth и Hangar", "not found on Modrinth or Hangar"));
    let name = plugin.name.trim();
    let Some(body) = http.get(&hangar_search_url(name))? else { return Ok((not_found(), None)) };
    let search: HgSearch =
        serde_json::from_str(&body).context(tr!("непонятный ответ Hangar", "unexpected Hangar response"))?;
    let Some(project) = search.result.into_iter().find(|p| p.name.trim().eq_ignore_ascii_case(name)) else {
        return Ok((not_found(), None));
    };
    let page = Some(format!("https://hangar.papermc.io/{}/{}", project.namespace.owner, project.namespace.slug));
    let Some(body) = http.get(&hangar_versions_url(&project.namespace.slug, target))? else {
        return Ok((not_found(), page));
    };
    let versions: HgVersions =
        serde_json::from_str(&body).context(tr!("непонятный ответ Hangar", "unexpected Hangar response"))?;
    let mut compatible: Vec<HgVersion> = versions.result.into_iter().filter(|v| v.supports(target)).collect();
    if compatible.is_empty() {
        let latest_supported =
            project.supported_platforms.get(HANGAR_PLATFORM).and_then(plugin_search::newest_game_version);
        return Ok((Compat::Missing { latest_supported }, page));
    }
    compatible.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    // Установленный файл — одна из совместимых сборок?
    if let Ok(installed) = digest_file::<Sha256>(&plugin.path)
        && let Some(v) = compatible.iter().find(|v| v.sha256().is_some_and(|h| h.eq_ignore_ascii_case(&installed)))
    {
        return Ok((Compat::Ready { version: v.name.clone(), update: None }, page));
    }
    let downloadable: Vec<&HgVersion> = compatible.iter().filter(|v| v.downloadable()).collect();
    let pick = downloadable.iter().find(|v| v.is_release()).or_else(|| downloadable.first());
    if let Some(v) = pick
        && let Some(update) = v.update()
    {
        return Ok((Compat::Ready { version: v.name.clone(), update: Some(update) }, page));
    }
    let why = match compatible.iter().find_map(|v| v.downloads.get(HANGAR_PLATFORM)?.external_url.clone()) {
        Some(url) => tr!(
            format!("сборка под {target} есть на Hangar только внешней ссылкой: {url}"),
            format!("a build for {target} is on Hangar only as an external link: {url}")
        ),
        None => tr!(
            format!("у сборки под {target} на Hangar нет файла для скачивания"),
            format!("the Hangar build for {target} has no downloadable file")
        ),
    };
    Ok((unknown(why), page))
}

// ---------------------------------------------------------------------------------------------------------------
// Установка

/// Ставит сборки из [`Compat::Ready`] с [`Update`] (остальные пункты пропускаются). Каждый файл — отдельно
/// и атомарно: скачивание во временный `.part` в той же папке, проверка размера, хэша (sha512 / sha256 /
/// sha1) и того, что это jar; затем старый jar уходит в `<plugins|mods>/.mctui-old/` (под свободным именем),
/// а новый встаёт рядом с тем местом, где был старый. Ошибка одного файла не мешает остальным и попадает
/// в заметки; `Err` — если не удалось ни одного. Возвращает заметки на языке интерфейса.
/// Сервер должен быть остановлен; новое начинает работать после запуска.
pub fn install(dir: &Path, items: &[&CompatItem], progress: &mut dyn FnMut(String)) -> Result<Vec<String>> {
    install_with(dir, items, progress, plugins::download)
}

pub(crate) fn install_with(
    dir: &Path,
    items: &[&CompatItem],
    progress: &mut dyn FnMut(String),
    fetch: impl Fn(&str, &Path) -> Result<()>,
) -> Result<Vec<String>> {
    let todo: Vec<(&CompatItem, &Update)> = items.iter().filter_map(|i| Some((*i, i.update()?))).collect();
    if todo.is_empty() {
        return Ok(vec![tr!("нечего обновлять", "nothing to update").to_string()]);
    }
    let total = todo.len();
    let mut notes = Vec::new();
    let mut failed = 0;
    let mut names = HashSet::new();
    for (i, (item, u)) in todo.into_iter().enumerate() {
        let name = &item.plugin.name;
        progress(tr!(
            format!("Скачиваю {} ({}/{total})…", u.file_name, i + 1),
            format!("Downloading {} ({}/{total})…", u.file_name, i + 1)
        ));
        let res = if names.insert(u.file_name.to_ascii_lowercase()) {
            replace_one(dir, &item.plugin, u, &fetch, i)
        } else {
            Err(anyhow::anyhow!(tr!(
                format!("файл {} уже поставлен для другого плагина", u.file_name),
                format!("file {} was already installed for another plugin", u.file_name)
            )))
        };
        match res {
            Ok(note) => notes.push(note),
            Err(e) => {
                failed += 1;
                notes.push(tr!(format!("{name}: не обновлён — {e:#}"), format!("{name}: not updated — {e:#}")));
            }
        }
    }
    if failed == total {
        bail!(notes.join("; "));
    }
    notes.push(
        tr!("Новые файлы заработают после запуска сервера.", "The new files take effect when the server starts.")
            .to_string(),
    );
    Ok(notes)
}

fn replace_one(
    dir: &Path,
    p: &PluginInfo,
    u: &Update,
    fetch: &impl Fn(&str, &Path) -> Result<()>,
    seq: usize,
) -> Result<String> {
    let folder = p.path.parent().context(tr!("нет папки плагина", "plugin has no parent folder"))?;
    // Только файлы из plugins/ или mods/ этого сервера.
    if !folder.starts_with(dir) {
        bail!(tr!(
            format!("{} лежит вне папки сервера", p.path.display()),
            format!("{} is outside the server folder", p.path.display())
        ));
    }
    let jar = plugins::safe_jar_name(&u.file_name)?;
    let new_name = if p.enabled { jar.to_string() } else { format!("{jar}{}", plugins::DISABLED_SUFFIX) };
    let new_path = folder.join(&new_name);
    let old_name = p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.file_name.clone());
    if !p.path.is_file() {
        bail!(tr!(format!("файл {old_name} не найден"), format!("file {old_name} not found")));
    }
    // Совпадение без учёта регистра — это тот же файл на macOS / Windows.
    if !new_name.eq_ignore_ascii_case(&old_name) && new_path.exists() {
        bail!(tr!(format!("файл {new_name} уже существует"), format!("file {new_name} already exists")));
    }

    // Без расширения .jar — сервер не подхватит недокачанный файл.
    let tmp = folder.join(format!(".mctui-download-{}-{seq}.part", std::process::id()));
    let res = (|| -> Result<()> {
        fetch(&u.url, &tmp)?;
        verify(u, &tmp)?;
        let old_dir = folder.join(plugins::OLD_DIR);
        std::fs::create_dir_all(&old_dir)?;
        let backup = core_update::free_name(&old_dir, &old_name);
        std::fs::rename(&p.path, &backup).with_context(|| {
            tr!(
                format!("не удалось убрать {old_name} в {}", plugins::OLD_DIR),
                format!("failed to move {old_name} to {}", plugins::OLD_DIR)
            )
        })?;
        if let Err(e) = std::fs::rename(&tmp, &new_path) {
            let _ = std::fs::rename(&backup, &p.path);
            return Err(e).with_context(|| {
                tr!(format!("не удалось записать {new_name}"), format!("failed to write {new_name}"))
            });
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    res?;
    Ok(tr!(
        format!("{}: {old_name} → {new_name} ({}); старый — в {}", p.name, u.source, plugins::OLD_DIR),
        format!("{}: {old_name} → {new_name} ({}); the old one is in {}", p.name, u.source, plugins::OLD_DIR)
    ))
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

/// Размер, самый сильный из известных хэшей и то, что это zip (jar).
fn verify(u: &Update, path: &Path) -> Result<()> {
    if let Some(size) = u.size {
        let actual = std::fs::metadata(path)?.len();
        if actual != size {
            bail!(tr!(
                format!("размер не совпал (ожидалось {size}, получено {actual})"),
                format!("size mismatch (expected {size}, got {actual})")
            ));
        }
    }
    let (algo, expected, actual) = if let Some(h) = &u.sha512 {
        ("sha512", h, digest_file::<Sha512>(path)?)
    } else if let Some(h) = &u.sha256 {
        ("sha256", h, digest_file::<Sha256>(path)?)
    } else if let Some(h) = &u.sha1 {
        ("sha1", h, digest_file::<Sha1>(path)?)
    } else {
        bail!(tr!("у файла нет контрольной суммы", "the file has no checksum"));
    };
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!(tr!(format!("контрольная сумма не совпала ({algo})"), format!("checksum mismatch ({algo})")));
    }
    zip::ZipArchive::new(std::fs::File::open(path)?)
        .context(tr!("скачанный файл — не jar", "downloaded file is not a jar"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Write;
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "mctui-compat-{tag}-{}-{}",
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

    fn jar_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn info(name: &str, sha1: &str, format: MetaFormat, kind: PluginKind) -> PluginInfo {
        PluginInfo {
            path: PathBuf::from(format!("/nonexistent/{name}.jar")),
            file_name: format!("{name}.jar"),
            enabled: true,
            kind,
            name: name.to_string(),
            version: None,
            description: None,
            authors: Vec::new(),
            sha1: sha1.to_string(),
            format,
        }
    }

    /// Подставные ответы: GET — по подстроке URL; POST — по URL и наличию `game_versions`.
    #[derive(Default)]
    struct Fake {
        gets: Vec<(&'static str, Option<String>)>,
        update_gv: String,
        update_any: String,
        files: String,
        calls: RefCell<Vec<String>>,
        bodies: RefCell<Vec<serde_json::Value>>,
        fail_get: bool,
    }

    impl Http for Fake {
        fn get(&self, url: &str) -> Result<Option<String>> {
            self.calls.borrow_mut().push(format!("GET {url}"));
            if self.fail_get {
                bail!("offline");
            }
            let hit = self.gets.iter().find(|(k, _)| url.contains(k));
            Ok(hit.map(|(_, v)| v.clone()).unwrap_or(Some(r#"{"result":[]}"#.into())))
        }
        fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<String> {
            self.calls.borrow_mut().push(format!("POST {url}"));
            self.bodies.borrow_mut().push(body.clone());
            Ok(if url.ends_with("/version_files/update") {
                if body.get("game_versions").is_some() { self.update_gv.clone() } else { self.update_any.clone() }
            } else {
                self.files.clone()
            })
        }
    }

    fn mr_version(id: &str, number: &str, sha1: &str, games: &[&str], loaders: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "id": id, "project_id": format!("proj-{id}"), "version_number": number,
            "game_versions": games, "loaders": loaders, "date_published": "2026-01-01T00:00:00Z",
            "files": [{
                "hashes": { "sha1": sha1, "sha512": format!("{sha1}-512") },
                "url": format!("https://cdn.modrinth.com/{id}.jar"),
                "filename": format!("{number}.jar"), "primary": true, "size": 123
            }]
        })
    }

    #[test]
    fn request_bodies_and_urls() {
        let h = vec!["aa".to_string(), "bb".to_string()];
        assert_eq!(
            update_body(&h, &["paper", "spigot"], Some("1.21.11")),
            serde_json::json!({"hashes": ["aa", "bb"], "algorithm": "sha1", "loaders": ["paper", "spigot"],
                               "game_versions": ["1.21.11"]})
        );
        assert!(update_body(&h, &["fabric"], None).get("game_versions").is_none());
        assert_eq!(files_body(&h), serde_json::json!({"hashes": ["aa", "bb"], "algorithm": "sha1"}));
        assert_eq!(
            hangar_search_url(" My Plugin "),
            format!("{HANGAR_API}/projects?q=My%20Plugin&limit=25&offset=0&platform=PAPER")
        );
        assert_eq!(
            hangar_versions_url("ViaVersion", "1.21.11"),
            format!(
                "{HANGAR_API}/projects/ViaVersion/versions?limit=25&offset=0&platform=PAPER&platformVersion=1.21.11"
            )
        );
        assert_eq!(scope(Core::Purpur).unwrap().1, &["purpur", "paper", "spigot", "bukkit"]);
        assert_eq!(scope(Core::Fabric).unwrap().0, PluginKind::Mod);
        assert!(scope(Core::Vanilla).is_none() && scope(Core::Velocity).is_none());
    }

    #[test]
    fn hangar_version_ranges() {
        assert!(hangar_matches("1.21.4", "1.21.4"));
        assert!(!hangar_matches("1.21.4", "1.21.40"));
        assert!(hangar_matches("1.21.x", "1.21"));
        assert!(hangar_matches("1.21.x", "1.21.11"));
        assert!(hangar_matches("1.21.*", "1.21.3"));
        assert!(!hangar_matches("1.21.x", "1.2"));
        assert!(!hangar_matches("1.21.x", "1.22"));
        assert!(!hangar_matches("1.2.x", "1.21.1"));
        assert!(hangar_matches("1.8-26.3", "1.21.11"));
        assert!(hangar_matches("1.8-26.3", "26.3"));
        assert!(hangar_matches("1.8-26.3", "1.8"));
        assert!(!hangar_matches("1.8-26.3", "26.4"));
        assert!(!hangar_matches("1.8-26.3", "1.7.10"));
        assert!(hangar_matches("1.20.5-1.21.x", "1.21.9"));
        assert!(!hangar_matches("1.20.5-1.21.x", "1.20.4"));
        assert!(!hangar_matches("1.20.5-1.21.x", "26.1"));
        assert!(!hangar_matches("", "1.21"));
        assert!(!hangar_matches("1.21-pre1", "1.21"));

        let v: HgVersion = serde_json::from_str(
            r#"{"name":"1","platformDependencies":{"PAPER":["1.20.6","1.21"]},
                "platformDependenciesFormatted":{"PAPER":["1.20.6-1.21"]}}"#,
        )
        .unwrap();
        assert!(v.supports("1.21") && v.supports("1.20.6") && !v.supports("1.21.1"));
        let v: HgVersion = serde_json::from_str(r#"{"name":"1"}"#).unwrap();
        assert!(v.supports("1.21"), "без списка верим фильтру API");
    }

    #[test]
    fn modrinth_and_hangar_flow() {
        let target = "1.21.11";
        // a: установленный файл и есть сборка под цель; b: есть новая сборка; c: установленная версия уже
        // заявляет поддержку; d: проект знаком, сборки нет; e: не на Modrinth → Hangar (обновление);
        // f: не на Modrinth → Hangar, сборки нет; g: без plugin.yml; h: не на Modrinth → Hangar, только внешняя
        // ссылка; z: пустой SHA-1; off: выключен — не проверяется.
        let update_gv = serde_json::json!({
            "aaaa": mr_version("va", "A-2", "aaaa", &[target], &["paper"]),
            "bbbb": mr_version("vb2", "B-2", "b2b2", &[target], &["paper"]),
            "cccc": mr_version("vc2", "C-2", "c2c2", &[target], &["paper"]),
        });
        let files = serde_json::json!({
            "aaaa": mr_version("va", "A-2", "aaaa", &[target], &["paper"]),
            "bbbb": mr_version("vb1", "B-1", "bbbb", &["1.21.4"], &["paper"]),
            "CCCC": mr_version("vc1", "C-1", "cccc", &["1.21.10", target], &["spigot"]),
            "dddd": mr_version("vd1", "D-1", "dddd", &["1.20.1"], &["paper"]),
        });
        let update_any = serde_json::json!({
            "dddd": mr_version("vd2", "D-2", "d2d2", &["1.21.4", "1.21.4-pre1", "1.21"], &["paper"]),
        });
        let hg_project = |name: &str| {
            serde_json::json!({"result": [
                {"name": format!("{name}Extra"), "namespace": {"owner": "x", "slug": "other"}},
                {"name": name.to_lowercase(), "namespace": {"owner": "Owner", "slug": name},
                 "supportedPlatforms": {"PAPER": ["1.20", "1.20.6", "1.20.10"]}}
            ]})
            .to_string()
        };
        let hg_versions = serde_json::json!({"result": [
            {"name": "E-3-SNAPSHOT", "createdAt": "2026-03-01T00:00:00Z", "channel": {"name": "Snapshot"},
             "platformDependencies": {"PAPER": ["1.21.x"]},
             "downloads": {"PAPER": {"fileInfo": {"name": "E-3.jar", "sizeBytes": 10, "sha256Hash": "E3"},
                                     "downloadUrl": "https://hangarcdn.papermc.io/E-3.jar"}}},
            {"name": "E-2", "createdAt": "2026-02-01T00:00:00Z", "channel": {"name": "Release"},
             "platformDependencies": {"PAPER": ["1.21.10", "1.21.11"]},
             "downloads": {"PAPER": {"fileInfo": {"name": "E-2.jar", "sizeBytes": 9, "sha256Hash": "E2"},
                                     "downloadUrl": "https://hangarcdn.papermc.io/E-2.jar"}}},
            {"name": "E-old", "createdAt": "2026-01-01T00:00:00Z", "channel": {"name": "Release"},
             "platformDependencies": {"PAPER": ["1.20.6"]},
             "downloads": {"PAPER": {"fileInfo": {"name": "E-0.jar", "sha256Hash": "E0"},
                                     "downloadUrl": "https://hangarcdn.papermc.io/E-0.jar"}}}
        ]});
        let hg_external = serde_json::json!({"result": [
            {"name": "H-1", "createdAt": "2026-02-01T00:00:00Z", "channel": {"name": "Release"},
             "downloads": {"PAPER": {"fileInfo": null, "externalUrl": "https://example.org/h.jar"}}}
        ]});
        let fake = Fake {
            gets: vec![
                ("projects?q=Eplug", Some(hg_project("Eplug"))),
                ("projects?q=Fplug", Some(hg_project("Fplug"))),
                ("projects?q=Hplug", Some(hg_project("Hplug"))),
                ("projects/Eplug/versions", Some(hg_versions.to_string())),
                ("projects/Fplug/versions", Some(r#"{"result":[]}"#.into())),
                ("projects/Hplug/versions", Some(hg_external.to_string())),
            ],
            update_gv: update_gv.to_string(),
            update_any: update_any.to_string(),
            files: files.to_string(),
            ..Fake::default()
        };
        let p = |n: &str, h: &str| info(n, h, MetaFormat::Bukkit, PluginKind::Plugin);
        let mut off = p("Off", "ffff");
        off.enabled = false;
        let scanned = vec![
            p("A", "AAAA"),
            p("B", "bbbb"),
            p("C", "cccc"),
            p("D", "dddd"),
            p("Eplug", "eeee"),
            p("Fplug", "f0f0"),
            info("G", "1111", MetaFormat::Unknown, PluginKind::Plugin),
            p("Hplug", "2222"),
            p("Z", ""),
            off,
            info("SomeMod", "3333", MetaFormat::Fabric, PluginKind::Mod),
        ];
        let r = check_with(&fake, scanned, Core::Paper, " 1.21.11 ").unwrap();
        assert_eq!(r.target, target);
        let by: HashMap<&str, &CompatItem> = r.items.iter().map(|i| (i.plugin.name.as_str(), i)).collect();
        assert_eq!(r.items.len(), 9, "выключенный плагин и мод на Paper не проверяются");

        assert_eq!(by["A"].compat, Compat::Ready { version: "A-2".into(), update: None });
        assert_eq!(by["A"].page.as_deref(), Some("https://modrinth.com/project/proj-va/version/va"));
        let Compat::Ready { version, update: Some(u) } = &by["B"].compat else { panic!("{:?}", by["B"].compat) };
        assert_eq!(version, "B-2");
        assert_eq!(
            u,
            &Update {
                file_name: "B-2.jar".into(),
                url: "https://cdn.modrinth.com/vb2.jar".into(),
                sha1: Some("b2b2".into()),
                sha512: Some("b2b2-512".into()),
                sha256: None,
                size: Some(123),
                source: "Modrinth",
            }
        );
        assert_eq!(by["C"].compat, Compat::Ready { version: "C-1".into(), update: None });
        assert_eq!(by["D"].compat, Compat::Missing { latest_supported: Some("1.21.4".into()) });
        assert_eq!(by["D"].page.as_deref(), Some("https://modrinth.com/project/proj-vd1"));

        let Compat::Ready { version, update: Some(u) } = &by["Eplug"].compat else { panic!() };
        assert_eq!(version, "E-2", "релиз предпочтительнее снапшота");
        assert_eq!((u.source, u.sha256.as_deref(), u.size), ("Hangar", Some("e2"), Some(9)));
        assert_eq!(by["Eplug"].page.as_deref(), Some("https://hangar.papermc.io/Owner/Eplug"));
        assert_eq!(by["Fplug"].compat, Compat::Missing { latest_supported: Some("1.20.10".into()) });
        assert!(matches!(&by["G"].compat, Compat::Unknown { .. }));
        let Compat::Unknown { why } = &by["Hplug"].compat else { panic!() };
        assert!(why.contains("https://example.org/h.jar"));
        assert!(matches!(&by["Z"].compat, Compat::Unknown { .. }));
        assert_eq!((r.ready(), r.missing(), r.unknown()), (4, 2, 3));
        assert_eq!(r.updates().len(), 2);

        // Запросы: Modrinth — три пакетных POST, без folia; на Hangar — поиск + версии на каждый плагин.
        let bodies = fake.bodies.borrow();
        assert_eq!(bodies.len(), 3);
        assert_eq!(bodies[0]["loaders"], serde_json::json!(["paper", "spigot", "bukkit"]));
        assert_eq!(bodies[0]["game_versions"], serde_json::json!([target]));
        assert!(bodies[0]["hashes"].as_array().unwrap().iter().any(|h| h == "aaaa"), "хэши в нижнем регистре");
        assert_eq!(bodies[2]["hashes"], serde_json::json!(["dddd"]));
        let gets = fake.calls.borrow().iter().filter(|c| c.starts_with("GET")).count();
        assert_eq!(gets, 6);
    }

    #[test]
    fn hangar_failure_and_limit() {
        let fake = Fake { update_gv: "{}".into(), files: "{}".into(), fail_get: true, ..Fake::default() };
        let scanned: Vec<PluginInfo> = (0..HANGAR_LIMIT + 5)
            .map(|i| info(&format!("P{i:02}"), &format!("{i:04x}"), MetaFormat::Paper, PluginKind::Plugin))
            .collect();
        let r = check_with(&fake, scanned, Core::Purpur, "26.3").unwrap();
        assert_eq!(r.unknown(), HANGAR_LIMIT + 5);
        // После первой сетевой ошибки Hangar больше не опрашивается.
        assert_eq!(fake.calls.borrow().iter().filter(|c| c.starts_with("GET")).count(), 1);

        let fake = Fake { update_gv: "{}".into(), files: "{}".into(), ..Fake::default() };
        let scanned: Vec<PluginInfo> = (0..HANGAR_LIMIT + 5)
            .map(|i| info(&format!("P{i:02}"), &format!("{i:04x}"), MetaFormat::Paper, PluginKind::Plugin))
            .collect();
        check_with(&fake, scanned, Core::Paper, "26.3").unwrap();
        assert_eq!(fake.calls.borrow().iter().filter(|c| c.starts_with("GET")).count(), HANGAR_LIMIT);
    }

    #[test]
    fn vanilla_velocity_fabric_and_summary() {
        let fake = Fake::default();
        let r = check_with(&fake, vec![info("X", "aa", MetaFormat::Bukkit, PluginKind::Plugin)], Core::Vanilla, "1.21")
            .unwrap();
        assert!(r.items.is_empty() && r.note.is_some());
        assert_eq!(r.summary(), r.note.clone().unwrap());
        let r =
            check_with(&fake, vec![info("X", "aa", MetaFormat::Unknown, PluginKind::Plugin)], Core::Velocity, "4.0")
                .unwrap();
        assert_eq!((r.items.len(), r.unknown()), (1, 1));
        assert!(fake.calls.borrow().is_empty(), "ни Vanilla, ни Velocity не ходят в сеть");
        assert!(check_with(&fake, Vec::new(), Core::Paper, "  ").is_err());

        // Fabric: только моды, загрузчик fabric, без Hangar.
        let fake = Fake { update_gv: "{}".into(), files: "{}".into(), ..Fake::default() };
        let scanned = vec![
            info("fabric-api", "abab", MetaFormat::Fabric, PluginKind::Mod),
            info("Plug", "cdcd", MetaFormat::Bukkit, PluginKind::Plugin),
        ];
        let r = check_with(&fake, scanned, Core::Fabric, "1.21.11").unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(fake.bodies.borrow()[0]["loaders"], serde_json::json!(["fabric"]));
        assert!(!fake.calls.borrow().iter().any(|c| c.starts_with("GET")));

        let item =
            |compat| CompatItem { plugin: info("x", "", MetaFormat::Unknown, PluginKind::Plugin), compat, page: None };
        let ready = || item(Compat::Ready { version: "1".into(), update: None });
        let mut r = CompatReport { target: "1.21".into(), items: Vec::new(), note: None };
        assert!(["нечего проверять", "nothing to check"].contains(&r.summary().as_str()));
        r.items = vec![ready(), ready(), item(Compat::Missing { latest_supported: None })];
        r.items.push(item(Compat::Unknown { why: "?".into() }));
        assert!(
            ["готово 2 · нет сборки 1 · неизвестно 1", "ready 2 · no build 1 · unknown 1"]
                .contains(&r.summary().as_str())
        );
        r.items.truncate(2);
        assert!(["готово 2", "ready 2"].contains(&r.summary().as_str()));
    }

    #[test]
    fn install_replaces_and_keeps_old() {
        let t = TempDir::new("install");
        let plugins_dir = t.0.join("plugins");
        std::fs::create_dir_all(plugins_dir.join(plugins::OLD_DIR)).unwrap();
        let old = plugins_dir.join("Foo-1.0.jar");
        std::fs::write(&old, jar_bytes(&[("plugin.yml", "name: Foo\nversion: 1.0\n")])).unwrap();
        // Имя в .mctui-old уже занято — старый файл не должен его затереть.
        std::fs::write(plugins_dir.join(plugins::OLD_DIR).join("Foo-1.0.jar"), b"older").unwrap();
        let bar = plugins_dir.join("Bar.jar");
        std::fs::write(&bar, b"bar").unwrap();

        let new_bytes = jar_bytes(&[("plugin.yml", "name: Foo\nversion: 2.0\n")]);
        let sha512: String = Sha512::digest(&new_bytes).iter().map(|b| format!("{b:02x}")).collect();
        let mut foo = info("Foo", "x", MetaFormat::Bukkit, PluginKind::Plugin);
        foo.path = old.clone();
        let mut barp = info("Bar", "y", MetaFormat::Bukkit, PluginKind::Plugin);
        barp.path = bar.clone();
        let upd = |name: &str, sha512: Option<String>| Update {
            file_name: name.into(),
            url: "https://example.org/x.jar".into(),
            sha1: None,
            sha512,
            sha256: None,
            size: Some(new_bytes.len() as u64),
            source: "Modrinth",
        };
        let ok = CompatItem {
            plugin: foo,
            compat: Compat::Ready { version: "2.0".into(), update: Some(upd("Foo-2.0.jar", Some(sha512))) },
            page: None,
        };
        let bad = CompatItem {
            plugin: barp,
            compat: Compat::Ready { version: "2".into(), update: Some(upd("Bar-2.jar", Some("00".into()))) },
            page: None,
        };
        let skip = CompatItem { compat: Compat::Missing { latest_supported: None }, ..bad.clone() };
        let fetch = |_: &str, dest: &Path| -> Result<()> { Ok(std::fs::write(dest, &new_bytes)?) };
        let mut msgs = Vec::new();
        let notes = install_with(&t.0, &[&ok, &bad, &skip], &mut |m| msgs.push(m), fetch).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[1].contains("Bar"));

        assert!(!old.exists());
        assert_eq!(std::fs::read(plugins_dir.join("Foo-2.0.jar")).unwrap(), new_bytes);
        let backups: Vec<String> = std::fs::read_dir(plugins_dir.join(plugins::OLD_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(backups.len(), 2, "{backups:?}");
        assert_eq!(std::fs::read(plugins_dir.join(plugins::OLD_DIR).join("Foo-1.0.jar")).unwrap(), b"older");
        // Неудачный файл остался на месте, временных файлов нет.
        assert_eq!(std::fs::read(&bar).unwrap(), b"bar");
        assert!(
            !std::fs::read_dir(&plugins_dir).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part"))
        );

        // Все неудачны — ошибка; пустой список — «нечего обновлять».
        assert!(install_with(&t.0, &[&bad], &mut |_| {}, fetch).is_err());
        assert_eq!(install_with(&t.0, &[&skip], &mut |_| {}, fetch).unwrap().len(), 1);
        // Файл вне папки сервера не трогаем.
        let elsewhere = TempDir::new("elsewhere");
        assert!(install_with(&elsewhere.0, &[&bad], &mut |_| {}, fetch).is_err());
    }

    // --- живые проверки (сеть): cargo test compat_net -- --ignored --nocapture

    fn net_download(url: &str, dir: &Path, name: &str) -> PathBuf {
        let dest = dir.join(name);
        plugins::download(url, &dest).unwrap();
        dest
    }

    #[test]
    #[ignore]
    fn compat_net_fabric_api() {
        // Fabric API под 1.21.4 → проверяем под 1.21.11 (новая сборка) и под несуществующую версию.
        let body = plugin_search::Curl
            .get(&format!(
                "{MODRINTH_API}/project/fabric-api/version?include_changelog=false&loaders=%5B%22fabric%22%5D\
                 &game_versions=%5B%221.21.4%22%5D"
            ))
            .unwrap()
            .unwrap();
        let versions: Vec<MrVer> = serde_json::from_str(&body).unwrap();
        let f = versions[0].jar().unwrap();
        let t = TempDir::new("net-fabric");
        let mods = t.0.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        net_download(&f.url, &mods, &f.filename);
        let inst = Installed {
            core: Core::Fabric,
            mc_version: "1.21.4".into(),
            build: None,
            jar: t.0.join("server.jar"),
            script: None,
            java: None,
        };

        let r = check(&t.0, &inst, "1.21.11").unwrap();
        println!("{r:#?}\n{}", r.summary());
        let Compat::Ready { update: Some(u), .. } = &r.items[0].compat else { panic!("{:?}", r.items[0].compat) };
        assert!(u.file_name.contains("1.21.11"), "{}", u.file_name);

        let r = check(&t.0, &inst, "1.21.4").unwrap();
        assert!(matches!(r.items[0].compat, Compat::Ready { update: None, .. }), "{:?}", r.items[0].compat);

        let r = check(&t.0, &inst, "1.99.1").unwrap();
        println!("{:?}", r.items[0].compat);
        let Compat::Missing { latest_supported: Some(_) } = &r.items[0].compat else { panic!() };

        // Установка новой сборки под 1.21.11.
        let r = check(&t.0, &inst, "1.21.11").unwrap();
        let notes = install(&t.0, &r.updates(), &mut |m| println!("{m}")).unwrap();
        println!("{notes:?}");
        let r = check(&t.0, &inst, "1.21.11").unwrap();
        assert!(matches!(r.items[0].compat, Compat::Ready { update: None, .. }), "{:?}", r.items[0].compat);
        assert!(mods.join(plugins::OLD_DIR).join(&f.filename).exists());
    }

    #[test]
    #[ignore]
    fn compat_net_hangar() {
        // Файлы с неизвестным Modrinth содержимым → поиск на Hangar по имени из plugin.yml.
        let t = TempDir::new("net-hangar");
        let plugins_dir = t.0.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        for (file, name) in [("via.jar", "ViaVersion"), ("ess.jar", "Essentials"), ("zz.jar", "NoSuchPluginXyz123")] {
            let body = format!("name: {name}\nversion: 0.0.1-test\nmain: a.B\n");
            std::fs::write(plugins_dir.join(file), jar_bytes(&[("plugin.yml", &body), ("nonce", file)])).unwrap();
        }
        let inst = Installed {
            core: Core::Paper,
            mc_version: "1.21.4".into(),
            build: None,
            jar: t.0.join("server.jar"),
            script: None,
            java: None,
        };
        let r = check(&t.0, &inst, "1.21.11").unwrap();
        println!("{r:#?}\n{}", r.summary());
        let by: HashMap<&str, &CompatItem> = r.items.iter().map(|i| (i.plugin.name.as_str(), i)).collect();
        let Compat::Ready { update: Some(u), .. } = &by["ViaVersion"].compat else { panic!() };
        assert_eq!(u.source, "Hangar");
        assert!(matches!(by["Essentials"].compat, Compat::Unknown { .. } | Compat::Ready { .. }));
        assert!(matches!(by["NoSuchPluginXyz123"].compat, Compat::Unknown { .. }));

        let r = check(&t.0, &inst, "1.7.10").unwrap();
        let via = r.items.iter().find(|i| i.plugin.name == "ViaVersion").unwrap();
        println!("{:?}", via.compat);
        assert!(matches!(via.compat, Compat::Missing { latest_supported: Some(_) }));
    }
}
