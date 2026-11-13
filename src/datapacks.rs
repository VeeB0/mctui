//! Датапаки основного мира: `<level>/datapacks/` (включены) и `<level>/datapacks.disabled/` (выключены).
//! Модуль без UI: скан, метаданные, признаки содержимого и перенос между папками.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const ENABLED_DIR: &str = "datapacks";
pub const DISABLED_DIR: &str = "datapacks.disabled";
/// Загрузчики Modrinth, под которыми лежат датапаки.
pub const DATAPACK_LOADERS: &[&str] = &["datapack"];

const MCMETA_LIMIT: u64 = 1 << 20;
const WORLDGEN: &[&str] = &["worldgen", "dimension", "dimension_type"];
/// Папки, которые `/reload` подхватывает без перезапуска сервера. `tags/` решает реестр тегов, см. [`TAG_RELOADABLE`].
const RELOADABLE: &[&str] = &[
    "function",
    "functions",
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
/// Реестры в `data/<ns>/tags/<реестр>/`, теги которых `/reload` применяет на ходу. Остальные теги
/// (`worldgen/…`, `enchantment`, `damage_type`, …) читаются только при запуске мира.
const TAG_RELOADABLE: &[&str] = &[
    "block",
    "blocks",
    "item",
    "items",
    "entity_type",
    "entity_types",
    "fluid",
    "fluids",
    "game_event",
    "game_events",
    "function",
    "functions",
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

/// Проверка обновлений zip-датапаков на Modrinth (загрузчик `datapack`).
pub fn check_updates(items: &[DatapackInfo], game_version: Option<&str>) -> Result<crate::plugins::HashCheck> {
    check_updates_with(&crate::plugins::post_json, items, game_version)
}

fn check_updates_with(
    post: crate::plugins::PostFn,
    items: &[DatapackInfo],
    game_version: Option<&str>,
) -> Result<crate::plugins::HashCheck> {
    let loaders: Vec<String> = DATAPACK_LOADERS.iter().map(|s| s.to_string()).collect();
    let files: Vec<(PathBuf, String, Vec<String>)> = items
        .iter()
        .filter(|d| d.is_zip && !d.sha1.is_empty())
        .map(|d| (d.path.clone(), d.sha1.clone(), loaders.clone()))
        .collect();
    let mut h = crate::plugins::check_hashes_with(post, &files, game_version)?;
    // Новая версия с jar (или чем-то ещё) вместо zip — не датапак; под именем `X.zip` её не ставим.
    h.updates.retain(|_, u| u.file_name.to_ascii_lowercase().ends_with(".zip"));
    Ok(h)
}

/// Заменяет zip-датапак новой версией под тем же именем и в той же папке; старый уходит в
/// `datapacks.disabled/.pechka-old/`. Возвращает путь к новому файлу.
pub fn apply_update(d: &DatapackInfo, u: &crate::plugins::UpdateInfo, level_dir: &Path) -> Result<PathBuf> {
    apply_update_with(d, u, level_dir, crate::plugins::download)
}

fn apply_update_with(
    d: &DatapackInfo,
    u: &crate::plugins::UpdateInfo,
    level_dir: &Path,
    fetch: impl Fn(&str, &Path) -> Result<()>,
) -> Result<PathBuf> {
    if !d.is_zip {
        bail!(crate::tr!("обновлять можно только zip-датапаки", "only zip datapacks can be updated"));
    }
    let name = &d.file_name;
    let fail = || {
        crate::tr!(
            format!("не удалось перенести {name}: остановите сервер и повторите"),
            format!("failed to move {name}: stop the server and try again")
        )
    };
    let tmp_dir = level_dir.join(DISABLED_DIR);
    std::fs::create_dir_all(&tmp_dir)?;
    // Без расширения .zip — игра не подхватит недокачанный файл.
    let tmp = tmp_dir.join(format!(".pechka-download-{}.part", std::process::id()));
    let res = (|| -> Result<PathBuf> {
        fetch(&u.url, &tmp)?;
        let actual = crate::plugins::sha1_file(&tmp)?;
        if let Some(expected) = &u.sha1
            && !expected.eq_ignore_ascii_case(&actual)
        {
            bail!(crate::tr!(
                format!("контрольная сумма не совпала (ожидалось {expected}, получено {actual})"),
                format!("checksum mismatch (expected {expected}, got {actual})")
            ));
        }
        let zip = zip::ZipArchive::new(std::fs::File::open(&tmp)?)
            .context(crate::tr!("скачанный файл — не zip", "downloaded file is not a zip"))?;
        if !has_pack_mcmeta(&zip) {
            bail!(not_a_datapack());
        }

        let old_dir = tmp_dir.join(crate::plugins::OLD_DIR);
        std::fs::create_dir_all(&old_dir).map_err(|_| anyhow::anyhow!(fail()))?;
        let mut backup = old_dir.join(name);
        let mut n = 0;
        while backup.symlink_metadata().is_ok() {
            n += 1;
            backup = old_dir.join(format!("{name}.{n}"));
        }
        std::fs::rename(&d.path, &backup).map_err(|_| anyhow::anyhow!(fail()))?;
        if std::fs::rename(&tmp, &d.path).is_err() {
            let _ = std::fs::rename(&backup, &d.path);
            bail!(fail());
        }
        Ok(d.path.clone())
    })();
    let _ = std::fs::remove_file(&tmp);
    res
}

/// В корне архива есть `pack.mcmeta` — без него игра пак не загрузит.
pub(crate) fn has_pack_mcmeta<R: Read + std::io::Seek>(zip: &zip::ZipArchive<R>) -> bool {
    zip.index_for_name("pack.mcmeta").is_some()
}

pub(crate) fn not_a_datapack() -> String {
    crate::tr!("не датапак: нет pack.mcmeta", "not a datapack: no pack.mcmeta").into()
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
        // Как в игре: только `.zip` в нижнем регистре (`X.ZIP` игра пропускает — и мы не показываем).
        if !file_name.ends_with(".zip") {
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
        if *dir == "tags" {
            // `data/<ns>/tags/<реестр>/…`: решает реестр. Теги генерации (`tags/worldgen/…`) сами генерацию
            // не меняют — только требуют перезапуска. Файл прямо в `tags/` — не содержимое.
            if let Some(registry) = parts.get(i + 3)
                && parts.len() > i + 4
            {
                restart |= !TAG_RELOADABLE.contains(registry);
            }
            continue;
        }
        worldgen |= WORLDGEN.contains(dir);
        restart |= !RELOADABLE.contains(dir);
    }
    (worldgen, restart)
}

/// Описание одной строкой: без кодов форматирования `§x`, переводы строк — пробелом. Пустое → `None`.
fn description_text(v: &serde_json::Value) -> Option<String> {
    let raw = flat_text(v)?;
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Код форматирования: `§` и следующий за ним символ.
            '§' => {
                chars.next();
            }
            '\n' | '\r' => {
                while chars.next_if(|c| matches!(c, '\n' | '\r')).is_some() {}
                out.push(' ');
            }
            c => out.push(c),
        }
    }
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

/// Плоский текст описания: строка, `{text, extra[]}` или массив таких.
fn flat_text(v: &serde_json::Value) -> Option<String> {
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
    let texts: Vec<String> = parts.filter_map(flat_text).collect();
    if texts.is_empty() { None } else { Some(texts.concat()) }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            // Часы macOS идут с шагом в микросекунду: параллельные тесты с одним `tag` (zip_bytes) получали
            // одну папку и удаляли её друг у друга. Счётчик делает имя уникальным.
            static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let d = std::env::temp_dir().join(format!(
                "pechka-datapacks-{tag}-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

    pub(crate) fn make_zip(path: &Path, entries: &[(&str, &str)]) {
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
    pub(crate) fn level_dat(path: &Path, disabled: &[&str]) {
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
        // Игра берёт только `.zip` в нижнем регистре — `D.ZIP` она пропускает, и мы не показываем.
        make_zip(&level.join("datapacks/D.ZIP"), &[("pack.mcmeta", MCMETA)]);
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
        // Теги: `/reload` применяет только теги блоков, предметов, сущностей, жидкостей, игровых событий и функций.
        for reg in [
            "block",
            "blocks",
            "item",
            "items",
            "entity_type",
            "entity_types",
            "fluid",
            "fluids",
            "game_event",
            "game_events",
            "function",
            "functions",
        ] {
            assert_eq!(f(&[format!("data/ns/tags/{reg}/x.json").as_str()]), (false, false), "{reg}");
        }
        assert_eq!(f(&["data/ns/tags/worldgen/biome/x.json"]), (false, true));
        assert_eq!(f(&["ov_1/data/ns/tags/worldgen/structure/x.json"]), (false, true));
        assert_eq!(f(&["data/ns/tags/enchantment/x.json"]), (false, true));
        assert_eq!(f(&["data/ns/tags/damage_type/x.json"]), (false, true));
        assert_eq!(f(&["data/ns/tags/", "data/ns/tags/block/"]), (false, false), "записи-папки zip");
    }

    #[test]
    fn description_flattens_components() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(description_text(&j(r#"{"text":"A","extra":[{"text":"B"},"C"]}"#)).as_deref(), Some("ABC"));
        assert_eq!(description_text(&j(r#"[{"text":"X"},"Y"]"#)).as_deref(), Some("XY"));
        assert_eq!(description_text(&j("42")), None);
    }

    #[test]
    fn description_strips_formatting_codes_and_newlines() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(
            description_text(&j(r#""§aGreen§r line\nnext\r\n\r\nend  ""#)).as_deref(),
            Some("Green line next end")
        );
        assert_eq!(description_text(&j(r#"{"text":"§6Gold\n","extra":["§lX"]}"#)).as_deref(), Some("Gold X"));
        assert_eq!(description_text(&j(r#""  §""#)).as_deref(), None);
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

    fn zip_bytes(body: &str) -> Vec<u8> {
        let t = TempDir::new("zb");
        let p = t.0.join("x.zip");
        make_zip(&p, &[("pack.mcmeta", body)]);
        std::fs::read(&p).unwrap()
    }

    fn info_for(level: &Path, name: &str) -> DatapackInfo {
        scan(level).into_iter().find(|d| d.file_name == name).unwrap()
    }

    fn upd(sha1: Option<String>) -> crate::plugins::UpdateInfo {
        crate::plugins::UpdateInfo {
            version: "2".into(),
            file_name: "a-2.zip".into(),
            url: "https://x/a-2.zip".into(),
            sha1,
            page: String::new(),
        }
    }

    #[test]
    fn apply_update_keeps_name_and_backup() {
        let t = TempDir::new("upd");
        let level = t.0.join("world");
        make_zip(&level.join("datapacks/a.zip"), &[("pack.mcmeta", "{\"pack\":{\"description\":\"old\"}}")]);
        let old = std::fs::read(level.join("datapacks/a.zip")).unwrap();
        let new1 = zip_bytes("{\"pack\":{\"description\":\"new1\"}}");
        let new2 = zip_bytes("{\"pack\":{\"description\":\"new2\"}}");

        let d = info_for(&level, "a.zip");
        let b = new1.clone();
        let p = apply_update_with(&d, &upd(None), &level, |_, dest| Ok(std::fs::write(dest, &b)?)).unwrap();
        assert_eq!(p, level.join("datapacks/a.zip"));
        assert_eq!(std::fs::read(&p).unwrap(), new1);
        assert_eq!(std::fs::read(level.join("datapacks.disabled/.pechka-old/a.zip")).unwrap(), old);

        let d = info_for(&level, "a.zip");
        let b = new2.clone();
        apply_update_with(&d, &upd(None), &level, |_, dest| Ok(std::fs::write(dest, &b)?)).unwrap();
        assert_eq!(std::fs::read(level.join("datapacks/a.zip")).unwrap(), new2);
        assert_eq!(std::fs::read(level.join("datapacks.disabled/.pechka-old/a.zip")).unwrap(), old);
        assert_eq!(std::fs::read(level.join("datapacks.disabled/.pechka-old/a.zip.1")).unwrap(), new1);
    }

    #[test]
    fn apply_update_works_for_disabled_pack() {
        let t = TempDir::new("updoff");
        let level = t.0.join("world");
        make_zip(&level.join("datapacks.disabled/a.zip"), &[("pack.mcmeta", "{}")]);
        let new = zip_bytes("{\"pack\":{}}");
        let d = info_for(&level, "a.zip");
        let p = apply_update_with(&d, &upd(None), &level, |_, dest| Ok(std::fs::write(dest, &new)?)).unwrap();
        assert_eq!(p, level.join("datapacks.disabled/a.zip"));
        assert!(!level.join("datapacks/a.zip").exists());
    }

    #[test]
    fn apply_update_rejects_bad_checksum() {
        let t = TempDir::new("badsum");
        let level = t.0.join("world");
        make_zip(&level.join("datapacks/a.zip"), &[("pack.mcmeta", "{}")]);
        let old = std::fs::read(level.join("datapacks/a.zip")).unwrap();
        let new = zip_bytes("{\"pack\":{}}");
        let d = info_for(&level, "a.zip");
        let r = apply_update_with(&d, &upd(Some("0".repeat(40))), &level, |_, dest| Ok(std::fs::write(dest, &new)?));
        assert!(r.is_err());
        assert_eq!(std::fs::read(level.join("datapacks/a.zip")).unwrap(), old);
        let left: Vec<_> = std::fs::read_dir(level.join("datapacks.disabled"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn apply_update_rejects_zip_without_pack_mcmeta() {
        let t = TempDir::new("nomcmeta");
        let level = t.0.join("world");
        make_zip(&level.join("datapacks/a.zip"), &[("pack.mcmeta", "{}")]);
        let old = std::fs::read(level.join("datapacks/a.zip")).unwrap();
        // zip, но не датапак: pack.mcmeta не в корне (например, плагин или обёртка-папка).
        let src = t.0.join("src.zip");
        make_zip(&src, &[("inner/pack.mcmeta", "{}"), ("plugin.yml", "name: x")]);
        let d = info_for(&level, "a.zip");
        let e = apply_update_with(&d, &upd(None), &level, |_, dest| Ok(std::fs::copy(&src, dest).map(|_| ())?))
            .unwrap_err();
        assert!(format!("{e:#}").contains("pack.mcmeta"), "{e:#}");
        assert_eq!(std::fs::read(level.join("datapacks/a.zip")).unwrap(), old);
        assert!(!level.join("datapacks.disabled/.pechka-old").exists());
    }

    fn mr_version(id: &str, filename: &str, sha1: &str, date: &str) -> String {
        format!(
            r#"{{"id":"{id}","project_id":"P","version_number":"{id}","date_published":"{date}",
                "files":[{{"url":"https://x/{filename}","filename":"{filename}","primary":true,"hashes":{{"sha1":"{sha1}"}}}}]}}"#
        )
    }

    #[test]
    fn check_updates_skips_non_zip_files() {
        let t = TempDir::new("chkzip");
        let level = t.0.join("world");
        make_zip(&level.join("datapacks/a.zip"), &[("pack.mcmeta", MCMETA)]);
        make_zip(&level.join("datapacks/b.zip"), &[("pack.mcmeta", r#"{"pack":{"description":"b"}}"#)]);
        let items = scan(&level);
        let (ha, hb) = (items[0].sha1.clone(), items[1].sha1.clone());
        let post = |url: &str, _: &serde_json::Value| -> Result<String> {
            if url.ends_with("/version_files/update") {
                // a — новая версия в zip; b — «новая версия» с jar-файлом (это не датапак).
                Ok(format!(
                    r#"{{"{ha}":{},"{hb}":{}}}"#,
                    mr_version("a2", "a-2.zip", "n1", "2026-02-01T00:00:00Z"),
                    mr_version("b2", "b-2.jar", "n2", "2026-02-01T00:00:00Z")
                ))
            } else {
                Ok(format!(
                    r#"{{"{ha}":{},"{hb}":{}}}"#,
                    mr_version("a1", "a.zip", &ha, "2026-01-01T00:00:00Z"),
                    mr_version("b1", "b.zip", &hb, "2026-01-01T00:00:00Z")
                ))
            }
        };
        let r = check_updates_with(&post, &items, None).unwrap();
        assert_eq!(r.updates.get(&items[0].path).map(|u| u.version.as_str()), Some("a2"));
        assert!(!r.updates.contains_key(&items[1].path), "{:?}", r.updates.get(&items[1].path));
        assert_eq!(r.current.get(&items[1].path).map(String::as_str), Some("b1"), "версию b всё равно знаем");
    }

    #[test]
    fn apply_update_rejects_non_zip_download_and_folders() {
        let t = TempDir::new("nonzip");
        let level = t.0.join("world");
        make_zip(&level.join("datapacks/a.zip"), &[("pack.mcmeta", "{}")]);
        write(&level.join("datapacks/B/pack.mcmeta"), "{}");
        let d = info_for(&level, "a.zip");
        assert!(apply_update_with(&d, &upd(None), &level, |_, dest| Ok(std::fs::write(dest, b"nope")?)).is_err());
        assert!(level.join("datapacks/a.zip").is_file());
        assert!(!level.join("datapacks.disabled/.pechka-old").exists());
        let b = info_for(&level, "B");
        assert!(apply_update_with(&b, &upd(None), &level, |_, _| Ok(())).is_err());
    }
}
