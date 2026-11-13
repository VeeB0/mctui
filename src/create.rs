//! Создание нового сервера Minecraft с нуля: ядро (Paper / Purpur / Fabric / Vanilla), Java,
//! eula.txt, server.properties и скрипт запуска.
//!
//! Блокирующий API — вызывающая сторона запускает [`create`] внутри `tokio::task::spawn_blocking`.
//! Сеть — через системный `curl`, как в `update.rs`.
//!
//! Источники (проверены вживую в ноябре 2026):
//! - Paper: PaperMC Fill API v3 — `fill.papermc.io/v3/projects/paper/versions` (новые сверху, с минимальной
//!   Java) и `…/versions/{v}/builds?channel=STABLE` (sha256 + размер);
//! - Purpur: `api.purpurmc.org/v2/purpur` и `…/{v}/latest` (md5), файл — `…/{v}/{build}/download`;
//! - Fabric: `meta.fabricmc.net/v2/versions/{game,installer,loader/{v}}`, серверный лаунчер —
//!   `…/loader/{v}/{loader}/{installer}/server/jar` (контрольной суммы API не даёт);
//! - Vanilla: Mojang `piston-meta.mojang.com/mc/game/version_manifest_v2.json` → JSON версии (sha1) →
//!   `downloads.server` (sha1 + размер). Тот же JSON даёт `javaVersion.majorVersion` для всех ядер;
//! - Java: Eclipse Temurin JRE из Adoptium API v3 (`api.adoptium.net/v3/assets/latest/{n}/hotspot`, sha256).
//!
//! Память в скрипте: `-Xmx` = `memory_mb`, `-Xms` = `min(memory_mb, 1024)` — сервер стартует быстро,
//! но не занимает весь лимит сразу (на домашнем компьютере память нужна и другим программам).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::tr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Core {
    Paper,
    Purpur,
    Fabric,
    Vanilla,
}

impl Core {
    pub const ALL: [Core; 4] = [Core::Paper, Core::Purpur, Core::Fabric, Core::Vanilla];

    pub fn label(self) -> &'static str {
        match self {
            Core::Paper => "Paper",
            Core::Purpur => "Purpur",
            Core::Fabric => "Fabric",
            Core::Vanilla => "Vanilla",
        }
    }

    pub fn description(self) -> String {
        match self {
            Core::Paper => tr!("Плагины Bukkit/Paper, высокая производительность", "Bukkit/Paper plugins, fast"),
            Core::Purpur => {
                tr!("Как Paper (плагины) + много настроек геймплея", "Like Paper (plugins) + gameplay tweaks")
            }
            Core::Fabric => tr!("Моды Fabric (часто нужны и игрокам)", "Fabric mods (players often need them too)"),
            Core::Vanilla => {
                tr!("Чистый сервер Mojang без плагинов и модов", "Plain Mojang server, no plugins or mods")
            }
        }
        .to_string()
    }

    /// Аргумент «без окна» для этого ядра (Fabric-лаунчер и старые ванильные сборки понимают только `nogui`).
    fn nogui(self) -> &'static str {
        match self {
            Core::Paper | Core::Purpur => "--nogui",
            Core::Fabric | Core::Vanilla => "nogui",
        }
    }
}

pub struct CreateOptions {
    pub core: Core,
    pub version: String,
    pub dir: PathBuf,
    pub memory_mb: u32,
    pub port: u16,
    pub accept_eula: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Step(String),
    Download { what: String, done: u64, total: Option<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub dir: PathBuf,
    /// Команда запуска относительно `dir` (`./start.sh` или `cmd /C start.bat`, как `config::d_command`).
    pub command: Vec<String>,
    /// `java` из PATH или абсолютный путь к скачанной JRE.
    pub java: PathBuf,
    /// Имя jar-файла сервера в `dir`.
    pub jar: String,
}

pub const EULA_URL: &str = "https://aka.ms/MinecraftEULA";
const MIN_MEMORY_MB: u32 = 512;

// ───────────────────────────── сеть ─────────────────────────────

const PAPER_API: &str = "https://fill.papermc.io/v3/projects/paper";
const PURPUR_API: &str = "https://api.purpurmc.org/v2/purpur";
const FABRIC_API: &str = "https://meta.fabricmc.net/v2";
const MOJANG_MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const ADOPTIUM_API: &str = "https://api.adoptium.net/v3";

fn user_agent() -> String {
    // PaperMC Fill API требует осмысленный User-Agent.
    format!("mctui/{} (https://github.com/{})", crate::update::VERSION, crate::update::REPO)
}

fn curl() -> Command {
    let mut c = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    c.args(["-fsSL", "--connect-timeout", "15", "--retry", "2", "-A"]).arg(user_agent());
    c
}

fn curl_failed(url: &str, stderr: &[u8]) -> anyhow::Error {
    let err = String::from_utf8_lossy(stderr).trim().to_string();
    anyhow::anyhow!(tr!(format!("не удалось получить {url}: {err}"), format!("failed to fetch {url}: {err}")))
}

fn get_bytes(url: &str) -> Result<Vec<u8>> {
    let out = curl()
        .args(["--max-time", "60"])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| tr!("не удалось запустить curl", "failed to run curl"))?;
    if !out.status.success() {
        return Err(curl_failed(url, &out.stderr));
    }
    Ok(out.stdout)
}

fn get_json<T: DeserializeOwned>(url: &str) -> Result<T> {
    let body = get_bytes(url)?;
    parse_json(&body, url)
}

fn parse_json<T: DeserializeOwned>(body: &[u8], what: &str) -> Result<T> {
    serde_json::from_slice(body)
        .with_context(|| tr!(format!("неожиданный ответ от {what}"), format!("unexpected response from {what}")))
}

/// Размер файла по заголовку `Content-Length` (после всех редиректов).
fn head_length(url: &str) -> Option<u64> {
    let out = curl().args(["--max-time", "20", "-I"]).arg(url).stdin(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_content_length(&String::from_utf8_lossy(&out.stdout))
}

fn parse_content_length(headers: &str) -> Option<u64> {
    headers
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<u64>().ok())?
        })
        .next_back()
        .filter(|&n| n > 0)
}

/// Скачивает `url` в `dest`, сообщая прогресс по мере роста файла.
fn download(url: &str, dest: &Path, what: &str, total: Option<u64>, progress: &mut dyn FnMut(Progress)) -> Result<()> {
    let total = total.or_else(|| head_length(url));
    progress(Progress::Download { what: what.to_string(), done: 0, total });
    let mut child = curl()
        .arg("-o")
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| tr!("не удалось запустить curl", "failed to run curl"))?;
    let mut last = 0;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        std::thread::sleep(Duration::from_millis(200));
        let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        if done != last {
            last = done;
            progress(Progress::Download { what: what.to_string(), done, total });
        }
    };
    if !status.success() {
        let mut err = Vec::new();
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_end(&mut err);
        }
        return Err(curl_failed(url, &err));
    }
    let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    progress(Progress::Download { what: what.to_string(), done, total: total.or(Some(done)) });
    Ok(())
}

// ───────────────────────────── контрольные суммы ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Checksum {
    Sha256(String),
    Sha1(String),
    Md5(String),
}

fn hash_file<D: Digest>(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = D::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn verify(path: &Path, sum: &Checksum) -> Result<()> {
    let (expected, actual) = match sum {
        Checksum::Sha256(e) => (e, hash_file::<sha2::Sha256>(path)?),
        Checksum::Sha1(e) => (e, hash_file::<sha1::Sha1>(path)?),
        Checksum::Md5(e) => (e, hash_file::<md5::Md5>(path)?),
    };
    if !expected.eq_ignore_ascii_case(&actual) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        bail!(tr!(
            format!("контрольная сумма {name} не совпала (ожидалось {expected}, получено {actual})"),
            format!("checksum mismatch for {name} (expected {expected}, got {actual})")
        ));
    }
    Ok(())
}

// ───────────────────────────── версии ─────────────────────────────

/// Релизная версия: только цифры и точки (без -rc, -pre, snapshot).
fn is_release(v: &str) -> bool {
    !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') && !v.starts_with('.') && !v.ends_with('.')
}

fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Оставляет релизы, без повторов, новые сверху.
fn newest_first(versions: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = versions.into_iter().filter(|v| is_release(v)).collect();
    v.sort_by_key(|s| std::cmp::Reverse(version_key(s)));
    v.dedup();
    v
}

/// Stable release versions, newest first.
pub fn list_versions(core: Core) -> Result<Vec<String>> {
    let versions = match core {
        Core::Paper => paper_versions(&get_bytes(&format!("{PAPER_API}/versions"))?)?,
        Core::Purpur => purpur_versions(&get_bytes(PURPUR_API)?)?,
        Core::Fabric => fabric_game_versions(&get_bytes(&format!("{FABRIC_API}/versions/game"))?)?,
        Core::Vanilla => mojang_releases(&get_bytes(MOJANG_MANIFEST)?)?,
    };
    if versions.is_empty() {
        bail!(tr!(
            format!("{} не вернул ни одной версии", core.label()),
            format!("{} returned no versions", core.label())
        ));
    }
    Ok(versions)
}

/// Что и куда скачать.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteFile {
    url: String,
    /// Имя файла на диске.
    name: String,
    size: Option<u64>,
    checksum: Option<Checksum>,
    /// Для сообщений о прогрессе, например «Paper 26.3 #249».
    label: String,
}

// Paper (Fill API v3)

#[derive(Deserialize)]
struct PaperVersions {
    versions: Vec<PaperVersionEntry>,
}
#[derive(Deserialize)]
struct PaperVersionEntry {
    version: PaperVersion,
}
#[derive(Deserialize)]
struct PaperVersion {
    id: String,
    #[serde(default)]
    java: Option<PaperJava>,
}
#[derive(Deserialize)]
struct PaperJava {
    version: PaperJavaVersion,
}
#[derive(Deserialize)]
struct PaperJavaVersion {
    minimum: u32,
}
#[derive(Deserialize)]
struct PaperBuild {
    id: u64,
    #[serde(default)]
    downloads: HashMap<String, PaperDownload>,
}
#[derive(Deserialize)]
struct PaperDownload {
    name: String,
    checksums: PaperChecksums,
    size: Option<u64>,
    url: String,
}
#[derive(Deserialize)]
struct PaperChecksums {
    sha256: Option<String>,
}

fn paper_versions(body: &[u8]) -> Result<Vec<String>> {
    let v: PaperVersions = parse_json(body, "PaperMC")?;
    Ok(newest_first(v.versions.into_iter().map(|e| e.version.id)))
}

/// Минимальная Java по данным PaperMC.
fn paper_java_minimum(body: &[u8], version: &str) -> Option<u32> {
    let v: PaperVersions = serde_json::from_slice(body).ok()?;
    v.versions.into_iter().find(|e| e.version.id == version)?.version.java.map(|j| j.version.minimum)
}

fn paper_latest(body: &[u8], version: &str) -> Result<Option<RemoteFile>> {
    let builds: Vec<PaperBuild> = parse_json(body, "PaperMC")?;
    let Some(b) = builds.into_iter().filter(|b| b.downloads.contains_key("server:default")).max_by_key(|b| b.id) else {
        return Ok(None);
    };
    let id = b.id;
    let d = b.downloads.into_iter().find(|(k, _)| k == "server:default").map(|(_, d)| d).context("server:default")?;
    Ok(Some(RemoteFile {
        url: d.url,
        name: d.name,
        size: d.size,
        checksum: d.checksums.sha256.map(Checksum::Sha256),
        label: format!("Paper {version} #{id}"),
    }))
}

fn paper_file(version: &str) -> Result<RemoteFile> {
    let base = format!("{PAPER_API}/versions/{version}/builds");
    if let Some(f) = paper_latest(&get_bytes(&format!("{base}?channel=STABLE"))?, version)? {
        return Ok(f);
    }
    // Для свежей версии стабильных сборок может ещё не быть — берём последнюю любую.
    paper_latest(&get_bytes(&base)?, version)?
        .with_context(|| tr!(format!("у Paper нет сборок для {version}"), format!("Paper has no builds for {version}")))
}

// Purpur (v2)

#[derive(Deserialize)]
struct PurpurProject {
    versions: Vec<String>,
}
#[derive(Deserialize)]
struct PurpurBuild {
    build: String,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    md5: Option<String>,
}

fn purpur_versions(body: &[u8]) -> Result<Vec<String>> {
    let p: PurpurProject = parse_json(body, "Purpur")?;
    Ok(newest_first(p.versions))
}

fn purpur_latest(body: &[u8], version: &str) -> Result<RemoteFile> {
    let b: PurpurBuild = parse_json(body, "Purpur")?;
    if b.result.as_deref().is_some_and(|r| r != "SUCCESS") {
        bail!(tr!(
            format!("последняя сборка Purpur {version} неуспешна"),
            format!("latest Purpur {version} build has failed")
        ));
    }
    Ok(RemoteFile {
        url: format!("{PURPUR_API}/{version}/{}/download", b.build),
        name: format!("purpur-{version}-{}.jar", b.build),
        size: None,
        checksum: b.md5.map(Checksum::Md5),
        label: format!("Purpur {version} #{}", b.build),
    })
}

fn purpur_file(version: &str) -> Result<RemoteFile> {
    purpur_latest(&get_bytes(&format!("{PURPUR_API}/{version}/latest"))?, version)
}

// Fabric (meta v2)

#[derive(Deserialize)]
struct FabricVersion {
    version: String,
    stable: bool,
}
#[derive(Deserialize)]
struct FabricLoaderEntry {
    loader: FabricVersion,
}

fn fabric_game_versions(body: &[u8]) -> Result<Vec<String>> {
    let v: Vec<FabricVersion> = parse_json(body, "Fabric")?;
    Ok(newest_first(v.into_iter().filter(|v| v.stable).map(|v| v.version)))
}

/// Последний стабильный загрузчик для версии игры (ответ `/versions/loader/{game}`).
fn fabric_loader(body: &[u8]) -> Result<Option<String>> {
    let v: Vec<FabricLoaderEntry> = parse_json(body, "Fabric")?;
    Ok(v.iter().find(|e| e.loader.stable).or(v.first()).map(|e| e.loader.version.clone()))
}

fn fabric_installer(body: &[u8]) -> Result<Option<String>> {
    let v: Vec<FabricVersion> = parse_json(body, "Fabric")?;
    Ok(v.iter().find(|e| e.stable).or(v.first()).map(|e| e.version.clone()))
}

fn fabric_file(version: &str) -> Result<RemoteFile> {
    let unsupported =
        || tr!(format!("Fabric не поддерживает версию {version}"), format!("Fabric does not support {version}"));
    let loader =
        fabric_loader(&get_bytes(&format!("{FABRIC_API}/versions/loader/{version}"))?)?.with_context(unsupported)?;
    let installer = fabric_installer(&get_bytes(&format!("{FABRIC_API}/versions/installer"))?)?
        .with_context(|| tr!("нет версий Fabric installer", "no Fabric installer versions"))?;
    Ok(RemoteFile {
        url: format!("{FABRIC_API}/versions/loader/{version}/{loader}/{installer}/server/jar"),
        name: format!("fabric-server-mc.{version}-loader.{loader}-launcher.{installer}.jar"),
        size: None,
        checksum: None,
        label: format!("Fabric {version} (loader {loader})"),
    })
}

// Mojang (piston-meta)

#[derive(Deserialize)]
struct MojangManifest {
    versions: Vec<MojangManifestEntry>,
}
#[derive(Deserialize)]
struct MojangManifestEntry {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
    #[serde(default)]
    sha1: Option<String>,
}
#[derive(Deserialize)]
struct MojangVersion {
    #[serde(default, rename = "javaVersion")]
    java_version: Option<MojangJava>,
    #[serde(default)]
    downloads: HashMap<String, MojangDownload>,
}
#[derive(Deserialize)]
struct MojangJava {
    #[serde(rename = "majorVersion")]
    major_version: u32,
}
#[derive(Deserialize)]
struct MojangDownload {
    sha1: String,
    size: Option<u64>,
    url: String,
}

fn mojang_releases(body: &[u8]) -> Result<Vec<String>> {
    let m: MojangManifest = parse_json(body, "Mojang")?;
    Ok(newest_first(m.versions.into_iter().filter(|v| v.kind == "release").map(|v| v.id)))
}

/// URL и sha1 JSON-описания версии из манифеста.
fn mojang_version_url(body: &[u8], version: &str) -> Result<Option<(String, Option<String>)>> {
    let m: MojangManifest = parse_json(body, "Mojang")?;
    Ok(m.versions.into_iter().find(|v| v.id == version).map(|v| (v.url, v.sha1)))
}

struct MojangInfo {
    /// `javaVersion.majorVersion`; у очень старых версий поля нет — тогда Java 8.
    java: u32,
    server: Option<RemoteFile>,
}

fn mojang_info(body: &[u8], version: &str) -> Result<MojangInfo> {
    let v: MojangVersion = parse_json(body, "Mojang")?;
    let server = v.downloads.into_iter().find(|(k, _)| k == "server").map(|(_, d)| RemoteFile {
        url: d.url,
        name: format!("minecraft_server.{version}.jar"),
        size: d.size,
        checksum: Some(Checksum::Sha1(d.sha1)),
        label: format!("Minecraft {version}"),
    });
    Ok(MojangInfo { java: v.java_version.map_or(8, |j| j.major_version), server })
}

fn fetch_mojang(version: &str) -> Result<MojangInfo> {
    let (url, sha1) = mojang_version_url(&get_bytes(MOJANG_MANIFEST)?, version)?.with_context(|| {
        tr!(format!("Mojang не знает версию {version}"), format!("Mojang has no version {version}"))
    })?;
    let body = get_bytes(&url)?;
    if let Some(expected) = sha1 {
        let actual: String = sha1::Sha1::digest(&body).iter().map(|b| format!("{b:02x}")).collect();
        if !expected.eq_ignore_ascii_case(&actual) {
            bail!(tr!(
                format!("контрольная сумма описания версии {version} не совпала"),
                format!("checksum mismatch for the {version} version manifest")
            ));
        }
    }
    mojang_info(&body, version)
}

// ───────────────────────────── Java ─────────────────────────────

/// Основная версия Java из вывода `java -version` (он идёт в stderr): `"1.8.0_391"` → 8, `"25.0.1"` → 25.
pub fn parse_java_major(output: &str) -> Option<u32> {
    let line = output.lines().find(|l| l.contains(" version \""))?;
    let quoted = line.split('"').nth(1)?;
    let mut parts = quoted.split(|c: char| !c.is_ascii_digit());
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 { parts.next()?.parse().ok() } else { Some(first) }
}

fn java_major(java: &Path) -> Option<u32> {
    let out = Command::new(java).arg("-version").stdin(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    parse_java_major(&text)
}

/// Ищет программу в PATH.
fn which(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Версия системной `java` из PATH, если она есть.
fn system_java_major() -> Option<u32> {
    let path = which("java")?;
    // В macOS /usr/bin/java — заглушка: без установленной JDK она открывает окно «установите Java».
    if cfg!(target_os = "macos") && path == Path::new("/usr/bin/java") {
        let ok = Command::new("/usr/libexec/java_home")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return None;
        }
    }
    java_major(Path::new("java"))
}

fn adoptium_os() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("mac")
    } else if cfg!(windows) {
        Some("windows")
    } else if cfg!(target_os = "linux") {
        Some(if Path::new("/etc/alpine-release").exists() { "alpine-linux" } else { "linux" })
    } else {
        None
    }
}

fn adoptium_arch() -> Option<&'static str> {
    if cfg!(target_arch = "x86_64") {
        Some("x64")
    } else if cfg!(target_arch = "aarch64") {
        Some("aarch64")
    } else {
        None
    }
}

#[derive(Deserialize)]
struct AdoptiumAsset {
    binary: AdoptiumBinary,
    #[serde(default)]
    release_name: Option<String>,
}
#[derive(Deserialize)]
struct AdoptiumBinary {
    package: AdoptiumPackage,
}
#[derive(Deserialize)]
struct AdoptiumPackage {
    link: String,
    name: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    checksum: Option<String>,
}
#[derive(Deserialize)]
struct AdoptiumReleases {
    available_lts_releases: Vec<u32>,
}

fn adoptium_asset(body: &[u8], feature: u32) -> Result<Option<RemoteFile>> {
    let assets: Vec<AdoptiumAsset> = parse_json(body, "Adoptium")?;
    Ok(assets.into_iter().next().map(|a| {
        let release = a.release_name.unwrap_or_else(|| feature.to_string());
        RemoteFile {
            url: a.binary.package.link,
            name: a.binary.package.name,
            size: a.binary.package.size,
            checksum: a.binary.package.checksum.map(Checksum::Sha256),
            label: format!("Java {feature} (Temurin JRE {release})"),
        }
    }))
}

/// Версии Java, которые пробуем скачать: сама нужная, затем LTS новее неё
/// (JRE для не-LTS вроде 16 и для Java 8 на Apple Silicon у Adoptium нет).
fn java_candidates(required: u32, lts: &[u32]) -> Vec<u32> {
    let mut v = vec![required];
    let mut newer: Vec<u32> = lts.iter().copied().filter(|&n| n > required).collect();
    newer.sort_unstable();
    v.extend(newer);
    v.dedup();
    v
}

fn find_jre(required: u32) -> Result<(u32, RemoteFile)> {
    let (Some(os), Some(arch)) = (adoptium_os(), adoptium_arch()) else {
        bail!(tr!(
            format!("для этой системы нет готовой Java — установите Java {required}+ вручную"),
            format!("no prebuilt Java for this system — install Java {required}+ manually")
        ));
    };
    let lts = get_json::<AdoptiumReleases>(&format!("{ADOPTIUM_API}/info/available_releases"))
        .map(|r| r.available_lts_releases)
        .unwrap_or_else(|_| vec![17, 21, 25]);
    for feature in java_candidates(required, &lts) {
        let url = format!(
            "{ADOPTIUM_API}/assets/latest/{feature}/hotspot?architecture={arch}&image_type=jre&os={os}&vendor=eclipse"
        );
        if let Some(f) = adoptium_asset(&get_bytes(&url)?, feature)? {
            return Ok((feature, f));
        }
    }
    bail!(tr!(
        format!("Adoptium не нашёл Java {required}+ для {os}/{arch} — установите её вручную"),
        format!("Adoptium has no Java {required}+ for {os}/{arch} — install it manually")
    ))
}

/// Распаковывает .tar.gz или .zip в `dest`.
fn extract(archive: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let name = archive.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
    let file = std::fs::File::open(archive)?;
    if name.ends_with(".zip") {
        zip::ZipArchive::new(file)?.extract(dest)?;
    } else {
        tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(dest)?;
    }
    Ok(())
}

/// Путь к `java` внутри распакованной JRE: `<jdk>/bin/java` или `<jdk>/Contents/Home/bin/java` (macOS).
fn find_java_binary(root: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    let mut dirs = vec![root.to_path_buf()];
    let mut subdirs: Vec<PathBuf> =
        std::fs::read_dir(root).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    subdirs.sort();
    dirs.extend(subdirs);
    dirs.iter().flat_map(|d| [d.join("bin").join(exe), d.join("Contents/Home/bin").join(exe)]).find(|p| p.is_file())
}

// ───────────────────────────── файлы сервера ─────────────────────────────

fn is_ignorable(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini")
}

/// Папка пригодна для нового сервера: не существует или пуста (не считая .DS_Store и т. п.).
/// Возвращает `true`, если папка уже существует.
fn check_target_dir(dir: &Path) -> Result<bool> {
    if !dir.exists() {
        return Ok(false);
    }
    if !dir.is_dir() {
        bail!(tr!(
            format!("{} — это файл, а не папка", dir.display()),
            format!("{} is a file, not a folder", dir.display())
        ));
    }
    let busy: Vec<String> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !is_ignorable(n))
        .collect();
    if !busy.is_empty() {
        let n = busy.len();
        bail!(tr!(
            format!("папка {} не пуста ({n} файл(ов)) — выберите пустую или новую папку", dir.display()),
            format!("folder {} is not empty ({n} item(s)) — pick an empty or new folder", dir.display())
        ));
    }
    Ok(true)
}

/// Значение в кавычках для sh, если без них нельзя.
fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-/+=:".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn bat_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || r"._-\+=:".contains(c)) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('%', "%%"))
    }
}

fn memory_args(memory_mb: u32) -> [String; 2] {
    [format!("-Xms{}M", memory_mb.min(1024)), format!("-Xmx{memory_mb}M")]
}

/// Текст start.sh (`windows = false`) или start.bat. `java` — `java` или путь относительно папки сервера.
pub fn start_script(java: &str, memory_mb: u32, jar: &str, nogui: &str, windows: bool) -> String {
    let [xms, xmx] = memory_args(memory_mb);
    if windows {
        let java = java.replace('/', "\\");
        format!(
            "@echo off\r\nrem Generated by mctui\r\ncd /d \"%~dp0\"\r\n{} {xms} {xmx} -jar {} {nogui} %*\r\n",
            bat_quote(&java),
            bat_quote(jar)
        )
    } else {
        format!(
            "#!/bin/sh\n# Generated by mctui\ncd \"$(dirname \"$0\")\" || exit 1\nexec {} {xms} {xmx} -jar {} {nogui} \"$@\"\n",
            sh_quote(java),
            sh_quote(jar)
        )
    }
}

fn eula_text(now: &str) -> String {
    format!(
        "#By changing the setting below to TRUE you are indicating your agreement to our EULA ({EULA_URL}).\n#{now}\neula=true\n"
    )
}

fn server_properties(port: u16) -> String {
    format!("#Minecraft server properties\nserver-port={port}\n")
}

/// Удаляет созданное, если `create` не дошёл до конца.
struct Cleanup {
    dir: PathBuf,
    created_dir: bool,
    paths: Vec<PathBuf>,
    armed: bool,
}

impl Cleanup {
    fn track(&mut self, p: PathBuf) -> PathBuf {
        self.paths.push(p.clone());
        p
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for p in self.paths.iter().rev() {
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(p);
            } else {
                let _ = std::fs::remove_file(p);
            }
        }
        if self.created_dir {
            // Только пустую: всё своё уже удалили.
            let _ = std::fs::remove_dir(&self.dir);
        }
    }
}

/// Скачивает файл во временный `<name>.part`, проверяет сумму и переименовывает.
fn fetch_file(
    file: &RemoteFile,
    dir: &Path,
    guard: &mut Cleanup,
    progress: &mut dyn FnMut(Progress),
) -> Result<PathBuf> {
    let part = guard.track(dir.join(format!("{}.part", file.name)));
    download(&file.url, &part, &file.label, file.size, progress)?;
    if let Some(sum) = &file.checksum {
        progress(Progress::Step(tr!("Проверяю контрольную сумму…", "Verifying checksum…").to_string()));
        verify(&part, sum)?;
    }
    let dest = guard.track(dir.join(&file.name));
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

fn server_file(core: Core, version: &str, mojang: &MojangInfo) -> Result<RemoteFile> {
    match core {
        Core::Paper => paper_file(version),
        Core::Purpur => purpur_file(version),
        Core::Fabric => fabric_file(version),
        Core::Vanilla => mojang.server.clone().with_context(|| {
            tr!(format!("у Minecraft {version} нет серверного jar"), format!("Minecraft {version} has no server jar"))
        }),
    }
}

pub fn create(opts: &CreateOptions, progress: &mut dyn FnMut(Progress)) -> Result<Created> {
    if !opts.accept_eula {
        bail!(tr!(
            format!("нужно принять EULA Minecraft ({EULA_URL}) — без этого сервер не запустится"),
            format!("you must accept the Minecraft EULA ({EULA_URL}) — the server won't start without it")
        ));
    }
    if opts.memory_mb < MIN_MEMORY_MB {
        bail!(tr!(
            format!("слишком мало памяти: {} МБ (нужно хотя бы {MIN_MEMORY_MB})", opts.memory_mb),
            format!("too little memory: {} MB (at least {MIN_MEMORY_MB} needed)", opts.memory_mb)
        ));
    }
    if opts.port == 0 {
        bail!(tr!("порт не может быть 0", "port cannot be 0"));
    }
    let version = opts.version.trim();
    if version.is_empty() {
        bail!(tr!("не выбрана версия", "no version selected"));
    }
    let core = opts.core;
    let dir = opts.dir.clone();

    progress(Progress::Step(tr!("Проверяю папку…", "Checking the folder…").to_string()));
    let existed = check_target_dir(&dir)?;
    if !existed {
        std::fs::create_dir_all(&dir).with_context(|| {
            tr!(
                format!("не удалось создать папку {}", dir.display()),
                format!("cannot create folder {}", dir.display())
            )
        })?;
    }
    let mut guard = Cleanup { dir: dir.clone(), created_dir: !existed, paths: Vec::new(), armed: true };

    progress(Progress::Step(tr!(
        format!("Узнаю, какая Java нужна для Minecraft {version}…"),
        format!("Checking which Java Minecraft {version} needs…")
    )));
    let mojang = fetch_mojang(version)?;
    let mut required = mojang.java;
    if core == Core::Paper
        && let Ok(body) = get_bytes(&format!("{PAPER_API}/versions"))
        && let Some(min) = paper_java_minimum(&body, version)
    {
        required = required.max(min);
    }

    progress(Progress::Step(tr!(
        format!("Ищу последнюю сборку {} {version}…", core.label()),
        format!("Looking up the latest {} {version} build…", core.label())
    )));
    let remote = server_file(core, version, &mojang)?;
    fetch_file(&remote, &dir, &mut guard, progress)?;
    let jar = remote.name.clone();

    // Java
    let (java_cmd, java_path) = match system_java_major() {
        Some(major) if major >= required => {
            progress(Progress::Step(tr!(
                format!("Использую системную Java {major} (нужна {required}+)"),
                format!("Using system Java {major} ({required}+ required)")
            )));
            ("java".to_string(), PathBuf::from("java"))
        }
        found => {
            let why = match found {
                Some(m) => tr!(format!("системная Java {m} слишком старая"), format!("system Java {m} is too old")),
                None => tr!("Java не найдена".to_string(), "Java not found".to_string()),
            };
            progress(Progress::Step(tr!(
                format!("{why} — скачиваю Java {required}…"),
                format!("{why} — downloading Java {required}…")
            )));
            let (feature, jre) = find_jre(required)?;
            let jre_dir = guard.track(dir.join("jre"));
            std::fs::create_dir_all(&jre_dir)?;
            let archive = fetch_file(&jre, &jre_dir, &mut guard, progress)?;
            progress(Progress::Step(tr!("Распаковываю Java…", "Unpacking Java…").to_string()));
            extract(&archive, &jre_dir)
                .with_context(|| tr!("не удалось распаковать Java", "failed to unpack Java").to_string())?;
            let _ = std::fs::remove_file(&archive);
            let bin = find_java_binary(&jre_dir)
                .with_context(|| tr!("в архиве Java нет bin/java", "no bin/java in the Java archive").to_string())?;
            match java_major(&bin) {
                Some(m) if m >= required => {}
                _ => bail!(tr!(
                    format!("скачанная Java {feature} не запускается"),
                    format!("downloaded Java {feature} does not run")
                )),
            }
            let rel = bin.strip_prefix(&dir).unwrap_or(&bin).to_string_lossy().replace('\\', "/");
            (rel, bin)
        }
    };

    progress(Progress::Step(
        tr!(
            "Записываю eula.txt, server.properties и скрипт запуска…",
            "Writing eula.txt, server.properties and the start script…"
        )
        .to_string(),
    ));
    let now = chrono::Local::now().format("%a %b %d %H:%M:%S %Z %Y").to_string();
    std::fs::write(guard.track(dir.join("eula.txt")), eula_text(&now))?;
    std::fs::write(guard.track(dir.join("server.properties")), server_properties(opts.port))?;
    let windows = cfg!(windows);
    let script_name = if windows { "start.bat" } else { "start.sh" };
    let script = guard.track(dir.join(script_name));
    std::fs::write(&script, start_script(&java_cmd, opts.memory_mb, &jar, core.nogui(), windows))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    }

    guard.armed = false;
    let command =
        if windows { vec!["cmd".into(), "/C".into(), script_name.into()] } else { vec![format!("./{script_name}")] };
    progress(Progress::Step(tr!(
        format!("Готово: {} {version} в {}", core.label(), dir.display()),
        format!("Done: {} {version} in {}", core.label(), dir.display())
    )));
    Ok(Created { dir, command, java: java_path, jar })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mctui-create-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    // Фикстуры — укороченные реальные ответы API (ноябрь 2026).

    const PAPER_VERSIONS: &str = r#"{"versions":[
        {"version":{"id":"26.3","support":{"status":"SUPPORTED"},"java":{"version":{"minimum":25},"flags":{"recommended":["-XX:+UseG1GC"]}}},"builds":[248,249]},
        {"version":{"id":"26.3-rc-3","support":{"status":"UNSUPPORTED","end":"2026-09-15"},"java":{"version":{"minimum":25}}},"builds":[1]},
        {"version":{"id":"1.21.11","support":{"status":"UNSUPPORTED"},"java":{"version":{"minimum":21}}},"builds":[132]},
        {"version":{"id":"1.21.11-pre5","support":{"status":"UNSUPPORTED"},"java":{"version":{"minimum":21}}},"builds":[2]},
        {"version":{"id":"1.8.8","support":{"status":"UNSUPPORTED"},"java":{"version":{"minimum":8}}},"builds":[445]}]}"#;

    const PAPER_BUILDS: &str = r#"[
        {"id":249,"time":"2026-11-11T18:01:48Z","channel":"STABLE","commits":[{"sha":"1a995d6","time":"2026-11-11T18:01:29Z","message":"Set correct tag registrar cause on reload (#14459)"}],
         "downloads":{"server:default":{"name":"paper-26.3-249.jar","checksums":{"sha256":"4c734aeb4da98845934af1e5481f0df7c80ad3ca50d272319ffe2b0e8a187c8a"},"size":55281059,"url":"https://fill-data.papermc.io/v1/objects/4c734aeb4da98845934af1e5481f0df7c80ad3ca50d272319ffe2b0e8a187c8a/paper-26.3-249.jar"}}},
        {"id":248,"time":"2026-11-10T11:35:52Z","channel":"STABLE","commits":[],
         "downloads":{"server:default":{"name":"paper-26.3-248.jar","checksums":{"sha256":"352dbbf965946456a81305f4c4a8d39cb7a8e55a0b0126b417a24efd944b92c2"},"size":55281327,"url":"https://fill-data.papermc.io/v1/objects/352dbbf9/paper-26.3-248.jar"}}}]"#;

    const PURPUR_PROJECT: &str = r#"{"project":"purpur","metadata":{"current":"26.3"},"versions":["1.14.1","1.20.6","1.21","1.21.11","26.1.2","26.2","26.3"]}"#;

    const PURPUR_LATEST: &str = r#"{"project":"purpur","version":"26.3","build":"2658","result":"SUCCESS","timestamp":1794455508533,"duration":481667,"commits":[{"author":"granny","email":"granny@purpurmc.org","description":"Updated Upstream (Paper)","hash":"e48fed00","timestamp":1794455488000}],"metadata":{},"md5":"54eae68933789d1f984d7ba29fe4b01d"}"#;

    const FABRIC_GAME: &str = r#"[{"version":"26.4-snapshot-8","stable":false},{"version":"26.3","stable":true},{"version":"26.3-rc-1","stable":false},{"version":"26.2","stable":true},{"version":"1.21.11","stable":true}]"#;

    const FABRIC_LOADER: &str = r#"[
        {"loader":{"separator":".","build":0,"maven":"net.fabricmc:fabric-loader:0.21.0-beta.1","version":"0.21.0-beta.1","stable":false},"intermediary":{"maven":"net.fabricmc:intermediary:0.0.0","version":"0.0.0","stable":true},"launcherMeta":{"version":2,"min_java_version":8}},
        {"loader":{"separator":".","build":1,"maven":"net.fabricmc:fabric-loader:0.20.1","version":"0.20.1","stable":true},"intermediary":{"maven":"net.fabricmc:intermediary:0.0.0","version":"0.0.0","stable":true},"launcherMeta":{"version":2,"min_java_version":8}}]"#;

    const FABRIC_INSTALLER: &str = r#"[{"url":"https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.2/fabric-installer-1.1.2.jar","maven":"net.fabricmc:fabric-installer:1.1.2","version":"1.1.2","stable":true},{"url":"https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar","maven":"net.fabricmc:fabric-installer:1.1.1","version":"1.1.1","stable":false}]"#;

    const MOJANG_MANIFEST_JSON: &str = r#"{"latest":{"release":"26.3","snapshot":"26.4-snapshot-8"},"versions":[
        {"id":"26.4-snapshot-8","type":"snapshot","url":"https://piston-meta.mojang.com/v1/packages/46f6dc6a5cefe084af0ce2ecf7f433c1222d4dd1/26.4-snapshot-8.json","time":"2026-11-10T14:28:50+00:00","releaseTime":"2026-11-10T14:20:05+00:00","sha1":"46f6dc6a5cefe084af0ce2ecf7f433c1222d4dd1","complianceLevel":1},
        {"id":"26.3","type":"release","url":"https://piston-meta.mojang.com/v1/packages/9f67e10ed013deeaad20ac3c800652765d493408/26.3.json","time":"2026-11-10T12:53:02+00:00","releaseTime":"2026-11-03T13:08:56+00:00","sha1":"9f67e10ed013deeaad20ac3c800652765d493408","complianceLevel":1},
        {"id":"1.21.11","type":"release","url":"https://piston-meta.mojang.com/v1/packages/x/1.21.11.json","sha1":"abc","complianceLevel":1},
        {"id":"b1.7.3","type":"old_beta","url":"https://piston-meta.mojang.com/v1/packages/y/b1.7.3.json","sha1":"def","complianceLevel":0}]}"#;

    const MOJANG_VERSION: &str = r#"{"assetIndex":{"id":"30"},"assets":"30","complianceLevel":1,
        "downloads":{"client":{"sha1":"aaaa","size":1,"url":"https://piston-data.mojang.com/v1/objects/aaaa/client.jar"},
                     "server":{"sha1":"33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c","size":62294556,"url":"https://piston-data.mojang.com/v1/objects/33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c/server.jar"}},
        "id":"26.3","javaVersion":{"component":"java-runtime-epsilon","majorVersion":25},"mainClass":"net.minecraft.client.main.Main","type":"release"}"#;

    const ADOPTIUM_ASSETS: &str = r#"[{"binary":{"architecture":"aarch64","download_count":31271,"heap_size":"normal","image_type":"jre",
        "installer":{"checksum":"6405c2b8","link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jre_aarch64_mac_hotspot_25.0.5_8.pkg","name":"OpenJDK25U-jre_aarch64_mac_hotspot_25.0.5_8.pkg","size":56667686},
        "jvm_impl":"hotspot","os":"mac",
        "package":{"checksum":"72ebd76b502c849e88dea39afae51baf5f82f2b5b1b6389c28ea707b20451bce","checksum_link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jre_aarch64_mac_hotspot_25.0.5_8.tar.gz.sha256.txt","download_count":27839,"link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jre_aarch64_mac_hotspot_25.0.5_8.tar.gz","name":"OpenJDK25U-jre_aarch64_mac_hotspot_25.0.5_8.tar.gz","size":56298727},
        "project":"jdk","scm_ref":"jdk-25.0.5+8_adopt","updated_at":"2026-10-22T08:22:19Z"},
        "release_link":"https://github.com/adoptium/temurin25-binaries/releases/tag/jdk-25.0.5%2B8","release_name":"jdk-25.0.5+8","vendor":"eclipse",
        "version":{"build":8,"major":25,"minor":0,"openjdk_version":"25.0.5+8-LTS","optional":"LTS","security":5,"semver":"25.0.5+8.0.LTS"}}]"#;

    #[test]
    fn core_labels() {
        assert_eq!(Core::ALL.map(Core::label), ["Paper", "Purpur", "Fabric", "Vanilla"]);
        for c in Core::ALL {
            assert!(!c.description().is_empty());
        }
    }

    #[test]
    fn release_filter_and_order() {
        assert!(is_release("1.21.11") && is_release("26.3") && is_release("1.8"));
        assert!(!is_release("26.3-rc-3") && !is_release("26.4-snapshot-8") && !is_release("b1.7.3") && !is_release(""));
        let v = newest_first(["1.21.11", "26.1.2", "1.8.8", "26.3", "1.21.9", "26.3"].map(String::from));
        assert_eq!(v, ["26.3", "26.1.2", "1.21.11", "1.21.9", "1.8.8"]);
    }

    #[test]
    fn paper_json() {
        assert_eq!(paper_versions(PAPER_VERSIONS.as_bytes()).unwrap(), ["26.3", "1.21.11", "1.8.8"]);
        assert_eq!(paper_java_minimum(PAPER_VERSIONS.as_bytes(), "26.3"), Some(25));
        assert_eq!(paper_java_minimum(PAPER_VERSIONS.as_bytes(), "1.21.11"), Some(21));
        assert_eq!(paper_java_minimum(PAPER_VERSIONS.as_bytes(), "9.9"), None);
        let f = paper_latest(PAPER_BUILDS.as_bytes(), "26.3").unwrap().unwrap();
        assert_eq!(f.name, "paper-26.3-249.jar");
        assert_eq!(f.size, Some(55281059));
        assert!(f.url.ends_with("/paper-26.3-249.jar"));
        assert_eq!(
            f.checksum,
            Some(Checksum::Sha256("4c734aeb4da98845934af1e5481f0df7c80ad3ca50d272319ffe2b0e8a187c8a".into()))
        );
        assert_eq!(f.label, "Paper 26.3 #249");
        assert_eq!(paper_latest(b"[]", "26.3").unwrap(), None);
    }

    #[test]
    fn purpur_json() {
        assert_eq!(purpur_versions(PURPUR_PROJECT.as_bytes()).unwrap()[..3], ["26.3", "26.2", "26.1.2"]);
        let f = purpur_latest(PURPUR_LATEST.as_bytes(), "26.3").unwrap();
        assert_eq!(f.url, "https://api.purpurmc.org/v2/purpur/26.3/2658/download");
        assert_eq!(f.name, "purpur-26.3-2658.jar");
        assert_eq!(f.checksum, Some(Checksum::Md5("54eae68933789d1f984d7ba29fe4b01d".into())));
        let failed = PURPUR_LATEST.replace("\"SUCCESS\"", "\"FAILURE\"");
        assert!(purpur_latest(failed.as_bytes(), "26.3").is_err());
    }

    #[test]
    fn fabric_json() {
        assert_eq!(fabric_game_versions(FABRIC_GAME.as_bytes()).unwrap(), ["26.3", "26.2", "1.21.11"]);
        assert_eq!(fabric_loader(FABRIC_LOADER.as_bytes()).unwrap().as_deref(), Some("0.20.1"));
        assert_eq!(fabric_loader(b"[]").unwrap(), None);
        assert_eq!(fabric_installer(FABRIC_INSTALLER.as_bytes()).unwrap().as_deref(), Some("1.1.2"));
    }

    #[test]
    fn mojang_json() {
        assert_eq!(mojang_releases(MOJANG_MANIFEST_JSON.as_bytes()).unwrap(), ["26.3", "1.21.11"]);
        let (url, sha1) = mojang_version_url(MOJANG_MANIFEST_JSON.as_bytes(), "26.3").unwrap().unwrap();
        assert!(url.ends_with("/26.3.json"));
        assert_eq!(sha1.as_deref(), Some("9f67e10ed013deeaad20ac3c800652765d493408"));
        assert!(mojang_version_url(MOJANG_MANIFEST_JSON.as_bytes(), "9.9").unwrap().is_none());
        let info = mojang_info(MOJANG_VERSION.as_bytes(), "26.3").unwrap();
        assert_eq!(info.java, 25);
        let s = info.server.unwrap();
        assert_eq!(s.name, "minecraft_server.26.3.jar");
        assert_eq!(s.size, Some(62294556));
        assert_eq!(s.checksum, Some(Checksum::Sha1("33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c".into())));
        // Нет javaVersion → Java 8; нет серверного jar.
        let old = mojang_info(br#"{"downloads":{"client":{"sha1":"a","size":1,"url":"u"}}}"#, "1.0").unwrap();
        assert_eq!(old.java, 8);
        assert!(old.server.is_none());
    }

    #[test]
    fn adoptium_json() {
        let f = adoptium_asset(ADOPTIUM_ASSETS.as_bytes(), 25).unwrap().unwrap();
        assert_eq!(f.name, "OpenJDK25U-jre_aarch64_mac_hotspot_25.0.5_8.tar.gz");
        assert_eq!(f.size, Some(56298727));
        assert!(f.url.ends_with(".tar.gz"));
        assert_eq!(
            f.checksum,
            Some(Checksum::Sha256("72ebd76b502c849e88dea39afae51baf5f82f2b5b1b6389c28ea707b20451bce".into()))
        );
        assert_eq!(f.label, "Java 25 (Temurin JRE jdk-25.0.5+8)");
        assert_eq!(adoptium_asset(b"[]", 16).unwrap(), None);
        assert_eq!(java_candidates(16, &[8, 11, 17, 21, 25]), [16, 17, 21, 25]);
        assert_eq!(java_candidates(25, &[8, 11, 17, 21, 25]), [25]);
        assert_eq!(java_candidates(8, &[25, 8, 11]), [8, 11, 25]);
    }

    #[test]
    fn java_version_output() {
        let oracle =
            "java version \"21.0.2\" 2024-01-16 LTS\nJava(TM) SE Runtime Environment (build 21.0.2+13-LTS-58)\n";
        assert_eq!(parse_java_major(oracle), Some(21));
        assert_eq!(
            parse_java_major("openjdk version \"25.0.5\" 2026-10-21 LTS\nOpenJDK Runtime Environment"),
            Some(25)
        );
        assert_eq!(parse_java_major("java version \"1.8.0_391\"\nJava(TM) SE Runtime Environment"), Some(8));
        assert_eq!(parse_java_major("openjdk version \"17\" 2021-09-14"), Some(17));
        assert_eq!(parse_java_major("openjdk version \"26-ea\" 2026-03-17"), Some(26));
        assert_eq!(
            parse_java_major(
                "Picked up JAVA_TOOL_OPTIONS: -Dfile.encoding=UTF-8\nopenjdk version \"11.0.2\" 2019-01-15"
            ),
            Some(11)
        );
        assert_eq!(parse_java_major("The operation couldn’t be completed. Unable to locate a Java Runtime."), None);
        assert_eq!(parse_java_major(""), None);
    }

    #[test]
    fn content_length_after_redirects() {
        let h = "HTTP/2 307\r\ncontent-length: 0\r\nlocation: x\r\n\r\nHTTP/2 200\r\nContent-Length: 56298727\r\n\r\n";
        assert_eq!(parse_content_length(h), Some(56298727));
        assert_eq!(parse_content_length("HTTP/2 200\r\ntransfer-encoding: chunked\r\n"), None);
    }

    #[test]
    fn scripts() {
        let sh = start_script("java", 4096, "paper-26.3-249.jar", "--nogui", false);
        assert_eq!(
            sh,
            "#!/bin/sh\n# Generated by mctui\ncd \"$(dirname \"$0\")\" || exit 1\n\
             exec java -Xms1024M -Xmx4096M -jar paper-26.3-249.jar --nogui \"$@\"\n"
        );
        let sh = start_script("jre/jdk-25.0.5+8-jre/Contents/Home/bin/java", 768, "a b.jar", "nogui", false);
        assert!(sh.contains("exec jre/jdk-25.0.5+8-jre/Contents/Home/bin/java -Xms768M -Xmx768M -jar 'a b.jar' nogui"));
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        let bat = start_script("jre/jdk-25.0.5+8-jre/bin/java.exe", 2048, "server.jar", "nogui", true);
        assert_eq!(
            bat,
            "@echo off\r\nrem Generated by mctui\r\ncd /d \"%~dp0\"\r\n\
             jre\\jdk-25.0.5+8-jre\\bin\\java.exe -Xms1024M -Xmx2048M -jar server.jar nogui %*\r\n"
        );
        assert!(
            start_script("C:/Program Files/Java/bin/java.exe", 1024, "s.jar", "nogui", true)
                .contains("\"C:\\Program Files\\Java\\bin\\java.exe\" -Xms1024M -Xmx1024M")
        );
        assert_eq!(server_properties(25570), "#Minecraft server properties\nserver-port=25570\n");
        assert!(eula_text("now").ends_with("\neula=true\n"));
    }

    #[test]
    fn target_dir_check() {
        let d = tmp("dircheck");
        assert!(!check_target_dir(&d).unwrap());
        std::fs::create_dir_all(&d).unwrap();
        assert!(check_target_dir(&d).unwrap());
        std::fs::write(d.join(".DS_Store"), b"").unwrap();
        assert!(check_target_dir(&d).unwrap());
        std::fs::write(d.join("world.txt"), b"").unwrap();
        assert!(check_target_dir(&d).is_err());
        assert!(check_target_dir(&d.join("world.txt")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn create_validates_before_touching_disk() {
        let d = tmp("validate");
        let mut opts = CreateOptions {
            core: Core::Paper,
            version: "26.3".into(),
            dir: d.clone(),
            memory_mb: 2048,
            port: 25565,
            accept_eula: false,
        };
        let mut sink = |_: Progress| {};
        assert!(create(&opts, &mut sink).is_err());
        opts.accept_eula = true;
        opts.memory_mb = 100;
        assert!(create(&opts, &mut sink).is_err());
        assert!(!d.exists());
    }

    #[test]
    fn checksums_and_cleanup() {
        let d = tmp("sums");
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("abc.bin");
        std::fs::write(&f, b"abc").unwrap();
        verify(&f, &Checksum::Sha256("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()))
            .unwrap();
        verify(&f, &Checksum::Sha1("A9993E364706816ABA3E25717850C26C9CD0D89D".into())).unwrap();
        verify(&f, &Checksum::Md5("900150983cd24fb0d6963f7d28e17f72".into())).unwrap();
        assert!(verify(&f, &Checksum::Md5("00000000000000000000000000000000".into())).is_err());

        // Cleanup удаляет только своё и созданную им папку.
        let sub = d.join("new");
        std::fs::create_dir_all(sub.join("jre/x")).unwrap();
        std::fs::write(sub.join("a.jar"), b"").unwrap();
        drop(Cleanup {
            dir: sub.clone(),
            created_dir: true,
            paths: vec![sub.join("jre"), sub.join("a.jar")],
            armed: true,
        });
        assert!(!sub.exists());
        assert!(f.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn extracts_tar_gz_and_zip_and_finds_java() {
        use std::io::Write;
        let d = tmp("extract");
        std::fs::create_dir_all(&d).unwrap();
        let exe = if cfg!(windows) { "java.exe" } else { "java" };

        // .tar.gz в стиле macOS: jdk/Contents/Home/bin/java
        let tgz = d.join("jre.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(std::fs::File::create(&tgz).unwrap(), flate2::Compression::fast());
            let mut t = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.set_size(2);
            h.set_mode(0o755);
            h.set_cksum();
            t.append_data(&mut h, format!("jdk-25-jre/Contents/Home/bin/{exe}"), &b"#!"[..]).unwrap();
            t.into_inner().unwrap().finish().unwrap();
        }
        let out = d.join("mac");
        extract(&tgz, &out).unwrap();
        assert_eq!(find_java_binary(&out), Some(out.join("jdk-25-jre/Contents/Home/bin").join(exe)));

        // .zip в стиле Windows/Linux: jdk/bin/java
        let zpath = d.join("jre.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zpath).unwrap());
            let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            z.start_file(format!("jdk-25-jre/bin/{exe}"), o).unwrap();
            z.write_all(b"MZ").unwrap();
            z.finish().unwrap();
        }
        let out = d.join("win");
        extract(&zpath, &out).unwrap();
        assert_eq!(find_java_binary(&out), Some(out.join("jdk-25-jre/bin").join(exe)));
        assert_eq!(find_java_binary(&d.join("missing")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ───────── сетевые (cargo test -- --ignored create) ─────────

    #[test]
    #[ignore = "network"]
    fn create_net_list_versions() {
        for core in Core::ALL {
            let v = list_versions(core).unwrap();
            assert!(v.len() > 5, "{core:?}: {v:?}");
            assert!(v.iter().all(|x| is_release(x)), "{core:?}: {v:?}");
            println!("{}: {:?}", core.label(), &v[..5]);
        }
    }

    fn run_create(core: Core, version: &str) {
        let d = tmp(&format!("e2e-{}", core.label().to_lowercase()));
        let opts = CreateOptions {
            core,
            version: version.into(),
            dir: d.clone(),
            memory_mb: 2048,
            port: 25599,
            accept_eula: true,
        };
        let mut last_pct = std::collections::HashMap::new();
        let mut log = |p: Progress| match p {
            Progress::Step(s) => println!("· {s}"),
            Progress::Download { what, done, total } => {
                let pct = total.map(|t| done * 100 / t.max(1));
                if last_pct.insert(what.clone(), pct) != Some(pct) && pct.is_none_or(|p| p % 25 == 0) {
                    println!("  {what}: {done} / {total:?}");
                }
            }
        };
        let res = create(&opts, &mut log);
        let check = || -> Result<()> {
            let c = res?;
            println!("{c:?}");
            let jar = d.join(&c.jar);
            assert!(std::fs::metadata(&jar)?.len() > 100_000);
            assert!(std::fs::read_to_string(d.join("eula.txt"))?.contains("eula=true"));
            assert!(std::fs::read_to_string(d.join("server.properties"))?.contains("server-port=25599"));
            let script = std::fs::read_to_string(d.join(if cfg!(windows) { "start.bat" } else { "start.sh" }))?;
            println!("{script}");
            assert!(script.contains(&c.jar));
            if c.java != Path::new("java") {
                assert!(c.java.is_absolute() && c.java.starts_with(&d));
                assert!(java_major(&c.java).is_some());
            }
            assert!(d.read_dir()?.flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".part")));
            Ok(())
        };
        let r = check();
        let _ = std::fs::remove_dir_all(&d);
        r.unwrap();
    }

    #[test]
    #[ignore = "network, ~120 MB"]
    fn create_net_paper() {
        run_create(Core::Paper, &list_versions(Core::Paper).unwrap()[0]);
    }

    #[test]
    #[ignore = "network, ~60 MB"]
    fn create_net_vanilla() {
        run_create(Core::Vanilla, "1.21.11");
    }

    #[test]
    #[ignore = "network, ~60 MB"]
    fn create_net_purpur() {
        run_create(Core::Purpur, "1.21.11");
    }

    #[test]
    #[ignore = "network"]
    fn create_net_bad_version_cleans_up() {
        let d = tmp("e2e-bad");
        let opts = CreateOptions {
            core: Core::Fabric,
            version: "9.9".into(),
            dir: d.clone(),
            memory_mb: 1024,
            port: 25565,
            accept_eula: true,
        };
        let err = create(&opts, &mut |_| {}).unwrap_err();
        println!("{err:#}");
        assert!(!d.exists());
    }

    #[test]
    #[ignore = "network"]
    fn create_net_fabric() {
        run_create(Core::Fabric, "1.21.11");
    }
}
