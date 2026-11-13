//! Миры сервера: измерения, размер на диске, сброс измерений, смена `level-name`,
//! прегенерация через Chunky.
//!
//! Три раскладки папок мира:
//! - **Modern** (Minecraft 26.x, и vanilla, и Paper/Purpur): `<level>/dimensions/minecraft/{overworld,
//!   the_nether,the_end}/region`, игроки — `<level>/players/{data,stats,advancements}`;
//! - **Vanilla** (до 26.x, vanilla / Fabric): верхний мир прямо в `<level>/region`, Незер — `<level>/DIM-1`,
//!   Энд — `<level>/DIM1`, измерения модов — `<level>/dimensions/<ns>/<id>`;
//! - **Bukkit** (до 26.x, Paper / Purpur / Spigot): Незер и Энд в отдельных папках
//!   `<level>_nether/DIM-1` и `<level>_the_end/DIM1` (у каждой свой `level.dat`).
//!
//! Все функции блокирующие (обход папок мира может занять секунды) — вызывать вне UI-потока.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Папка для измерений, отложенных при сбросе (внутри папки сервера).
pub const RESET_STASH_DIR: &str = ".pechka-before-reset";
/// Сколько отложенных сбросов хранить.
const KEEP_BEFORE_RESET: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Dimension {
    Overworld,
    Nether,
    End,
    /// Измерение мода / датапака: `namespace:id`.
    Custom(String),
}

impl Dimension {
    /// Идентификатор измерения в игре (`minecraft:the_nether`).
    pub fn id(&self) -> String {
        match self {
            Dimension::Overworld => "minecraft:overworld".into(),
            Dimension::Nether => "minecraft:the_nether".into(),
            Dimension::End => "minecraft:the_end".into(),
            Dimension::Custom(id) => id.clone(),
        }
    }

    /// Название для интерфейса.
    pub fn title(&self) -> String {
        match self {
            Dimension::Overworld => crate::tr!("Верхний мир", "Overworld").into(),
            Dimension::Nether => crate::tr!("Незер", "Nether").into(),
            Dimension::End => crate::tr!("Энд", "The End").into(),
            Dimension::Custom(id) => id.clone(),
        }
    }

    /// Можно ли сбросить измерение: верхний мир, Незер и Энд (измерения модов — нет).
    pub fn resettable(&self) -> bool {
        !matches!(self, Dimension::Custom(_))
    }

    /// `the_nether` / `the_end` — имя папки в раскладке Modern.
    fn modern_dir(&self) -> Option<&str> {
        match self {
            Dimension::Overworld => Some("overworld"),
            Dimension::Nether => Some("the_nether"),
            Dimension::End => Some("the_end"),
            Dimension::Custom(_) => None,
        }
    }

    /// `DIM-1` / `DIM1` — имя папки в раскладках Vanilla и Bukkit.
    fn legacy_dir(&self) -> Option<&str> {
        match self {
            Dimension::Nether => Some("DIM-1"),
            Dimension::End => Some("DIM1"),
            _ => None,
        }
    }

    /// Суффикс отдельной папки Bukkit: `<level>_nether`.
    fn bukkit_suffix(&self) -> Option<&str> {
        match self {
            Dimension::Nether => Some("_nether"),
            Dimension::End => Some("_the_end"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Minecraft 26.x: `<level>/dimensions/minecraft/<dim>`.
    Modern,
    /// Старый vanilla / Fabric: `DIM-1`, `DIM1` внутри `<level>`.
    Vanilla,
    /// Старый Paper / Spigot: `<level>_nether`, `<level>_the_end`.
    Bukkit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DimensionInfo {
    pub dim: Dimension,
    /// Папка измерения (для Bukkit — `<level>_nether` целиком; для старого верхнего мира — `<level>`).
    pub path: PathBuf,
    /// Измерение уже создано (есть папка).
    pub exists: bool,
    /// Размер на диске, байт (для старого верхнего мира — без вложенных измерений).
    pub size_bytes: u64,
    /// Число файлов регионов `region/*.mca` (1 регион = 32×32 чанка).
    pub region_files: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorldInfo {
    pub level_name: String,
    /// `<dir>/<level>`.
    pub path: PathBuf,
    /// Мир уже создан (есть `level.dat`).
    pub exists: bool,
    pub layout: Option<Layout>,
    /// Верхний мир, Незер, Энд (всегда, даже если ещё не созданы), затем измерения модов.
    pub dimensions: Vec<DimensionInfo>,
    /// Весь мир на диске: `<level>` и отдельные папки Bukkit.
    pub total_bytes: u64,
    /// Другие миры в папке сервера (папки с `level.dat`) — для смены `level-name`.
    pub other_worlds: Vec<String>,
    pub seed: Option<i64>,
    /// Версия Minecraft, в которой мир последний раз сохранялся (`26.3`).
    pub version: Option<String>,
    /// Время последнего сохранения мира (`LastPlayed`).
    pub last_played: Option<SystemTime>,
}

impl WorldInfo {
    #[cfg(test)]
    pub fn dimension(&self, dim: &Dimension) -> Option<&DimensionInfo> {
        self.dimensions.iter().find(|d| &d.dim == dim)
    }
}

/// `level-name` из server.properties (по умолчанию `world`).
pub fn current_level_name(dir: &Path) -> String {
    crate::properties::Properties::load(&dir.join("server.properties"))
        .ok()
        .and_then(|p| p.get("level-name").map(str::to_string))
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "world".into())
}

/// Раскладка папок мира (`None` — мира ещё нет).
pub fn detect_layout(dir: &Path, level_name: &str) -> Option<Layout> {
    let level = dir.join(level_name);
    if level.join("dimensions/minecraft/overworld").is_dir() {
        Some(Layout::Modern)
    } else if [Dimension::Nether, Dimension::End]
        .iter()
        .any(|d| bukkit_dir(dir, level_name, d).is_some_and(|p| p.is_dir()))
    {
        Some(Layout::Bukkit)
    } else if level.is_dir() {
        Some(Layout::Vanilla)
    } else {
        None
    }
}

fn bukkit_dir(dir: &Path, level_name: &str, dim: &Dimension) -> Option<PathBuf> {
    Some(dir.join(format!("{level_name}{}", dim.bukkit_suffix()?)))
}

/// Папка измерения для показа и папка его регионов.
fn dim_paths(dir: &Path, level_name: &str, layout: Layout, dim: &Dimension) -> (PathBuf, PathBuf) {
    let level = dir.join(level_name);
    let modern = |d: &str| level.join("dimensions/minecraft").join(d);
    let path = match (layout, dim) {
        (_, Dimension::Custom(id)) => {
            let (ns, name) = id.split_once(':').unwrap_or(("minecraft", id));
            level.join("dimensions").join(ns).join(name)
        }
        (Layout::Modern, d) => modern(d.modern_dir().unwrap_or_default()),
        (_, Dimension::Overworld) => level.clone(),
        (Layout::Vanilla, d) => level.join(d.legacy_dir().unwrap_or_default()),
        (Layout::Bukkit, d) => bukkit_dir(dir, level_name, d).unwrap_or_default(),
    };
    let region = match (layout, dim) {
        (Layout::Bukkit, Dimension::Nether | Dimension::End) => path.join(dim.legacy_dir().unwrap_or_default()),
        _ => path.clone(),
    }
    .join("region");
    (path, region)
}

/// Миры сервера: измерения с размерами, другие миры, сид и версия.
pub fn scan(dir: &Path, level_name: &str) -> WorldInfo {
    let level = dir.join(level_name);
    let layout = detect_layout(dir, level_name);
    let mut info = WorldInfo {
        level_name: level_name.to_string(),
        path: level.clone(),
        exists: level.join("level.dat").is_file(),
        layout,
        other_worlds: list_worlds(dir).into_iter().filter(|w| w != level_name).collect(),
        ..Default::default()
    };
    let lay = layout.unwrap_or(Layout::Vanilla);
    let mut dims = vec![Dimension::Overworld, Dimension::Nether, Dimension::End];
    dims.extend(custom_dimensions(&level));
    for dim in dims {
        let (path, region) = dim_paths(dir, level_name, lay, &dim);
        let exists = path.is_dir();
        let size_bytes = if !exists {
            0
        } else if dim == Dimension::Overworld && lay != Layout::Modern {
            // Старый верхний мир — это сама папка <level>: вложенные измерения считаем отдельно.
            let nested: Vec<PathBuf> = ["DIM-1", "DIM1", "dimensions"].iter().map(|d| level.join(d)).collect();
            dir_size(&path, &nested)
        } else {
            dir_size(&path, &[])
        };
        info.dimensions.push(DimensionInfo { dim, path, exists, size_bytes, region_files: count_regions(&region) });
    }
    info.total_bytes = dir_size(&level, &[])
        + [Dimension::Nether, Dimension::End]
            .iter()
            .filter_map(|d| bukkit_dir(dir, level_name, d))
            .map(|p| dir_size(&p, &[]))
            .sum::<u64>();
    if let Some(ld) = read_level_dat(&level.join("level.dat")) {
        info.version = ld.version;
        info.seed = ld.seed;
        info.last_played = ld.last_played;
    }
    if info.seed.is_none() {
        // 26.x хранит настройки генерации отдельно от level.dat.
        info.seed = ["data/minecraft/world_gen_settings.dat", "data/world_gen_settings.dat"]
            .iter()
            .find_map(|f| nbt::read_file(&level.join(f)).and_then(|t| t.find_long("seed")));
    }
    info
}

/// Измерения модов: `<level>/dimensions/<ns>/<id>` кроме стандартных.
fn custom_dimensions(level: &Path) -> Vec<Dimension> {
    let mut out = vec![];
    let Ok(namespaces) = std::fs::read_dir(level.join("dimensions")) else { return out };
    for ns in namespaces.flatten().filter(|e| e.path().is_dir()) {
        let ns_name = ns.file_name().to_string_lossy().into_owned();
        let Ok(ids) = std::fs::read_dir(ns.path()) else { continue };
        for id in ids.flatten().filter(|e| e.path().is_dir()) {
            let id_name = id.file_name().to_string_lossy().into_owned();
            if ns_name == "minecraft" && ["overworld", "the_nether", "the_end"].contains(&id_name.as_str()) {
                continue;
            }
            out.push(Dimension::Custom(format!("{ns_name}:{id_name}")));
        }
    }
    out.sort_by_key(Dimension::id);
    out
}

/// Папки миров в папке сервера (есть `level.dat`), без отдельных папок Незера / Энда Bukkit.
pub fn list_worlds(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().join("level.dat").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    let mut out: Vec<String> = names
        .iter()
        .filter(|n| {
            !["_nether", "_the_end"]
                .iter()
                .any(|s| n.strip_suffix(s).is_some_and(|base| names.iter().any(|m| m == base)))
        })
        .cloned()
        .collect();
    out.sort_by_key(|n| n.to_lowercase());
    out
}

/// Размер папки (рекурсивно, символические ссылки не учитываются), кроме папок `exclude`.
pub fn dir_size(path: &Path, exclude: &[PathBuf]) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
            if meta.is_dir() {
                let child = e.path();
                if !exclude.contains(&child) {
                    stack.push(child);
                }
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

fn count_regions(region: &Path) -> usize {
    std::fs::read_dir(region)
        .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".mca")).count())
        .unwrap_or(0)
}

/// Папки, в которых лежат данные измерения при любой раскладке (то, что переносится при сбросе).
fn reset_candidates(dir: &Path, level_name: &str, dim: &Dimension) -> Vec<PathBuf> {
    let level = dir.join(level_name);
    let terrain = |base: PathBuf| TERRAIN.map(|d| base.join(d));
    let mut out = vec![];
    // 26.x: рядом с ландшафтом в папке измерения лежат data/ (сид генерации, правила игры, граница мира,
    // погода) и paper-world.yml — они остаются, переносится только ландшафт.
    if let Some(m) = dim.modern_dir() {
        out.extend(terrain(level.join("dimensions/minecraft").join(m)));
    }
    if *dim == Dimension::Overworld {
        // Старый верхний мир лежит прямо в <level>: level.dat (сид, правила), игроки, data/, датапаки и
        // вложенные Незер / Энд остаются.
        out.extend(terrain(level.clone()));
    }
    if let (Some(b), Some(l)) = (bukkit_dir(dir, level_name, dim), dim.legacy_dir()) {
        out.push(b.join(l));
    }
    if let Some(l) = dim.legacy_dir() {
        out.push(level.join(l));
    }
    out
}

/// Папки ландшафта измерения.
const TERRAIN: [&str; 3] = ["region", "entities", "poi"];

/// Сброс измерения (сервер должен быть остановлен — это обеспечивает вызывающий): ландшафт (`region`,
/// `entities`, `poi`; у старых Незера / Энда — вся папка `DIM-1` / `DIM1`) не удаляется, а переносится в
/// `<dir>/.pechka-before-reset/<время>/…` с сохранением относительного пути (хранятся последние 3 сброса).
/// Сид, правила игры, игроки и настройки измерения остаются. При следующем запуске сервер создаст ландшафт
/// заново с тем же сидом. Возвращает папку копии.
pub fn reset_dimension(dir: &Path, level_name: &str, dim: Dimension) -> Result<PathBuf> {
    if !dim.resettable() {
        bail!(crate::tr!(
            format!("измерение «{}» нельзя сбросить", dim.title()),
            format!("“{}” cannot be reset", dim.title())
        ));
    }
    validate_level_name(level_name)?;
    let found: Vec<PathBuf> = reset_candidates(dir, level_name, &dim).into_iter().filter(|p| has_data(p)).collect();
    if found.is_empty() {
        bail!(crate::tr!(
            format!("{} ещё не создан — сбрасывать нечего", dim.title()),
            format!("{} has not been generated yet — nothing to reset", dim.title())
        ));
    }
    let stash_root = dir.join(RESET_STASH_DIR);
    let base = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut stash = stash_root.join(&base);
    let mut n = 2;
    while stash.exists() {
        stash = stash_root.join(format!("{base}-{n}"));
        n += 1;
    }
    std::fs::create_dir_all(&stash).with_context(|| stash.display().to_string())?;
    let mut moved: Vec<(&PathBuf, PathBuf)> = vec![];
    for src in &found {
        let dst = stash.join(src.strip_prefix(dir).unwrap_or(src));
        let step = || -> Result<()> {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).with_context(|| parent.display().to_string())?;
            }
            std::fs::rename(src, &dst).with_context(|| format!("{} → {}", src.display(), dst.display()))
        };
        if let Err(e) = step() {
            // Полусброшенное измерение хуже несброшенного: возвращаем на место всё, что успели перенести.
            let stuck: Vec<String> = moved
                .iter()
                .rev()
                .filter(|(s, d)| std::fs::rename(d, s).is_err())
                .map(|(s, _)| s.display().to_string())
                .collect();
            if stuck.is_empty() {
                remove_empty_dirs(&stash); // там остались только пустые папки
                return Err(e);
            }
            let stuck = stuck.join(", ");
            return Err(e.context(crate::tr!(
                format!("сброс прерван; не удалось вернуть из {}: {stuck}", stash.display()),
                format!("reset interrupted; could not move back from {}: {stuck}", stash.display())
            )));
        }
        moved.push((src, dst));
    }
    for (src, dst) in &moved {
        restore_configs(dst, src)?;
    }
    rotate_stash(&stash_root, &stash);
    Ok(stash)
}

/// Удаляет `p` и вложенные папки, только если в них нет ни одного файла (данные не трогает никогда).
fn remove_empty_dirs(p: &Path) {
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_dirs(&e.path());
            }
        }
    }
    let _ = std::fs::remove_dir(p);
}

/// В папке есть что-то кроме файлов настроек (после сброса там остаётся только `paper-world.yml`).
fn has_data(p: &Path) -> bool {
    std::fs::read_dir(p).is_ok_and(|rd| rd.flatten().any(|e| !(e.path().is_file() && is_config(&e.file_name()))))
}

fn is_config(name: &std::ffi::OsStr) -> bool {
    let n = name.to_string_lossy().to_lowercase();
    n.ends_with(".yml") || n.ends_with(".yaml")
}

/// Возвращает `*.yml` / `*.yaml` из верхнего уровня перенесённой папки (настройки Paper для измерения).
fn restore_configs(moved: &Path, original: &Path) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(moved) else { return Ok(()) };
    for e in rd.flatten() {
        if e.path().is_file() && is_config(&e.file_name()) {
            std::fs::create_dir_all(original)?;
            std::fs::copy(e.path(), original.join(e.file_name()))?;
        }
    }
    Ok(())
}

/// Оставляет последние `KEEP_BEFORE_RESET` копий; только что сделанную (`fresh`) не трогает никогда,
/// даже если часы ушли назад и её имя оказалось «старше» прежних.
fn rotate_stash(stash_root: &Path, fresh: &Path) {
    if let Ok(rd) = std::fs::read_dir(stash_root) {
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && p != fresh).collect();
        dirs.sort();
        while dirs.len() >= KEEP_BEFORE_RESET {
            let _ = std::fs::remove_dir_all(dirs.remove(0));
        }
    }
}

/// Отложенные сбросы, новые первыми.
#[cfg(test)]
pub fn reset_stashes(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir.join(RESET_STASH_DIR)) else { return vec![] };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.reverse();
    dirs
}

/// Допустимое имя папки мира: не пустое, без разделителей пути и управляющих символов.
pub fn validate_level_name(name: &str) -> Result<()> {
    let bad = name.trim().is_empty()
        || name != name.trim()
        || name == "."
        || name == ".."
        || name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control());
    if bad {
        bail!(crate::tr!(format!("недопустимое имя мира: «{name}»"), format!("invalid world name: “{name}”")));
    }
    Ok(())
}

/// Записать `level-name` в server.properties (остальные строки и комментарии не меняются).
/// Новый мир (если такой папки нет) сервер создаст при следующем запуске.
#[cfg(test)]
pub fn set_level_name(dir: &Path, name: &str) -> Result<()> {
    validate_level_name(name)?;
    let path = dir.join("server.properties");
    let mut props = if path.exists() {
        crate::properties::Properties::load(&path).with_context(|| path.display().to_string())?
    } else {
        crate::properties::Properties::default()
    };
    props.set("level-name", name);
    props.save(&path).with_context(|| path.display().to_string())
}

// ---------- level.dat ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LevelDat {
    pub level_name: Option<String>,
    pub version: Option<String>,
    pub data_version: Option<i32>,
    pub seed: Option<i64>,
    pub last_played: Option<SystemTime>,
}

/// Основные поля `level.dat` (gzip NBT). `None` — файла нет или он не читается.
pub fn read_level_dat(path: &Path) -> Option<LevelDat> {
    let root = nbt::read_file(path)?;
    let data = root.get("Data")?;
    let s = |t: Option<&nbt::Tag>| t.and_then(nbt::Tag::as_str).map(str::to_string);
    Some(LevelDat {
        level_name: s(data.get("LevelName")),
        version: s(data.get("Version").and_then(|v| v.get("Name"))),
        data_version: data.get("DataVersion").and_then(nbt::Tag::as_i64).map(|v| v as i32),
        seed: data
            .get("WorldGenSettings")
            .and_then(|w| w.get("seed"))
            .or_else(|| data.get("RandomSeed"))
            .and_then(nbt::Tag::as_i64),
        last_played: data
            .get("LastPlayed")
            .and_then(nbt::Tag::as_i64)
            .filter(|&ms| ms > 0)
            .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64)),
    })
}

/// Минимальный читатель NBT (Java Edition, big-endian): только то, что нужно для `level.dat`.
/// Массивы (`ByteArray`/`IntArray`/`LongArray`) пропускаются — хранится только длина.
pub(crate) mod nbt {
    use std::io::Read;
    use std::path::Path;

    const MAX_DEPTH: usize = 128;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Tag {
        Byte(i8),
        Short(i16),
        Int(i32),
        Long(i64),
        Float(f32),
        Double(f64),
        Array(usize),
        String(String),
        List(Vec<Tag>),
        Compound(Vec<(String, Tag)>),
    }

    impl Tag {
        pub fn get(&self, key: &str) -> Option<&Tag> {
            match self {
                Tag::Compound(v) => v.iter().find(|(k, _)| k == key).map(|(_, t)| t),
                _ => None,
            }
        }

        pub fn as_str(&self) -> Option<&str> {
            match self {
                Tag::String(s) => Some(s),
                _ => None,
            }
        }

        pub fn as_i64(&self) -> Option<i64> {
            match *self {
                Tag::Byte(v) => Some(v.into()),
                Tag::Short(v) => Some(v.into()),
                Tag::Int(v) => Some(v.into()),
                Tag::Long(v) => Some(v),
                _ => None,
            }
        }

        /// Первая строка с таким именем где угодно в дереве.
        pub fn find_str(&self, key: &str) -> Option<&str> {
            match self {
                Tag::Compound(v) => v.iter().find_map(|(k, t)| {
                    if k == key
                        && let Some(s) = t.as_str()
                    {
                        Some(s)
                    } else {
                        t.find_str(key)
                    }
                }),
                Tag::List(v) => v.iter().find_map(|t| t.find_str(key)),
                _ => None,
            }
        }

        /// Первый `Long`/`Int` с таким именем где угодно в дереве.
        pub fn find_long(&self, key: &str) -> Option<i64> {
            match self {
                Tag::Compound(v) => v.iter().find_map(|(k, t)| {
                    if k == key
                        && let Some(n) = t.as_i64()
                    {
                        Some(n)
                    } else {
                        t.find_long(key)
                    }
                }),
                Tag::List(v) => v.iter().find_map(|t| t.find_long(key)),
                _ => None,
            }
        }
    }

    /// Файл NBT: gzip (как `level.dat`) или без сжатия.
    pub fn read_file(path: &Path) -> Option<Tag> {
        let raw = std::fs::read(path).ok()?;
        if raw.starts_with(&[0x1f, 0x8b]) {
            let mut out = vec![];
            flate2::read::GzDecoder::new(&raw[..]).take(64 << 20).read_to_end(&mut out).ok()?;
            parse(&out)
        } else {
            parse(&raw)
        }
    }

    /// Корневой именованный тег (обычно Compound).
    pub fn parse(data: &[u8]) -> Option<Tag> {
        let mut r = Reader { data, pos: 0 };
        let id = r.u8()?;
        if id == 0 {
            return None;
        }
        r.string()?;
        r.payload(id, 0)
    }

    struct Reader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl Reader<'_> {
        fn take(&mut self, n: usize) -> Option<&[u8]> {
            let end = self.pos.checked_add(n)?;
            let s = self.data.get(self.pos..end)?;
            self.pos = end;
            Some(s)
        }
        fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
            self.take(N)?.try_into().ok()
        }
        fn u8(&mut self) -> Option<u8> {
            Some(self.arr::<1>()?[0])
        }
        fn i32(&mut self) -> Option<i32> {
            Some(i32::from_be_bytes(self.arr()?))
        }
        fn len(&mut self) -> Option<usize> {
            usize::try_from(self.i32()?).ok()
        }
        fn string(&mut self) -> Option<String> {
            let n = u16::from_be_bytes(self.arr()?) as usize;
            // Modified UTF-8: для обычных строк совпадает с UTF-8.
            Some(String::from_utf8_lossy(self.take(n)?).into_owned())
        }
        fn payload(&mut self, id: u8, depth: usize) -> Option<Tag> {
            if depth > MAX_DEPTH {
                return None;
            }
            Some(match id {
                1 => Tag::Byte(self.u8()? as i8),
                2 => Tag::Short(i16::from_be_bytes(self.arr()?)),
                3 => Tag::Int(self.i32()?),
                4 => Tag::Long(i64::from_be_bytes(self.arr()?)),
                5 => Tag::Float(f32::from_be_bytes(self.arr()?)),
                6 => Tag::Double(f64::from_be_bytes(self.arr()?)),
                7 | 11 | 12 => {
                    let n = self.len()?;
                    let width = match id {
                        7 => 1,
                        11 => 4,
                        _ => 8,
                    };
                    self.take(n.checked_mul(width)?)?;
                    Tag::Array(n)
                }
                8 => Tag::String(self.string()?),
                9 => {
                    let elem = self.u8()?;
                    let n = self.len()?;
                    // Каждый элемент занимает минимум байт — защита от огромной длины в битом файле.
                    if n > self.data.len() - self.pos {
                        return None;
                    }
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(self.payload(elem, depth + 1)?);
                    }
                    Tag::List(v)
                }
                10 => {
                    let mut v = vec![];
                    loop {
                        let t = self.u8()?;
                        if t == 0 {
                            break;
                        }
                        let name = self.string()?;
                        v.push((name, self.payload(t, depth + 1)?));
                    }
                    Tag::Compound(v)
                }
                _ => return None,
            })
        }
    }
}

// ---------- Прегенерация (Chunky) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkyKind {
    /// Плагин Bukkit / Paper: `plugins/Chunky-*.jar`.
    Plugin,
    /// Мод Fabric / NeoForge / Forge: `mods/Chunky*.jar`.
    Mod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkyInstall {
    pub kind: ChunkyKind,
    pub path: PathBuf,
}

/// Установлен ли Chunky (включённый `.jar` в `plugins/` или `mods/`; ChunkyBorder не считается).
pub fn chunky_installed(dir: &Path) -> Option<ChunkyInstall> {
    for (sub, kind) in [("plugins", ChunkyKind::Plugin), ("mods", ChunkyKind::Mod)] {
        let Ok(rd) = std::fs::read_dir(dir.join(sub)) else { continue };
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_lowercase();
                n.starts_with("chunky") && !n.starts_with("chunkyborder") && n.ends_with(".jar")
            })
            .map(|e| e.path())
            .collect();
        found.sort();
        if let Some(path) = found.pop() {
            return Some(ChunkyInstall { kind, path });
        }
    }
    None
}

/// Имя мира для `chunky world <w>`: у плагина — имя мира Bukkit (`world`, `world_nether`, `world_the_end`),
/// у мода — идентификатор измерения (`minecraft:the_nether`).
pub fn chunky_world(kind: ChunkyKind, level_name: &str, dim: &Dimension) -> String {
    match (kind, dim) {
        (ChunkyKind::Plugin, Dimension::Overworld) => level_name.to_string(),
        (ChunkyKind::Plugin, Dimension::Nether | Dimension::End) => {
            format!("{level_name}{}", dim.bukkit_suffix().unwrap_or_default())
        }
        _ => dim.id(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PregenCenter {
    /// Точка 0, 0.
    Origin,
    /// Точка появления мира (`chunky spawn`).
    Spawn,
    /// Заданная точка (пока задаётся только в тестах).
    #[allow(dead_code)]
    At(i64, i64),
}

/// Команды консоли для запуска прегенерации квадрата радиусом `radius` блоков вокруг центра.
pub fn pregen_commands(world: &str, center: PregenCenter, radius: u32) -> Vec<String> {
    let center = match center {
        PregenCenter::Origin => "chunky center 0 0".to_string(),
        PregenCenter::Spawn => "chunky spawn".to_string(),
        PregenCenter::At(x, z) => format!("chunky center {x} {z}"),
    };
    vec![format!("chunky world {world}"), center, format!("chunky radius {radius}"), "chunky start".into()]
}

/// Пауза / продолжение / отмена (отмена требует подтверждения — вторая команда).
pub const PREGEN_PAUSE: &str = "chunky pause";
pub const PREGEN_CONTINUE: &str = "chunky continue";
pub const PREGEN_CANCEL: [&str; 2] = ["chunky cancel", "chunky confirm"];

/// Сколько чанков в квадрате радиусом `radius` блоков (для оценки до запуска).
pub fn pregen_chunk_count(radius: u32) -> u64 {
    let side = (2 * radius as u64).div_ceil(16);
    side * side
}

#[derive(Debug, Clone, PartialEq)]
pub struct PregenProgress {
    pub world: String,
    pub chunks: u64,
    pub percent: f32,
    /// Оставшееся время (для завершённой задачи — `None`).
    pub eta: Option<Duration>,
    /// Чанков в секунду (для завершённой задачи — `None`).
    pub rate: Option<f64>,
    /// Задача завершена («Task finished»); тогда `total_time` — сколько она шла.
    pub finished: bool,
    pub total_time: Option<Duration>,
}

/// Разбор строки прогресса Chunky из лога / консоли:
/// `[Chunky] Task running for world. Processed: 1234 chunks (12.34%), ETA: 0:05:12, Rate: 250.0 cps, Current: -12, 34`
/// и `[Chunky] Task finished for world. Processed: 1234 chunks (100.00%), Total time: 0:05:12`.
/// Числа могут быть с десятичной запятой (Chunky форматирует их в локали JVM).
pub fn parse_pregen_progress(line: &str) -> Option<PregenProgress> {
    let s = crate::log::strip_codes(line);
    let (finished, rest) = if let Some(i) = s.find("Task running for ") {
        (false, &s[i + "Task running for ".len()..])
    } else {
        let i = s.find("Task finished for ")?;
        (true, &s[i + "Task finished for ".len()..])
    };
    let (world, rest) = rest.split_once(". Processed: ")?;
    let (chunks, rest) = rest.split_once(" chunks (")?;
    let (percent, rest) = rest.split_once("%)")?;
    let after = |key: &str, end: &str| -> Option<String> {
        let i = rest.find(key)? + key.len();
        let tail = &rest[i..];
        Some(tail[..tail.find(end).unwrap_or(tail.len())].trim().to_string())
    };
    let progress = PregenProgress {
        world: world.trim().to_string(),
        chunks: chunks.trim().replace([',', '.', ' ', '\u{a0}'], "").parse().ok()?,
        percent: decimal(percent)? as f32,
        eta: if finished { None } else { after("ETA: ", ",").and_then(|t| hms(&t)) },
        rate: if finished { None } else { after("Rate: ", " cps").and_then(|t| decimal(&t)) },
        finished,
        total_time: if finished { after("Total time: ", ",").and_then(|t| hms(&t)) } else { None },
    };
    Some(progress)
}

/// `[Chunky] Task stopped for world.` (пауза или отмена) → имя мира.
#[cfg(test)]
pub fn parse_pregen_stopped(line: &str) -> Option<String> {
    let s = crate::log::strip_codes(line);
    let i = s.find("Task stopped for ")? + "Task stopped for ".len();
    Some(s[i..].trim().trim_end_matches('.').to_string())
}

fn decimal(s: &str) -> Option<f64> {
    s.trim().replace(',', ".").parse().ok()
}

/// `0:05:12` → 312 с.
fn hms(s: &str) -> Option<Duration> {
    let parts: Vec<u64> = s.trim().split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let secs = parts.iter().fold(0u64, |acc, p| acc.saturating_mul(60).saturating_add(*p));
    (!parts.is_empty() && parts.len() <= 3).then(|| Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pechka-world-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn file(p: &Path, len: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![7u8; len]).unwrap();
    }

    /// NBT-строка (имя тега или значение).
    fn s(out: &mut Vec<u8>, v: &str) {
        out.extend((v.len() as u16).to_be_bytes());
        out.extend(v.as_bytes());
    }

    /// level.dat: Data{LevelName, Version{Name}, WorldGenSettings{seed}, LastPlayed, DataPacks{Enabled:[..]}}.
    fn level_dat(path: &Path, seed: Option<i64>) {
        let mut b = vec![10u8];
        s(&mut b, "");
        b.push(10);
        s(&mut b, "Data");
        b.push(8);
        s(&mut b, "LevelName");
        s(&mut b, "world");
        b.push(10);
        s(&mut b, "Version");
        b.push(8);
        s(&mut b, "Name");
        s(&mut b, "1.21.4");
        b.push(0);
        if let Some(seed) = seed {
            b.push(10);
            s(&mut b, "WorldGenSettings");
            b.push(4);
            s(&mut b, "seed");
            b.extend(seed.to_be_bytes());
            b.push(0);
        }
        b.push(4);
        s(&mut b, "LastPlayed");
        b.extend(1_700_000_000_000i64.to_be_bytes());
        b.push(9);
        s(&mut b, "Enabled");
        b.push(8);
        b.extend(2i32.to_be_bytes());
        s(&mut b, "vanilla");
        s(&mut b, "paper");
        b.push(12);
        s(&mut b, "Longs");
        b.extend(2i32.to_be_bytes());
        b.extend([0u8; 16]);
        b.push(0);
        b.push(0);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(path).unwrap(), flate2::Compression::fast());
        gz.write_all(&b).unwrap();
        gz.finish().unwrap();
    }

    #[test]
    fn scan_modern_layout() {
        let d = tmp("modern");
        let w = d.join("world");
        level_dat(&w.join("level.dat"), Some(-42));
        file(&w.join("dimensions/minecraft/overworld/region/r.0.0.mca"), 1000);
        file(&w.join("dimensions/minecraft/overworld/region/r.0.1.mca"), 1000);
        file(&w.join("dimensions/minecraft/the_nether/region/r.0.0.mca"), 300);
        file(&w.join("dimensions/minecraft/the_nether/paper-world.yml"), 10);
        file(&w.join("dimensions/mymod/mining/region/r.0.0.mca"), 50);
        file(&w.join("players/stats/a.json"), 5);
        let info = scan(&d, "world");
        assert_eq!(info.layout, Some(Layout::Modern));
        assert!(info.exists);
        assert_eq!(info.seed, Some(-42));
        assert_eq!(info.version.as_deref(), Some("1.21.4"));
        assert!(info.last_played.is_some());
        let ow = info.dimension(&Dimension::Overworld).unwrap();
        assert_eq!((ow.region_files, ow.size_bytes), (2, 2000));
        let ne = info.dimension(&Dimension::Nether).unwrap();
        assert_eq!((ne.exists, ne.region_files, ne.size_bytes), (true, 1, 310));
        assert!(!info.dimension(&Dimension::End).unwrap().exists);
        assert_eq!(info.dimension(&Dimension::Custom("mymod:mining".into())).unwrap().region_files, 1);
        assert!(info.total_bytes >= 2000 + 310 + 50 + 5);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scan_vanilla_and_bukkit_layouts() {
        let d = tmp("legacy");
        // vanilla: DIM-1 / DIM1 внутри мира
        file(&d.join("vworld/level.dat"), 10);
        file(&d.join("vworld/region/r.0.0.mca"), 100);
        file(&d.join("vworld/DIM-1/region/r.0.0.mca"), 40);
        file(&d.join("vworld/DIM1/region/r.0.0.mca"), 20);
        file(&d.join("vworld/DIM1/region/r.1.0.mca"), 20);
        let v = scan(&d, "vworld");
        assert_eq!(v.layout, Some(Layout::Vanilla));
        assert_eq!(v.seed, None); // level.dat не NBT — просто нет сида
        let ow = v.dimension(&Dimension::Overworld).unwrap();
        assert_eq!((ow.region_files, ow.size_bytes), (1, 110)); // без DIM-1 / DIM1
        assert_eq!(v.dimension(&Dimension::End).unwrap().region_files, 2);
        assert_eq!(v.total_bytes, 190);
        // bukkit: отдельные папки
        level_dat(&d.join("world/level.dat"), None);
        file(&d.join("world/region/r.0.0.mca"), 100);
        file(&d.join("world_nether/level.dat"), 10);
        file(&d.join("world_nether/DIM-1/region/r.0.0.mca"), 40);
        file(&d.join("world_the_end/level.dat"), 10);
        let b = scan(&d, "world");
        assert_eq!(b.layout, Some(Layout::Bukkit));
        let ne = b.dimension(&Dimension::Nether).unwrap();
        assert_eq!(ne.path, d.join("world_nether"));
        assert_eq!((ne.region_files, ne.size_bytes), (1, 50));
        assert!(b.dimension(&Dimension::End).unwrap().exists);
        assert_eq!(b.other_worlds, vec!["vworld".to_string()]);
        assert_eq!(list_worlds(&d), vec!["vworld".to_string(), "world".to_string()]);
        // нет мира
        let none = scan(&d, "nope");
        assert_eq!((none.layout, none.exists), (None, false));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reset_moves_aside_all_layouts_and_rotates() {
        let d = tmp("reset");
        file(&d.join("world/level.dat"), 1);
        // modern
        file(&d.join("world/dimensions/minecraft/the_nether/region/r.0.0.mca"), 5);
        file(&d.join("world/dimensions/minecraft/the_nether/paper-world.yml"), 3);
        file(&d.join("world/dimensions/minecraft/the_nether/data/minecraft/world_gen_settings.dat"), 1);
        let st = reset_dimension(&d, "world", Dimension::Nether).unwrap();
        assert!(st.starts_with(d.join(RESET_STASH_DIR)));
        assert!(st.join("world/dimensions/minecraft/the_nether/region/r.0.0.mca").exists());
        assert!(!d.join("world/dimensions/minecraft/the_nether/region").exists());
        assert!(d.join("world/dimensions/minecraft/the_nether/paper-world.yml").exists()); // настройки на месте
        assert!(d.join("world/dimensions/minecraft/the_nether/data/minecraft/world_gen_settings.dat").exists());
        // bukkit
        file(&d.join("world_the_end/level.dat"), 1);
        file(&d.join("world_the_end/DIM1/region/r.0.0.mca"), 5);
        let st2 = reset_dimension(&d, "world", Dimension::End).unwrap();
        assert_ne!(st, st2);
        assert!(st2.join("world_the_end/DIM1/region/r.0.0.mca").exists());
        assert!(d.join("world_the_end/level.dat").exists()); // сам мир Bukkit не трогаем
        assert!(!d.join("world_the_end/DIM1").exists());
        // vanilla
        file(&d.join("world/DIM-1/region/r.0.0.mca"), 5);
        reset_dimension(&d, "world", Dimension::Nether).unwrap();
        assert!(!d.join("world/DIM-1").exists());
        // ротация: остаются последние 3
        file(&d.join("world/DIM1/region/r.0.0.mca"), 5);
        let last = reset_dimension(&d, "world", Dimension::End).unwrap();
        let stashes = reset_stashes(&d);
        assert_eq!(stashes.len(), 3);
        assert_eq!(stashes[0], last);
        assert!(!st.exists());
        // нечего сбрасывать (остались только настройки; ландшафта верхнего мира нет)
        assert!(reset_dimension(&d, "world", Dimension::Nether).is_err());
        assert!(reset_dimension(&d, "world", Dimension::End).is_err());
        assert!(reset_dimension(&d, "world", Dimension::Overworld).is_err());
        assert!(reset_dimension(&d, "../x", Dimension::Nether).is_err());
        assert!(d.join("world/level.dat").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn reset_rolls_back_when_a_move_fails() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("reset-rollback");
        file(&d.join("world/dimensions/minecraft/overworld/region/r.0.0.mca"), 5);
        file(&d.join("world/region/r.0.0.mca"), 5);
        // Первый перенос (26.x) проходит, второй (из <level>, только для чтения) — нет.
        let level = d.join("world");
        std::fs::set_permissions(&level, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(level.join("probe"), b"").is_ok() {
            // root (например, Docker CI) пишет и в папку только для чтения — сбой не воспроизвести
            std::fs::set_permissions(&level, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        let res = reset_dimension(&d, "world", Dimension::Overworld);
        std::fs::set_permissions(&level, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(res.is_err());
        assert!(d.join("world/dimensions/minecraft/overworld/region/r.0.0.mca").exists());
        assert!(d.join("world/region/r.0.0.mca").exists());
        assert!(reset_stashes(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fresh_stash_survives_rotation_after_clock_jump() {
        let d = tmp("reset-clock");
        for n in 1..=3 {
            std::fs::create_dir_all(d.join(RESET_STASH_DIR).join(format!("99991231-23595{n}"))).unwrap();
        }
        file(&d.join("world/DIM-1/region/r.0.0.mca"), 5);
        let st = reset_dimension(&d, "world", Dimension::Nether).unwrap();
        assert!(st.join("world/DIM-1/region/r.0.0.mca").exists());
        assert_eq!(reset_stashes(&d).len(), 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn overworld_reset_moves_only_terrain() {
        let d = tmp("reset-overworld");
        // vanilla / bukkit: ландшафт прямо в <level>
        for f in ["level.dat", "region/r.0.0.mca", "entities/r.0.0.mca", "poi/r.0.0.mca", "playerdata/a.dat"] {
            file(&d.join("world").join(f), 2);
        }
        file(&d.join("world/data/raids.dat"), 1);
        file(&d.join("world/datapacks/p.zip"), 1);
        file(&d.join("world/DIM-1/region/r.0.0.mca"), 3);
        file(&d.join("world_the_end/DIM1/region/r.0.0.mca"), 3);
        let st = reset_dimension(&d, "world", Dimension::Overworld).unwrap();
        for f in ["region/r.0.0.mca", "entities/r.0.0.mca", "poi/r.0.0.mca"] {
            assert!(st.join("world").join(f).exists(), "{f}");
            assert!(!d.join("world").join(f).exists(), "{f}");
        }
        for f in ["level.dat", "playerdata/a.dat", "data/raids.dat", "datapacks/p.zip", "DIM-1/region/r.0.0.mca"] {
            assert!(d.join("world").join(f).exists(), "{f}");
        }
        assert!(d.join("world_the_end/DIM1/region/r.0.0.mca").exists());
        // повторно — уже нечего
        assert!(reset_dimension(&d, "world", Dimension::Overworld).is_err());
        // modern: только ландшафт; сид, правила и настройки Paper остаются в папке измерения
        let ow = d.join("world/dimensions/minecraft/overworld");
        for f in ["region/r.0.0.mca", "entities/r.0.0.mca", "poi/r.0.0.mca"] {
            file(&ow.join(f), 5);
        }
        file(&ow.join("paper-world.yml"), 1);
        file(&ow.join("data/minecraft/world_gen_settings.dat"), 1);
        file(&ow.join("data/minecraft/game_rules.dat"), 1);
        file(&d.join("world/players/data/a.dat"), 1);
        let st2 = reset_dimension(&d, "world", Dimension::Overworld).unwrap();
        for f in ["region/r.0.0.mca", "entities/r.0.0.mca", "poi/r.0.0.mca"] {
            assert!(st2.join("world/dimensions/minecraft/overworld").join(f).exists(), "{f}");
            assert!(!ow.join(f).exists(), "{f}");
        }
        for f in ["paper-world.yml", "data/minecraft/world_gen_settings.dat", "data/minecraft/game_rules.dat"] {
            assert!(ow.join(f).exists(), "{f}");
        }
        assert!(d.join("world/players/data/a.dat").exists());
        assert!(d.join("world/level.dat").exists());
        // измерения модов не сбрасываются
        file(&d.join("world/dimensions/mod/sky/region/r.0.0.mca"), 1);
        assert!(reset_dimension(&d, "world", Dimension::Custom("mod:sky".into())).is_err());
        assert!(d.join("world/dimensions/mod/sky/region/r.0.0.mca").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn level_name_is_written_preserving_file() {
        let d = tmp("lvl");
        let text = "#Minecraft server properties\nmotd=Hi\nlevel-name=world\npvp=true\n";
        std::fs::write(d.join("server.properties"), text).unwrap();
        set_level_name(&d, "creative").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("server.properties")).unwrap(),
            text.replace("level-name=world", "level-name=creative")
        );
        assert_eq!(current_level_name(&d), "creative");
        assert!(set_level_name(&d, "a/b").is_err());
        assert!(set_level_name(&d, "").is_err());
        assert!(set_level_name(&d, "..").is_err());
        let e = tmp("lvl-new");
        assert_eq!(current_level_name(&e), "world");
        set_level_name(&e, "Мой мир").unwrap();
        assert_eq!(current_level_name(&e), "Мой мир");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn nbt_rejects_garbage() {
        assert!(nbt::parse(&[]).is_none());
        assert!(nbt::parse(&[10, 0, 0, 9, 0, 1, b'x', 10, 0x7f, 0xff, 0xff, 0xff]).is_none());
        assert!(nbt::parse(&[10, 0, 0, 1, 0, 1, b'b']).is_none()); // обрыв
        let d = tmp("nbt");
        std::fs::write(d.join("level.dat"), [0x1f, 0x8b, 1, 2, 3]).unwrap();
        assert!(read_level_dat(&d.join("level.dat")).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn real_level_dat_if_present() {
        // Только чтение, если рядом лежит настоящий сервер.
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../world/level.dat");
        if p.exists() {
            assert!(read_level_dat(&p).and_then(|l| l.version).is_some());
        }
    }

    #[test]
    fn chunky_detection_and_commands() {
        let d = tmp("chunky");
        assert_eq!(chunky_installed(&d), None);
        file(&d.join("plugins/ChunkyBorder-1.2.jar"), 1);
        file(&d.join("plugins/Chunky-Bukkit-1.4.40.jar.disabled"), 1);
        assert_eq!(chunky_installed(&d), None);
        file(&d.join("mods/Chunky-Fabric-1.4.40.jar"), 1);
        assert_eq!(chunky_installed(&d).unwrap().kind, ChunkyKind::Mod);
        file(&d.join("plugins/Chunky-Bukkit-1.4.40.jar"), 1);
        assert_eq!(chunky_installed(&d).unwrap().kind, ChunkyKind::Plugin);
        assert_eq!(chunky_world(ChunkyKind::Plugin, "world", &Dimension::Nether), "world_nether");
        assert_eq!(chunky_world(ChunkyKind::Plugin, "world", &Dimension::Overworld), "world");
        assert_eq!(chunky_world(ChunkyKind::Mod, "world", &Dimension::End), "minecraft:the_end");
        assert_eq!(
            pregen_commands("world", PregenCenter::Origin, 5000),
            vec!["chunky world world", "chunky center 0 0", "chunky radius 5000", "chunky start"]
        );
        assert_eq!(pregen_commands("w", PregenCenter::Spawn, 100)[1], "chunky spawn");
        assert_eq!(pregen_commands("w", PregenCenter::At(-5, 7), 100)[1], "chunky center -5 7");
        assert_eq!(pregen_chunk_count(5000), 625 * 625);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn chunky_progress_lines() {
        let p = parse_pregen_progress(
            "[12:00:00 INFO]: [Chunky] Task running for world. Processed: 1234 chunks (12.34%), ETA: 0:05:12, Rate: 250.0 cps, Current: -12, 34",
        )
        .unwrap();
        assert_eq!(p.world, "world");
        assert_eq!(p.chunks, 1234);
        assert!((p.percent - 12.34).abs() < 1e-4);
        assert_eq!(p.eta, Some(Duration::from_secs(312)));
        assert_eq!(p.rate, Some(250.0));
        assert!(!p.finished);
        // русская локаль JVM, цвета, namespaced-мир Fabric
        let p = parse_pregen_progress(
            "§2[Chunky]§r Task running for minecraft:the_nether. Processed: 99 chunks (0,50%), ETA: 12:00:01, Rate: 31,5 cps, Current: 1, 2",
        )
        .unwrap();
        assert_eq!(p.world, "minecraft:the_nether");
        assert!((p.percent - 0.5).abs() < 1e-4);
        assert_eq!(p.rate, Some(31.5));
        assert_eq!(p.eta, Some(Duration::from_secs(12 * 3600 + 1)));
        let f = parse_pregen_progress(
            "[Server thread/INFO]: [Chunky] Task finished for world_the_end. Processed: 40000 chunks (100.00%), Total time: 0:10:05",
        )
        .unwrap();
        assert!(f.finished);
        assert_eq!((f.percent, f.eta, f.rate), (100.0, None, None));
        assert_eq!(f.total_time, Some(Duration::from_secs(605)));
        assert_eq!(parse_pregen_progress("[Chunky] Task started in world for the square region"), None);
        assert_eq!(parse_pregen_progress("<Steve> Task running for fun"), None);
        assert_eq!(parse_pregen_stopped("[INFO]: [Chunky] Task stopped for world."), Some("world".into()));
    }
}
