//! Бэкапы: save-off → save-all flush → архив `.tar.gz` или инкрементальный снимок → save-on, ротация,
//! затем копирование в другие места (см. [`crate::backup_copy`]).
//!
//! Имена бэкапов строго `<prefix>-YYYYmmdd-HHMMSS.tar.gz` (полный архив) и
//! `<prefix>-YYYYmmdd-HHMMSS.snap/` (снимок). Всё остальное в папке бэкапов (в том числе недописанные
//! `….tar.gz.part` и `….snap.partial/`) не показывается и ротацией не удаляется — кроме недописанных
//! остатков этого же сервера, которые чистятся.
//!
//! Снимок — папка с деревом `backup_paths`. Файл, не изменившийся с предыдущего снимка (тот же размер и
//! время изменения, а для маленьких файлов — и то же содержимое), не копируется, а становится жёсткой
//! ссылкой на копию из предыдущего снимка. Поэтому каждый снимок выглядит полным (его можно удалить или
//! восстановить отдельно), а места занимает только изменившееся.

use crate::config::BackupMode;
use crate::event::{AppEvent, BackupEvent};
use crate::tr;
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::{broadcast, mpsc};

const SAVE_TIMEOUT: Duration = Duration::from_secs(60);
/// Сводка снимка внутри его папки.
pub const SNAPSHOT_META: &str = ".pechka-snapshot.json";
const ARCHIVE_EXT: &str = ".tar.gz";
const SNAP_EXT: &str = ".snap";
/// Недописанный снимок: `<name>.snap.partial/`.
const SNAP_PARTIAL: &str = ".partial";
/// Недописанный архив: `<name>.tar.gz.part`.
const ARCHIVE_PARTIAL: &str = ".part";
/// Файлы до этого размера при совпадении размера и времени ещё и сравниваются побайтно.
const SMALL_FILE: u64 = 256 * 1024;

pub struct BackupJob {
    pub id: usize,
    pub name: String,
    pub root: PathBuf,
    pub paths: Vec<String>,
    pub backup_dir: PathBuf,
    pub keep: usize,
    pub mode: BackupMode,
    /// `backup_copy_to` из конфига: папки и `rclone:<remote>:<path>`.
    pub copy_to: Vec<String>,
    /// stdin и поток строк запущенного сервера; None — сервер остановлен.
    pub live: Option<(mpsc::UnboundedSender<String>, broadcast::Receiver<String>)>,
    /// Файл-отметка «бэкап выключил автосохранение» (см. [`take_save_off_mark`]); вместе с `live`.
    pub save_off_mark: Option<PathBuf>,
}

/// Отметка пишется перед `save-off` и удаляется после `save-on`. Если служба умерла посреди бэкапа,
/// сервер (он переживает службу) остался бы без автосохранения до своего перезапуска — новая служба
/// находит отметку при подключении и включает сохранение обратно. Ручной `save-off` из консоли не трогается.
fn put_save_off_mark(mark: &Path) {
    if let Some(dir) = mark.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(mark, b"save-off\n");
}

/// Убирает отметку `save-off`; `true` — она была, то есть автосохранение надо включить (`save-on`).
pub fn take_save_off_mark(mark: &Path) -> bool {
    std::fs::remove_file(mark).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    /// Архив `.tar.gz`.
    Full,
    /// Снимок-папка `.snap` с жёсткими ссылками на предыдущий.
    Incremental,
}

#[derive(Debug, Clone)]
pub struct BackupInfo {
    pub path: PathBuf,
    pub kind: BackupKind,
    /// Full — размер архива. Incremental — сколько байт было новых (скопированных, а не связанных
    /// жёсткой ссылкой) при создании снимка; столько примерно освободит его удаление.
    pub size: u64,
    /// Full — то же, что `size`. Incremental — полный размер всех файлов снимка.
    #[allow(dead_code)] // для вкладки «Бэкапы»
    pub total_size: u64,
    /// Число файлов (только для снимков).
    #[allow(dead_code)] // для вкладки «Бэкапы»
    pub files: Option<u64>,
    pub modified: Option<SystemTime>,
}

/// Содержимое `<snap>/.pechka-snapshot.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub files: u64,
    pub total_bytes: u64,
    pub new_bytes: u64,
    /// Время создания, unix-секунды.
    #[serde(default)]
    pub created: i64,
}

/// Итог создания снимка.
#[derive(Debug, Default)]
pub struct SnapshotStats {
    pub meta: SnapshotMeta,
    /// Сколько файлов стали жёсткими ссылками.
    pub linked: u64,
    /// Жёсткие ссылки не получились (FAT/exFAT, другой диск…) — всё скопировано целиком.
    pub link_error: Option<String>,
}

pub fn file_prefix(name: &str) -> String {
    name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// `<prefix>-YYYYmmdd-HHMMSS.tar.gz` → Full, `<prefix>-YYYYmmdd-HHMMSS.snap` → Incremental, иначе None.
pub fn backup_kind(name: &str, prefix: &str) -> Option<BackupKind> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
    let (kind, stamp) = match rest.strip_suffix(ARCHIVE_EXT) {
        Some(s) => (BackupKind::Full, s),
        None => (BackupKind::Incremental, rest.strip_suffix(SNAP_EXT)?),
    };
    is_stamp(stamp).then_some(kind)
}

/// `YYYYmmdd-HHMMSS`
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 15 && b[8] == b'-' && b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit())
}

/// Недописанный бэкап этого сервера (`….snap.partial`, `….tar.gz.part`).
fn is_partial_name(name: &str, prefix: &str) -> bool {
    name.strip_suffix(SNAP_PARTIAL).is_some_and(|n| backup_kind(n, prefix) == Some(BackupKind::Incremental))
        || name.strip_suffix(ARCHIVE_PARTIAL).is_some_and(|n| backup_kind(n, prefix) == Some(BackupKind::Full))
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(SNAP_PARTIAL);
    PathBuf::from(s)
}

pub fn backup_name(prefix: &str, mode: BackupMode) -> String {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    match mode {
        BackupMode::Full => format!("{prefix}-{stamp}{ARCHIVE_EXT}"),
        BackupMode::Incremental => format!("{prefix}-{stamp}{SNAP_EXT}"),
    }
}

pub async fn run(job: BackupJob, tx: mpsc::UnboundedSender<AppEvent>) {
    let id = job.id;
    let progress = |s: String| {
        let _ = tx.send(AppEvent::Backup { id, ev: BackupEvent::Progress(s) });
    };
    let mut warnings: Vec<String> = vec![];
    let live = job.live;
    let mark = job.save_off_mark.filter(|_| live.is_some());
    if let Some((stdin, mut lines)) = live.as_ref().map(|(s, l)| (s.clone(), l.resubscribe())) {
        progress("save-off, save-all flush…".into());
        if let Some(m) = &mark {
            put_save_off_mark(m);
        }
        let _ = stdin.send("save-off".into());
        let _ = stdin.send("save-all flush".into());
        let waited = tokio::time::timeout(SAVE_TIMEOUT, async {
            loop {
                match lines.recv().await {
                    Ok(l) if l.contains("Saved the game") => return true,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return false,
                }
            }
        })
        .await;
        if !matches!(waited, Ok(true)) {
            warnings.push(
                tr!(
                    "сервер не подтвердил сохранение (Saved the game) — архив может быть неполным",
                    "the server did not confirm the save (Saved the game) — the archive may be incomplete"
                )
                .to_string(),
            );
        }
    }
    let mode = job.mode;
    progress(match mode {
        BackupMode::Full => tr!("архивирование…", "archiving…").into(),
        BackupMode::Incremental => tr!("снимок…", "snapshot…").into(),
    });
    let tx2 = tx.clone();
    let (root, paths, dir, prefix, keep) = (job.root, job.paths, job.backup_dir, file_prefix(&job.name), job.keep);
    let (root2, prefix2) = (root.clone(), prefix.clone());
    let result = tokio::task::spawn_blocking(move || {
        let dest = dir.join(backup_name(&prefix, mode));
        let res = match mode {
            BackupMode::Full => {
                let bytes = create_archive(&root, &paths, &dest, |n| {
                    let _ = tx2.send(AppEvent::Backup {
                        id,
                        ev: BackupEvent::Progress(tr!(
                            format!("архивирование… {n} файлов"),
                            format!("archiving… {n} files")
                        )),
                    });
                })?;
                (dest, bytes, None)
            }
            BackupMode::Incremental => {
                let prev = latest_snapshot(&dir, &prefix);
                let st = create_snapshot(&root, &paths, &dest, prev.as_deref(), std::slice::from_ref(&dir), |n| {
                    let _ = tx2.send(AppEvent::Backup {
                        id,
                        ev: BackupEvent::Progress(tr!(format!("снимок… {n} файлов"), format!("snapshot… {n} files"))),
                    });
                })?;
                (dest, st.meta.new_bytes, st.link_error.map(|e| link_warning(&e)))
            }
        };
        rotate(&dir, &prefix, keep)?;
        Ok::<_, anyhow::Error>(res)
    })
    .await;
    // Копирование в другие места мир уже не трогает — сохранение можно включать сразу.
    if let Some((stdin, _)) = &live {
        let _ = stdin.send("save-on".into());
        if let Some(m) = &mark {
            take_save_off_mark(m);
        }
    }
    let ev = match result {
        Ok(Ok((path, bytes, link_warn))) => {
            warnings.extend(link_warn);
            if !job.copy_to.is_empty() {
                let (tx3, path2, targets) = (tx.clone(), path.clone(), job.copy_to);
                let copied = tokio::task::spawn_blocking(move || {
                    crate::backup_copy::copy_all(&root2, &targets, &path2, &prefix2, keep, |s| {
                        let _ = tx3.send(AppEvent::Backup { id, ev: BackupEvent::Progress(s) });
                    })
                })
                .await;
                match copied {
                    Ok(w) => warnings.extend(w),
                    Err(e) => warnings.push(e.to_string()),
                }
            }
            let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
            BackupEvent::Done { path, bytes, warning }
        }
        Ok(Err(e)) => BackupEvent::Failed(format!("{e:#}")),
        Err(e) => BackupEvent::Failed(e.to_string()),
    };
    let _ = tx.send(AppEvent::Backup { id, ev });
}

pub(crate) fn link_warning(e: &str) -> String {
    tr!(
        format!("жёсткие ссылки не работают на этом диске ({e}) — файлы скопированы целиком"),
        format!("hard links do not work on this disk ({e}) — files were copied in full")
    )
}

/// Сколько копий «мира до восстановления» хранить в `.pechka-before-restore/`.
const KEEP_BEFORE_RESTORE: usize = 3;

/// Восстановление из архива `.tar.gz` или из снимка `.snap/` (сервер должен быть остановлен). Текущие
/// `paths` не удаляются, а переносятся в `<root>/.pechka-before-restore/<время>/` (хранятся последние 3).
/// Возвращает эту папку.
pub fn restore(root: &Path, backup: &Path, paths: &[String], mut progress: impl FnMut(String)) -> Result<PathBuf> {
    if backup.is_dir() {
        return restore_snapshot(root, backup, paths, progress);
    }
    // Проверяем архив до того, как что-то трогать.
    progress(crate::tr!("проверка архива…", "checking archive…").into());
    let open = || -> Result<tar::Archive<flate2::read::GzDecoder<std::fs::File>>> {
        let f = std::fs::File::open(backup).with_context(|| backup.display().to_string())?;
        Ok(tar::Archive::new(flate2::read::GzDecoder::new(f)))
    };
    let mut total = 0u64;
    for e in open()?.entries()? {
        let e = e?;
        let p = e.path()?.into_owned();
        if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            anyhow::bail!(crate::tr!(
                format!("в архиве недопустимый путь: {}", p.display()),
                format!("archive contains an unsafe path: {}", p.display())
            ));
        }
        total += 1;
    }
    let stash = stash_current(root, paths)?;
    let mut done = 0u64;
    for e in open()?.entries()? {
        let mut e = e?;
        e.unpack_in(root)?;
        done += 1;
        if done.is_multiple_of(200) {
            progress(crate::tr!(format!("распаковка… {done}/{total}"), format!("extracting… {done}/{total}")));
        }
    }
    rotate_stash(root);
    Ok(stash)
}

/// Переносит текущие `paths` в `<root>/.pechka-before-restore/<время>/`.
fn stash_current(root: &Path, paths: &[String]) -> Result<PathBuf> {
    let stash = root.join(".pechka-before-restore").join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    std::fs::create_dir_all(&stash).with_context(|| stash.display().to_string())?;
    for p in paths {
        let src = root.join(p);
        if src.exists() {
            let dst = stash.join(p);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&src, &dst).with_context(|| format!("{} → {}", src.display(), dst.display()))?;
        }
    }
    Ok(stash)
}

fn rotate_stash(root: &Path) {
    if let Ok(rd) = std::fs::read_dir(root.join(".pechka-before-restore")) {
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        while dirs.len() > KEEP_BEFORE_RESTORE {
            let _ = std::fs::remove_dir_all(dirs.remove(0));
        }
    }
}

/// Восстановление из снимка: файлы КОПИРУЮТСЯ (не связываются ссылками — иначе сервер, дописывая
/// регион, испортил бы и снимок).
fn restore_snapshot(root: &Path, snap: &Path, paths: &[String], mut progress: impl FnMut(String)) -> Result<PathBuf> {
    progress(crate::tr!("проверка снимка…", "checking snapshot…").into());
    let name = snap.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if !name.ends_with(SNAP_EXT) || std::fs::symlink_metadata(snap).map(|m| !m.is_dir()).unwrap_or(true) {
        bail!(
            "{}",
            tr!(format!("не снимок бэкапа: {}", snap.display()), format!("not a backup snapshot: {}", snap.display()))
        );
    }
    let entries = top_entries(snap)?;
    let mut total = 0u64;
    for e in &entries {
        total += tree_size(&snap.join(e)).0;
    }
    let stash = stash_current(root, paths)?;
    let mut done = 0u64;
    for e in &entries {
        copy_tree(&snap.join(e), &root.join(e), &mut |n| {
            done += n;
            if done.is_multiple_of(200) {
                progress(crate::tr!(format!("копирование… {done}/{total}"), format!("copying… {done}/{total}")));
            }
        })?;
    }
    rotate_stash(root);
    Ok(stash)
}

/// Содержимое папки снимка, кроме служебного файла.
fn top_entries(snap: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut v: Vec<_> = std::fs::read_dir(snap)
        .with_context(|| snap.display().to_string())?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<Result<_, _>>()?;
    v.retain(|n| n != SNAPSHOT_META);
    v.sort();
    Ok(v)
}

/// Обычное рекурсивное копирование (с сохранением времени изменения); символьные ссылки пропускаются.
fn copy_tree(src: &Path, dst: &Path, on_file: &mut impl FnMut(u64)) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst).with_context(|| dst.display().to_string())?;
        let mut entries: Vec<_> = std::fs::read_dir(src)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            copy_tree(&e.path(), &dst.join(e.file_name()), on_file)?;
        }
    } else if meta.is_file() {
        copy_file(src, dst, meta.modified().ok())?;
        on_file(1);
    }
    Ok(())
}

/// Копия файла с тем же временем изменения (по нему следующий снимок узнаёт неизменённые файлы).
fn copy_file(src: &Path, dst: &Path, mtime: Option<SystemTime>) -> Result<()> {
    std::fs::copy(src, dst).with_context(|| format!("{} → {}", src.display(), dst.display()))?;
    if let Some(t) = mtime
        && let Ok(f) = std::fs::File::options().write(true).open(dst)
    {
        let _ = f.set_modified(t);
    }
    Ok(())
}

/// (файлов, байт) в дереве.
fn tree_size(p: &Path) -> (u64, u64) {
    let Ok(m) = std::fs::symlink_metadata(p) else { return (0, 0) };
    if m.is_file() {
        return (1, m.len());
    }
    if !m.is_dir() {
        return (0, 0);
    }
    let mut acc = (0, 0);
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let (f, b) = tree_size(&e.path());
            acc.0 += f;
            acc.1 += b;
        }
    }
    acc
}

/// Пакует `paths` (относительно `root`) в `dest` (.tar.gz). Пишет во временный файл и переименовывает.
pub fn create_archive(root: &Path, paths: &[String], dest: &Path, mut progress: impl FnMut(u64)) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            tr!(format!("не создать {}", parent.display()), format!("cannot create {}", parent.display()))
        })?;
    }
    let part = dest.with_extension("gz.part");
    let file = std::fs::File::create(&part)
        .with_context(|| tr!(format!("не создать {}", part.display()), format!("cannot create {}", part.display())))?;
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    tar.follow_symlinks(false);
    let mut count = 0u64;
    let res = (|| {
        for p in paths {
            let src = root.join(p);
            if !src.exists() {
                anyhow::bail!(
                    "{}",
                    tr!(format!("нет пути {}", src.display()), format!("path not found: {}", src.display()))
                );
            }
            add_tree(&mut tar, &src, Path::new(p), &mut count, &mut progress)?;
        }
        tar.into_inner()?.finish()?;
        Ok(())
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest)?;
    Ok(std::fs::metadata(dest)?.len())
}

fn add_tree<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    name: &Path,
    count: &mut u64,
    progress: &mut impl FnMut(u64),
) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        tar.append_dir(name, src)?;
        let mut entries: Vec<_> = std::fs::read_dir(src)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            add_tree(tar, &e.path(), &name.join(e.file_name()), count, progress)?;
        }
    } else if meta.is_file() {
        if src.file_name().is_some_and(|n| n == "session.lock") {
            return Ok(());
        }
        tar.append_path_with_name(src, name)
            .with_context(|| tr!(format!("файл {}", src.display()), format!("file {}", src.display())))?;
        *count += 1;
        if (*count).is_multiple_of(200) {
            progress(*count);
        }
    }
    Ok(())
}

// ───────────────────────────── снимки ─────────────────────────────

/// Состояние обхода при создании снимка.
struct Mirror<'a> {
    st: SnapshotStats,
    links_ok: bool,
    /// Папки, в которые не заходим (сама папка бэкапов, недописанный снимок).
    exclude: &'a [PathBuf],
    progress: &'a mut dyn FnMut(u64),
}

impl Mirror<'_> {
    /// `src` → `dst`; `prev` — тот же путь в предыдущем снимке.
    fn tree(&mut self, src: &Path, dst: &Path, prev: Option<&Path>) -> Result<()> {
        let meta = std::fs::symlink_metadata(src).with_context(|| src.display().to_string())?;
        if meta.is_dir() {
            if self.exclude.iter().any(|e| e == src) {
                return Ok(());
            }
            std::fs::create_dir_all(dst).with_context(|| dst.display().to_string())?;
            let mut entries: Vec<_> = std::fs::read_dir(src)?.collect::<Result<_, _>>()?;
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let n = e.file_name();
                let p = prev.map(|p| p.join(&n));
                self.tree(&src.join(&n), &dst.join(&n), p.as_deref())?;
            }
        } else if meta.is_file() {
            if src.file_name().is_some_and(|n| n == "session.lock") {
                return Ok(());
            }
            self.file(src, &meta, dst, prev)?;
            self.st.meta.files += 1;
            if self.st.meta.files.is_multiple_of(200) {
                (self.progress)(self.st.meta.files);
            }
        }
        Ok(())
    }

    fn file(&mut self, src: &Path, meta: &std::fs::Metadata, dst: &Path, prev: Option<&Path>) -> Result<()> {
        let len = meta.len();
        self.st.meta.total_bytes += len;
        if self.links_ok
            && let Some(p) = prev
            && unchanged(src, meta, p)
        {
            match std::fs::hard_link(p, dst) {
                Ok(()) => {
                    self.st.linked += 1;
                    return Ok(());
                }
                Err(e) => {
                    self.links_ok = false;
                    self.st.link_error = Some(e.to_string());
                }
            }
        }
        copy_file(src, dst, meta.modified().ok())?;
        self.st.meta.new_bytes += len;
        Ok(())
    }
}

/// Файл не менялся с предыдущего снимка: тот же размер и время изменения (маленькие — ещё и побайтно).
fn unchanged(src: &Path, meta: &std::fs::Metadata, prev: &Path) -> bool {
    let Ok(pm) = std::fs::symlink_metadata(prev) else { return false };
    let (Ok(a), Ok(b)) = (meta.modified(), pm.modified()) else { return false };
    if !pm.is_file() || pm.len() != meta.len() || a != b {
        return false;
    }
    meta.len() > SMALL_FILE || matches!((std::fs::read(src), std::fs::read(prev)), (Ok(x), Ok(y)) if x == y)
}

/// Пишет снимок в `<dest>.partial/` и переименовывает в `dest` только при успехе.
fn build_snapshot(
    dest: &Path,
    exclude: &[PathBuf],
    progress: &mut dyn FnMut(u64),
    created: i64,
    fill: impl FnOnce(&mut Mirror, &Path) -> Result<()>,
) -> Result<SnapshotStats> {
    if dest.exists() {
        bail!("{}", tr!(format!("уже существует: {}", dest.display()), format!("already exists: {}", dest.display())));
    }
    let partial = partial_path(dest);
    if partial.exists() {
        std::fs::remove_dir_all(&partial).with_context(|| partial.display().to_string())?;
    }
    std::fs::create_dir_all(&partial).with_context(|| {
        tr!(format!("не создать {}", partial.display()), format!("cannot create {}", partial.display()))
    })?;
    let mut excl = exclude.to_vec();
    excl.push(partial.clone());
    let mut m = Mirror { st: SnapshotStats::default(), links_ok: true, exclude: &excl, progress };
    let res = fill(&mut m, &partial).and_then(|()| {
        m.st.meta.created = created;
        let json = serde_json::to_vec_pretty(&m.st.meta)?;
        std::fs::write(partial.join(SNAPSHOT_META), json)?;
        std::fs::rename(&partial, dest).with_context(|| dest.display().to_string())?;
        Ok(())
    });
    if let Err(e) = res {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e);
    }
    Ok(m.st)
}

/// Снимок `paths` (относительно `root`) в папку `dest` (`….snap`). `prev` — предыдущий снимок, с которым
/// неизменённые файлы связываются жёсткими ссылками; `exclude` — папки, в которые не заходить.
pub fn create_snapshot(
    root: &Path,
    paths: &[String],
    dest: &Path,
    prev: Option<&Path>,
    exclude: &[PathBuf],
    mut progress: impl FnMut(u64),
) -> Result<SnapshotStats> {
    for p in paths {
        let pp = Path::new(p);
        if pp.is_absolute() || pp.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!(
                "{}",
                tr!(
                    format!("путь бэкапа должен быть внутри папки сервера: {p}"),
                    format!("backup path must be inside the server folder: {p}")
                )
            );
        }
    }
    build_snapshot(dest, exclude, &mut progress, chrono::Local::now().timestamp(), |m, partial| {
        for p in paths {
            let src = root.join(p);
            if !src.exists() {
                bail!("{}", tr!(format!("нет пути {}", src.display()), format!("path not found: {}", src.display())));
            }
            let pv = prev.map(|d| d.join(p));
            m.tree(&src, &partial.join(p), pv.as_deref())?;
        }
        Ok(())
    })
}

/// Копия готового снимка `src` в `dest` (например, на другой диск) — тоже с жёсткими ссылками на `prev`
/// (предыдущий снимок в той же папке назначения).
pub fn copy_snapshot(
    src: &Path,
    dest: &Path,
    prev: Option<&Path>,
    mut progress: impl FnMut(u64),
) -> Result<SnapshotStats> {
    let created = read_meta(src).map(|m| m.created).unwrap_or_else(|| chrono::Local::now().timestamp());
    build_snapshot(dest, &[], &mut progress, created, |m, partial| {
        for n in top_entries(src)? {
            let pv = prev.map(|d| d.join(&n));
            m.tree(&src.join(&n), &partial.join(&n), pv.as_deref())?;
        }
        Ok(())
    })
}

pub fn read_meta(snap: &Path) -> Option<SnapshotMeta> {
    serde_json::from_slice(&std::fs::read(snap.join(SNAPSHOT_META)).ok()?).ok()
}

/// Самый свежий снимок этого сервера в папке.
pub fn latest_snapshot(dir: &Path, prefix: &str) -> Option<PathBuf> {
    list(dir, prefix).into_iter().find(|b| b.kind == BackupKind::Incremental).map(|b| b.path)
}

// ───────────────────────────── список, удаление, ротация ─────────────────────────────

/// Бэкапы сервера в папке (архивы и снимки), новые первыми.
pub fn list(dir: &Path, prefix: &str) -> Vec<BackupInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut v: Vec<BackupInfo> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let kind = backup_kind(name.to_str()?, prefix)?;
            info(e.path(), kind)
        })
        .collect();
    v.sort_by(|a, b| b.path.file_name().cmp(&a.path.file_name()));
    v
}

fn info(path: PathBuf, kind: BackupKind) -> Option<BackupInfo> {
    let m = std::fs::symlink_metadata(&path).ok()?;
    match kind {
        BackupKind::Full => m.is_file().then(|| BackupInfo {
            size: m.len(),
            total_size: m.len(),
            files: None,
            modified: m.modified().ok(),
            path,
            kind,
        }),
        BackupKind::Incremental if m.is_dir() => {
            let (files, total, new, modified) = match read_meta(&path) {
                Some(s) => {
                    let t = (s.created > 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(s.created as u64));
                    (s.files, s.total_bytes, s.new_bytes, t.or(m.modified().ok()))
                }
                // Снимок без сводки (скопирован вручную) — считаем как есть.
                None => {
                    let (f, b) = tree_size(&path);
                    (f, b, b, m.modified().ok())
                }
            };
            Some(BackupInfo { path, kind, size: new, total_size: total, files: Some(files), modified })
        }
        BackupKind::Incremental => None,
    }
}

/// Удаляет один бэкап (для кнопки «удалить» в окне).
pub fn delete(info: &BackupInfo) -> Result<()> {
    delete_path(&info.path)
}

/// Удаляет архив `….tar.gz` или папку-снимок `….snap`; другие пути не трогает. Снимок сначала
/// переименовывается в `….snap.partial`, чтобы прерванное удаление не оставило «целый» с виду снимок.
/// Удалять снимок безопасно: файлы, общие с другими снимками (жёсткие ссылки), в них остаются.
pub fn delete_path(path: &Path) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let m = std::fs::symlink_metadata(path).with_context(|| path.display().to_string())?;
    if name.ends_with(SNAP_EXT) && m.is_dir() {
        let trash = partial_path(path);
        if trash.exists() {
            std::fs::remove_dir_all(&trash).with_context(|| trash.display().to_string())?;
        }
        std::fs::rename(path, &trash).with_context(|| path.display().to_string())?;
        std::fs::remove_dir_all(&trash).with_context(|| trash.display().to_string())?;
    } else if name.ends_with(ARCHIVE_EXT) && m.is_file() {
        std::fs::remove_file(path).with_context(|| path.display().to_string())?;
    } else {
        bail!("{}", tr!(format!("это не бэкап: {}", path.display()), format!("not a backup: {}", path.display())));
    }
    Ok(())
}

/// Удаляет недописанные бэкапы этого сервера (остатки прерванных бэкапов и удалений).
pub fn cleanup_partials(dir: &Path, prefix: &str) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if !is_partial_name(n, prefix) {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        if m.is_dir() {
            let _ = std::fs::remove_dir_all(e.path());
        } else {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Оставляет `keep` последних бэкапов сервера (архивы и снимки вместе); 0 — хранить все.
pub fn rotate(dir: &Path, prefix: &str, keep: usize) -> Result<()> {
    cleanup_partials(dir, prefix);
    if keep == 0 {
        return Ok(());
    }
    for old in list(dir, prefix).into_iter().skip(keep) {
        delete(&old)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pechka-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Мир: level.dat, session.lock, большой регион и маленький файл.
    pub(crate) fn make_world(root: &Path) {
        std::fs::create_dir_all(root.join("world/region")).unwrap();
        std::fs::write(root.join("world/level.dat"), b"level-1").unwrap();
        std::fs::write(root.join("world/session.lock"), b"lock").unwrap();
        std::fs::write(root.join("world/region/r.0.0.mca"), vec![1u8; 300_000]).unwrap();
        std::fs::write(root.join("world/region/r.0.1.mca"), vec![2u8; 300_000]).unwrap();
    }

    #[cfg(unix)]
    pub(crate) fn ino(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).unwrap().ino()
    }

    #[test]
    fn archive_skips_session_lock_and_rotates() {
        let root = tmpdir("bk");
        std::fs::create_dir_all(root.join("world/region")).unwrap();
        std::fs::write(root.join("world/level.dat"), b"x").unwrap();
        std::fs::write(root.join("world/session.lock"), b"lock").unwrap();
        std::fs::write(root.join("world/region/r.0.0.mca"), vec![1u8; 5000]).unwrap();
        let out = root.join("backups");
        for i in 0..3 {
            let dest = out.join(format!("srv-2026010{i}-000000.tar.gz"));
            create_archive(&root, &["world".into()], &dest, |_| {}).unwrap();
        }
        let dest = out.join("srv-20260101-000000.tar.gz");
        let f = std::fs::File::open(&dest).unwrap();
        let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
        let names: Vec<String> =
            a.entries().unwrap().map(|e| e.unwrap().path().unwrap().display().to_string()).collect();
        assert!(names.iter().any(|n| n == "world/level.dat"));
        assert!(names.iter().any(|n| n == "world/region/r.0.0.mca"));
        assert!(!names.iter().any(|n| n.contains("session.lock")));
        rotate(&out, "srv", 2).unwrap();
        let left: Vec<_> =
            list(&out, "srv").iter().map(|b| b.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(left, vec!["srv-20260102-000000.tar.gz", "srv-20260101-000000.tar.gz"]);
        assert!(list(&out, "srv").iter().all(|b| b.kind == BackupKind::Full));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prefix_is_sanitized() {
        assert_eq!(file_prefix("My Server/1"), "My_Server_1");
    }

    #[test]
    fn name_pattern_is_strict() {
        assert_eq!(backup_kind("srv-20260101-000000.tar.gz", "srv"), Some(BackupKind::Full));
        assert_eq!(backup_kind("srv-20260101-000000.snap", "srv"), Some(BackupKind::Incremental));
        assert_eq!(backup_kind("srv-20260101-000000.snap.partial", "srv"), None);
        assert_eq!(backup_kind("srv-20260101-000000.tar.gz.part", "srv"), None);
        // другой сервер с похожим именем
        assert_eq!(backup_kind("srv-2-20260101-000000.tar.gz", "srv"), None);
        assert_eq!(backup_kind("srv-2-20260101-000000.tar.gz", "srv-2"), Some(BackupKind::Full));
        assert_eq!(backup_kind("srv-notes.tar.gz", "srv"), None);
        assert_eq!(backup_kind("srv-2026010a-000000.snap", "srv"), None);
        assert!(is_partial_name("srv-20260101-000000.snap.partial", "srv"));
        assert!(is_partial_name("srv-20260101-000000.tar.gz.part", "srv"));
        assert!(!is_partial_name("srv-x.snap.partial", "srv"));
    }

    #[test]
    fn restore_moves_current_world_aside() {
        let root = std::env::temp_dir().join(format!("pechka-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("world/region")).unwrap();
        std::fs::write(root.join("world/level.dat"), b"old-good").unwrap();
        std::fs::write(root.join("world/region/r.0.0.mca"), b"chunk").unwrap();
        let arc = root.join("backups/b.tar.gz");
        create_archive(&root, &["world".into()], &arc, |_| {}).unwrap();
        // мир «испортили»
        std::fs::write(root.join("world/level.dat"), b"broken").unwrap();
        std::fs::write(root.join("world/new.txt"), b"junk").unwrap();
        let stash = restore(&root, &arc, &["world".into()], |_| {}).unwrap();
        assert_eq!(std::fs::read(root.join("world/level.dat")).unwrap(), b"old-good");
        assert!(root.join("world/region/r.0.0.mca").exists());
        assert!(!root.join("world/new.txt").exists());
        assert_eq!(std::fs::read(stash.join("world/level.dat")).unwrap(), b"broken");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn incremental_snapshot_links_unchanged_files() {
        let root = tmpdir("snap");
        make_world(&root);
        let out = root.join("backups");
        let s1 = out.join("srv-20260101-000000.snap");
        let st1 = create_snapshot(&root, &["world".into()], &s1, None, std::slice::from_ref(&out), |_| {}).unwrap();
        assert_eq!(st1.meta.files, 3, "session.lock не входит");
        assert_eq!(st1.meta.new_bytes, st1.meta.total_bytes);
        assert_eq!(st1.linked, 0);
        assert!(!s1.join("world/session.lock").exists());
        assert_eq!(read_meta(&s1).unwrap(), st1.meta);

        // меняем один регион (тот же размер, другое содержимое и время) и level.dat
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(root.join("world/region/r.0.1.mca"), vec![3u8; 300_000]).unwrap();
        std::fs::write(root.join("world/level.dat"), b"level-2").unwrap();
        let s2 = out.join("srv-20260101-010000.snap");
        let prev = latest_snapshot(&out, "srv").unwrap();
        assert_eq!(prev, s1);
        let st2 =
            create_snapshot(&root, &["world".into()], &s2, Some(&prev), std::slice::from_ref(&out), |_| {}).unwrap();
        assert!(st2.link_error.is_none());
        assert_eq!(st2.meta.files, 3);
        assert_eq!(st2.linked, 1);
        assert_eq!(st2.meta.new_bytes, 300_000 + 7);
        assert_eq!(std::fs::read(s2.join("world/region/r.0.1.mca")).unwrap(), vec![3u8; 300_000]);
        assert_eq!(std::fs::read(s1.join("world/region/r.0.1.mca")).unwrap(), vec![2u8; 300_000]);
        assert_eq!(std::fs::read(s2.join("world/level.dat")).unwrap(), b"level-2");
        #[cfg(unix)]
        {
            assert_eq!(ino(&s1.join("world/region/r.0.0.mca")), ino(&s2.join("world/region/r.0.0.mca")));
            assert_ne!(ino(&s1.join("world/region/r.0.1.mca")), ino(&s2.join("world/region/r.0.1.mca")));
            assert_ne!(ino(&s1.join("world/level.dat")), ino(&s2.join("world/level.dat")));
        }

        let l = list(&out, "srv");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].path, s2);
        assert_eq!(l[0].kind, BackupKind::Incremental);
        assert_eq!(l[0].size, 300_007);
        assert_eq!(l[0].total_size, 600_007);
        assert_eq!(l[0].files, Some(3));

        // backup_paths = ["."]: папка бэкапов внутри — не заходим в неё
        let s3 = out.join("srv-20260101-020000.snap");
        create_snapshot(&root, &[".".into()], &s3, None, std::slice::from_ref(&out), |_| {}).unwrap();
        assert!(s3.join("world/level.dat").exists());
        assert!(!s3.join("backups").exists());
        assert!(
            create_snapshot(&root, &["../x".into()], &out.join("srv-20260101-030000.snap"), None, &[], |_| {}).is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn partial_snapshot_is_never_listed() {
        let root = tmpdir("partial");
        make_world(&root);
        let out = root.join("backups");
        // прерванный снимок с виду почти целый
        let stale = out.join("srv-20260101-000000.snap.partial");
        std::fs::create_dir_all(stale.join("world")).unwrap();
        std::fs::write(stale.join(SNAPSHOT_META), b"{}").unwrap();
        std::fs::write(out.join("srv-20260101-000000.tar.gz.part"), b"half").unwrap();
        assert!(list(&out, "srv").is_empty());
        assert!(latest_snapshot(&out, "srv").is_none());
        // неудачный снимок не оставляет ничего
        let bad = out.join("srv-20260102-000000.snap");
        assert!(create_snapshot(&root, &["world".into(), "missing".into()], &bad, None, &[], |_| {}).is_err());
        assert!(!bad.exists());
        assert!(!partial_path(&bad).exists());
        assert!(list(&out, "srv").is_empty());
        // ротация чистит остатки
        rotate(&out, "srv", 5).unwrap();
        assert!(!stale.exists());
        assert!(!out.join("srv-20260101-000000.tar.gz.part").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_from_snapshot() {
        let root = tmpdir("snaprestore");
        make_world(&root);
        let out = root.join("backups");
        let s1 = out.join("srv-20260101-000000.snap");
        create_snapshot(&root, &["world".into()], &s1, None, &[], |_| {}).unwrap();
        std::fs::write(root.join("world/level.dat"), b"broken").unwrap();
        std::fs::write(root.join("world/new.txt"), b"junk").unwrap();
        let stash = restore(&root, &s1, &["world".into()], |_| {}).unwrap();
        assert_eq!(std::fs::read(root.join("world/level.dat")).unwrap(), b"level-1");
        assert_eq!(std::fs::read(root.join("world/region/r.0.0.mca")).unwrap(), vec![1u8; 300_000]);
        assert!(!root.join("world/new.txt").exists());
        assert!(!root.join(SNAPSHOT_META).exists(), "служебный файл снимка не попадает в мир");
        assert_eq!(std::fs::read(stash.join("world/level.dat")).unwrap(), b"broken");
        #[cfg(unix)]
        assert_ne!(ino(&s1.join("world/level.dat")), ino(&root.join("world/level.dat")), "копия, а не ссылка");
        // недописанный снимок восстановить нельзя
        let p = partial_path(&out.join("srv-20260102-000000.snap"));
        std::fs::create_dir_all(&p).unwrap();
        assert!(restore(&root, &p, &["world".into()], |_| {}).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn run_incremental_with_copy_targets() {
        let root = tmpdir("run");
        make_world(&root);
        let disk = root.join("disk");
        std::fs::create_dir_all(&disk).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let job = BackupJob {
            id: 7,
            name: "My srv".into(),
            root: root.clone(),
            paths: vec!["world".into()],
            backup_dir: root.join("backups"),
            keep: 3,
            mode: BackupMode::Incremental,
            copy_to: vec![disk.display().to_string(), "rclone:gd:mc".into(), String::new()],
            live: None,
            save_off_mark: None,
        };
        run(job, tx).await;
        let mut done = None;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Backup { id: 7, ev: BackupEvent::Done { path, bytes, warning } } = ev {
                done = Some((path, bytes, warning));
            }
        }
        let (path, bytes, warning) = done.expect("Done");
        assert!(path.is_dir());
        assert_eq!(backup_kind(path.file_name().unwrap().to_str().unwrap(), "My_srv"), Some(BackupKind::Incremental));
        assert_eq!(bytes, 600_007);
        assert!(disk.join(path.file_name().unwrap()).join("world/level.dat").exists());
        // rclone, скорее всего, не установлен в тестовом окружении; если установлен — удалённого «gd» нет.
        let w = warning.expect("rclone target must warn");
        assert!(w.contains("rclone"), "{w}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Пока идёт бэкап запущенного сервера, лежит отметка `save-off`; после `save-on` её нет.
    #[tokio::test]
    async fn run_live_marks_save_off_until_save_on() {
        let root = tmpdir("run-live");
        make_world(&root);
        let mark = root.join("run/srv.save-off");
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let (lines_tx, lines_rx) = broadcast::channel::<String>(16);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let job = BackupJob {
            id: 1,
            name: "srv".into(),
            root: root.clone(),
            paths: vec!["world".into()],
            backup_dir: root.join("backups"),
            keep: 3,
            mode: BackupMode::Full,
            copy_to: vec![],
            live: Some((stdin_tx, lines_rx)),
            save_off_mark: Some(mark.clone()),
        };
        let task = tokio::spawn(run(job, tx));
        assert_eq!(stdin_rx.recv().await.as_deref(), Some("save-off"));
        assert!(mark.exists(), "the mark must be written before save-off");
        assert_eq!(stdin_rx.recv().await.as_deref(), Some("save-all flush"));
        lines_tx.send("[12:00:00] [Server thread/INFO]: Saved the game".into()).unwrap();
        task.await.unwrap();
        assert_eq!(stdin_rx.recv().await.as_deref(), Some("save-on"));
        assert!(!mark.exists(), "the mark must be gone after save-on");
        assert!(!take_save_off_mark(&mark));
        let mut done = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Backup { id: 1, ev: BackupEvent::Done { warning, .. } } = ev {
                assert_eq!(warning, None);
                done = true;
            }
        }
        assert!(done);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_off_mark_is_taken_once() {
        let root = tmpdir("mark");
        let mark = root.join("run/x.save-off");
        assert!(!take_save_off_mark(&mark));
        put_save_off_mark(&mark);
        assert!(take_save_off_mark(&mark));
        assert!(!take_save_off_mark(&mark));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rotate_and_delete_mixed_kinds() {
        let root = tmpdir("mixed");
        make_world(&root);
        let out = root.join("backups");
        let w = ["world".to_string()];
        create_archive(&root, &w, &out.join("srv-20260101-000000.tar.gz"), |_| {}).unwrap();
        create_snapshot(&root, &w, &out.join("srv-20260102-000000.snap"), None, &[], |_| {}).unwrap();
        create_archive(&root, &w, &out.join("srv-20260103-000000.tar.gz"), |_| {}).unwrap();
        let prev = latest_snapshot(&out, "srv");
        create_snapshot(&root, &w, &out.join("srv-20260104-000000.snap"), prev.as_deref(), &[], |_| {}).unwrap();
        std::fs::write(out.join("other-20260101-000000.tar.gz"), b"foreign").unwrap();
        std::fs::write(out.join("srv-notes.txt"), b"mine").unwrap();
        let kinds: Vec<_> = list(&out, "srv").iter().map(|b| b.kind).collect();
        use BackupKind::*;
        assert_eq!(kinds, vec![Incremental, Full, Incremental, Full]);
        rotate(&out, "srv", 2).unwrap();
        let names: Vec<_> =
            list(&out, "srv").iter().map(|b| b.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["srv-20260104-000000.snap", "srv-20260103-000000.tar.gz"]);
        // файлы снимка, связанные с удалённым, на месте
        assert_eq!(std::fs::read(out.join("srv-20260104-000000.snap/world/level.dat")).unwrap(), b"level-1");
        assert!(out.join("other-20260101-000000.tar.gz").exists());
        assert!(out.join("srv-notes.txt").exists());
        for b in list(&out, "srv") {
            delete(&b).unwrap();
        }
        assert!(list(&out, "srv").is_empty());
        assert!(!partial_path(&out.join("srv-20260104-000000.snap")).exists());
        assert!(delete_path(&out.join("srv-notes.txt")).is_err());
        assert!(out.join("srv-notes.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
