//! Датапаки основного мира: `<level>/datapacks/` (включены) и `<level>/datapacks.disabled/` (выключены).
//! Модуль без UI: скан, метаданные, признаки содержимого и перенос между папками.

use anyhow::{Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const ENABLED_DIR: &str = "datapacks";
pub const DISABLED_DIR: &str = "datapacks.disabled";
/// Загрузчики Modrinth, под которыми лежат датапаки.
pub const DATAPACK_LOADERS: &[&str] = &["datapack"];

const MCMETA_LIMIT: u64 = 1 << 20;
const WORLDGEN: &[&str] = &["worldgen", "dimension", "dimension_type"];
/// Папки, которые `/reload` подхватывает без перезапуска сервера.
const RELOADABLE: &[&str] = &[
    "function",
    "functions",
    "tags",
    "recipe",
    "recipes",
    "loot_table",
    "loot_tables",
    "advancement",
    "advancements",
    "predicate",
    "predicates",
    "item_modifier",
    "item_modifiers",
    "structure",
    "structures",
];

#[derive(Debug, Clone, PartialEq)]
pub struct DatapackInfo {
    /// Где лежит сейчас.
    pub path: PathBuf,
    /// "Terralith.zip" или имя папки.
    pub file_name: String,
    /// `file_name` без ".zip".
    pub name: String,
    pub is_zip: bool,
    /// Лежит в `datapacks/`.
    pub enabled: bool,
    /// `enabled` и "file/<file_name>" есть в `level.dat` → `Data.DataPacks.Disabled`.
    pub disabled_in_game: bool,
    pub description: Option<String>,
    pub worldgen: bool,
    /// Worldgen ⇒ нужен перезапуск, а не `reload`.
    pub needs_restart: bool,
    /// Только для zip; "" для папок и нечитаемых.
    pub sha1: String,
}

impl DatapackInfo {
    /// Идентификатор пака в игре: `file/<file_name>`.
    pub fn id(&self) -> String {
        format!("file/{}", self.file_name)
    }
}

/// Папка основного мира: `<server_dir>/<level-name>`.
pub fn level_dir(server_dir: &Path) -> PathBuf {
    server_dir.join(crate::world::current_level_name(server_dir))
}

/// Датапаки из обеих папок, по имени без учёта регистра.
pub fn scan(level_dir: &Path) -> Vec<DatapackInfo> {
    let off = game_disabled(level_dir);
    let mut out = vec![];
    for (sub, enabled) in [(ENABLED_DIR, true), (DISABLED_DIR, false)] {
        let Ok(rd) = std::fs::read_dir(level_dir.join(sub)) else { continue };
        for e in rd.flatten() {
            if let Some(d) = inspect(&e.path(), enabled, &off) {
                out.push(d);
            }
        }
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

/// Переносит пак между `datapacks/` и `datapacks.disabled/`; возвращает новый путь.
pub fn set_enabled(d: &DatapackInfo, enabled: bool, level_dir: &Path) -> Result<PathBuf> {
    if d.enabled == enabled {
        return Ok(d.path.clone());
    }
    let folder = if enabled { ENABLED_DIR } else { DISABLED_DIR };
    let dir = level_dir.join(folder);
    let to = dir.join(&d.file_name);
    if to.symlink_metadata().is_ok() {
        let name = &d.file_name;
        bail!(crate::tr!(
            format!("файл {name} уже есть в {folder}"),
            format!("file {name} already exists in {folder}")
        ));
    }
    let name = &d.file_name;
    let fail = || {
        crate::tr!(
            format!("не удалось перенести {name}: остановите сервер и повторите"),
            format!("failed to move {name}: stop the server and try again")
        )
    };
    std::fs::create_dir_all(&dir).map_err(|_| anyhow::anyhow!(fail()))?;
    std::fs::rename(&d.path, &to).map_err(|_| anyhow::anyhow!(fail()))?;
    Ok(to)
}

/// `Data.DataPacks.Disabled` из `level.dat` (пусто, если файла или списка нет).
fn game_disabled(level_dir: &Path) -> Vec<String> {
    use crate::world::nbt::{Tag, read_file};
    let Some(root) = read_file(&level_dir.join("level.dat")) else { return vec![] };
    let list = root.get("Data").and_then(|t| t.get("DataPacks")).and_then(|t| t.get("Disabled"));
    match list {
        Some(Tag::List(v)) => v.iter().filter_map(|t| t.as_str().map(str::to_string)).collect(),
        _ => vec![],
    }
}

fn inspect(path: &Path, enabled: bool, game_off: &[String]) -> Option<DatapackInfo> {
    let file_name = path.file_name()?.to_str()?.to_string();
    if file_name.starts_with('.') {
        return None;
    }
    let is_zip = if path.is_file() {
        if !file_name.to_lowercase().ends_with(".zip") {
            return None;
        }
        true
    } else if path.join("pack.mcmeta").is_file() {
        false
    } else {
        return None;
    };
    let (meta, (worldgen, needs_restart)) = if is_zip { read_zip(path) } else { read_folder(path) };
    let description = meta
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("pack").and_then(|p| p.get("description")).and_then(description_text));
    let name = if is_zip { file_name[..file_name.len() - 4].to_string() } else { file_name.clone() };
    let mut d = DatapackInfo {
        path: path.to_path_buf(),
        file_name,
        name,
        is_zip,
        enabled,
        disabled_in_game: false,
        description,
        worldgen,
        needs_restart,
        sha1: if is_zip { crate::plugins::sha1_file(path).unwrap_or_default() } else { String::new() },
    };
    d.disabled_in_game = enabled && game_off.contains(&d.id());
    Some(d)
}

/// `pack.mcmeta` (до 1 MiB) и признаки содержимого zip. Нечитаемый zip → `(None, (false, true))`.
fn read_zip(path: &Path) -> (Option<Vec<u8>>, (bool, bool)) {
    let Ok(file) = std::fs::File::open(path) else { return (None, (false, true)) };
    let Ok(mut zip) = zip::ZipArchive::new(file) else { return (None, (false, true)) };
    let names: Vec<String> = zip.file_names().filter_map(Result::ok).map(|n| n.replace('\\', "/")).collect();
    let flags = content_flags(names.iter().map(String::as_str));
    let meta = zip.by_name("pack.mcmeta").ok().and_then(read_limited);
    (meta, flags)
}

fn read_folder(path: &Path) -> (Option<Vec<u8>>, (bool, bool)) {
    let meta = std::fs::File::open(path.join("pack.mcmeta")).ok().and_then(read_limited);
    let mut rel = vec![];
    walk(path, "", &mut rel);
    (meta, content_flags(rel.iter().map(String::as_str)))
}

fn read_limited(r: impl Read) -> Option<Vec<u8>> {
    let mut buf = vec![];
    r.take(MCMETA_LIMIT).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Относительные пути файлов с `/`; симлинки пропускаются.
fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
        let rel = format!("{prefix}{name}");
        if ft.is_dir() {
            walk(&e.path(), &format!("{rel}/"), out);
        } else if ft.is_file() {
            out.push(rel);
        }
    }
}

/// `(worldgen, needs_restart)` по путям: `data/<ns>/<dir>/…` или `<overlay>/data/<ns>/<dir>/…`.
fn content_flags<'a>(paths: impl Iterator<Item = &'a str>) -> (bool, bool) {
    let (mut worldgen, mut restart) = (false, false);
    for p in paths {
        let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
        let Some(i) = (0..2).find(|&i| parts.get(i) == Some(&"data")) else { continue };
        let Some(dir) = parts.get(i + 2) else { continue };
        worldgen |= WORLDGEN.contains(dir);
        restart |= !RELOADABLE.contains(dir);
    }
    (worldgen, restart)
}

/// Плоский текст описания: строка, `{text, extra[]}` или массив таких.
fn description_text(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => join_parts(a.iter()),
        Value::Object(o) => join_parts(
            o.get("text")
                .into_iter()
                .chain(o.get("extra").into_iter().flat_map(|e| e.as_array().into_iter().flatten())),
        ),
        _ => None,
    }
}

fn join_parts<'a>(parts: impl Iterator<Item = &'a serde_json::Value>) -> Option<String> {
    let texts: Vec<String> = parts.filter_map(description_text).collect();
    if texts.is_empty() { None } else { Some(texts.concat()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "pechka-datapacks-{tag}-{}-{}",
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

    fn make_zip(path: &Path, entries: &[(&str, &str)]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, body) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn s(b: &mut Vec<u8>, v: &str) {
        b.extend((v.len() as u16).to_be_bytes());
        b.extend(v.as_bytes());
    }

    /// `level.dat` с `Data.DataPacks.Disabled = disabled`.
    fn level_dat(path: &Path, disabled: &[&str]) {
        let mut b = vec![10u8];
        s(&mut b, "");
        b.push(10);
        s(&mut b, "Data");
        b.push(10);
        s(&mut b, "DataPacks");
        b.push(9);
        s(&mut b, "Disabled");
        b.push(8);
        b.extend((disabled.len() as i32).to_be_bytes());
        for d in disabled {
            s(&mut b, d);
        }
        b.extend([0u8; 3]); // DataPacks, Data, корень
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(path).unwrap(), flate2::Compression::fast());
        gz.write_all(&b).unwrap();
        gz.finish().unwrap();
    }

    const MCMETA: &str = r#"{"pack":{"description":"Hi"}}"#;

    fn sample(level: &Path) {
        make_zip(&level.join("datapacks/a.zip"), &[("pack.mcmeta", MCMETA)]);
        write(&level.join("datapacks/B/pack.mcmeta"), MCMETA);
        write(&level.join("datapacks/nopack/x.txt"), "x");
        write(&level.join("datapacks/readme.txt"), "x");
        make_zip(&level.join("datapacks/.hidden.zip"), &[("pack.mcmeta", MCMETA)]);
        make_zip(&level.join("datapacks.disabled/c.zip"), &[("pack.mcmeta", MCMETA)]);
        make_zip(&level.join("datapacks.disabled/.pechka-old/old.zip"), &[("pack.mcmeta", MCMETA)]);
    }

    #[test]
    fn scan_lists_zips_and_folders() {
        let t = TempDir::new("scan");
        let level = t.0.join("world");
        sample(&level);
        let v = scan(&level);
        assert_eq!(v.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["a", "B", "c"]);
        assert_eq!(v.iter().map(|d| d.enabled).collect::<Vec<_>>(), [true, true, false]);
        assert_eq!(v[0].description.as_deref(), Some("Hi"));
        assert!(v[0].is_zip && !v[0].sha1.is_empty());
        assert!(!v[1].is_zip && v[1].sha1.is_empty());
        assert_eq!(v[0].id(), "file/a.zip");
    }

    #[test]
    fn scan_missing_world_is_empty() {
        let t = TempDir::new("missing");
        assert!(scan(&t.0.join("world")).is_empty());
    }

    #[test]
    fn content_flags_by_folders() {
        let f = |p: &[&str]| content_flags(p.iter().copied());
        assert_eq!(f(&["data/ns/function/a.mcfunction"]), (false, false));
        assert_eq!(f(&["data/ns/worldgen/biome/x.json"]), (true, true));
        assert_eq!(f(&["data/ns/enchantment/x.json"]), (false, true));
        assert_eq!(f(&["ov_1/data/ns/dimension_type/x.json"]), (true, true));
        assert_eq!(f(&["assets/x.png", "pack.mcmeta"]), (false, false));
    }

    #[test]
    fn description_flattens_components() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(description_text(&j(r#"{"text":"A","extra":[{"text":"B"},"C"]}"#)).as_deref(), Some("ABC"));
        assert_eq!(description_text(&j(r#"[{"text":"X"},"Y"]"#)).as_deref(), Some("XY"));
        assert_eq!(description_text(&j("42")), None);
    }

    #[test]
    fn scan_survives_broken_packs() {
        let t = TempDir::new("broken");
        let level = t.0.join("world");
        write(&level.join("datapacks/bad.zip"), "garbage");
        make_zip(&level.join("datapacks/json.zip"), &[("pack.mcmeta", "{not json")]);
        let v = scan(&level);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "bad");
        assert!(v[0].description.is_none() && v[0].needs_restart && !v[0].worldgen);
        assert_eq!(v[1].name, "json");
        assert!(v[1].description.is_none());
    }

    #[test]
    fn disabled_in_game_from_level_dat() {
        let t = TempDir::new("leveldat");
        let level = t.0.join("world");
        sample(&level);
        level_dat(&level.join("level.dat"), &["file/a.zip", "trade_rebalance"]);
        let v = scan(&level);
        assert!(v[0].disabled_in_game);
        assert!(!v[1].disabled_in_game);
        assert!(!v[2].disabled_in_game); // выключен переносом, а не в игре
    }

    #[test]
    fn set_enabled_moves_both_ways() {
        let t = TempDir::new("move");
        let level = t.0.join("world");
        sample(&level);
        let v = scan(&level);
        let p = set_enabled(&v[0], false, &level).unwrap();
        assert_eq!(p, level.join("datapacks.disabled/a.zip"));
        assert!(p.is_file() && !level.join("datapacks/a.zip").exists());
        let v = scan(&level);
        let a = v.iter().find(|d| d.name == "a").unwrap();
        assert!(!a.enabled);
        let p = set_enabled(a, true, &level).unwrap();
        assert_eq!(p, level.join("datapacks/a.zip"));
        assert!(p.is_file() && !level.join("datapacks.disabled/a.zip").exists());
        let b = scan(&level).into_iter().find(|d| d.name == "B").unwrap();
        set_enabled(&b, false, &level).unwrap();
        assert!(level.join("datapacks.disabled/B/pack.mcmeta").is_file());
        assert!(!level.join("datapacks/B").exists());
    }

    #[test]
    fn set_enabled_refuses_to_overwrite() {
        let t = TempDir::new("refuse");
        let level = t.0.join("world");
        write(&level.join("datapacks/a.zip"), "one");
        write(&level.join("datapacks.disabled/a.zip"), "two");
        let d = scan(&level).into_iter().find(|d| d.enabled).unwrap();
        assert!(set_enabled(&d, false, &level).is_err());
        assert_eq!(std::fs::read_to_string(level.join("datapacks/a.zip")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(level.join("datapacks.disabled/a.zip")).unwrap(), "two");
    }
}
