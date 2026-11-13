//! Бэкапы: save-off → save-all flush → tar.gz → save-on, ротация.

use crate::event::{AppEvent, BackupEvent};
use crate::tr;
use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::{broadcast, mpsc};

const SAVE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct BackupJob {
    pub id: usize,
    pub name: String,
    pub root: PathBuf,
    pub paths: Vec<String>,
    pub backup_dir: PathBuf,
    pub keep: usize,
    /// stdin и поток строк запущенного сервера; None — сервер остановлен.
    pub live: Option<(mpsc::UnboundedSender<String>, broadcast::Receiver<String>)>,
}

#[derive(Debug, Clone)]
pub struct BackupInfo {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

pub fn file_prefix(name: &str) -> String {
    name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

pub async fn run(job: BackupJob, tx: mpsc::UnboundedSender<AppEvent>) {
    let id = job.id;
    let progress = |s: String| {
        let _ = tx.send(AppEvent::Backup { id, ev: BackupEvent::Progress(s) });
    };
    let mut warning = None;
    let live = job.live;
    if let Some((stdin, mut lines)) = live.as_ref().map(|(s, l)| (s.clone(), l.resubscribe())) {
        progress("save-off, save-all flush…".into());
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
            warning = Some(
                tr!(
                    "сервер не подтвердил сохранение (Saved the game) — архив может быть неполным",
                    "the server did not confirm the save (Saved the game) — the archive may be incomplete"
                )
                .to_string(),
            );
        }
    }
    progress(tr!("архивирование…", "archiving…").into());
    let tx2 = tx.clone();
    let (root, paths, dir, prefix, keep) = (job.root, job.paths, job.backup_dir, file_prefix(&job.name), job.keep);
    let result = tokio::task::spawn_blocking(move || {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dest = dir.join(format!("{prefix}-{stamp}.tar.gz"));
        let bytes = create_archive(&root, &paths, &dest, |n| {
            let _ = tx2.send(AppEvent::Backup {
                id,
                ev: BackupEvent::Progress(tr!(format!("архивирование… {n} файлов"), format!("archiving… {n} files"))),
            });
        })?;
        rotate(&dir, &prefix, keep)?;
        Ok::<_, anyhow::Error>((dest, bytes))
    })
    .await;
    if let Some((stdin, _)) = &live {
        let _ = stdin.send("save-on".into());
    }
    let ev = match result {
        Ok(Ok((path, bytes))) => BackupEvent::Done { path, bytes, warning },
        Ok(Err(e)) => BackupEvent::Failed(format!("{e:#}")),
        Err(e) => BackupEvent::Failed(e.to_string()),
    };
    let _ = tx.send(AppEvent::Backup { id, ev });
}

/// Сколько копий «мира до восстановления» хранить в `.mctui-before-restore/`.
const KEEP_BEFORE_RESTORE: usize = 3;

/// Восстановление из архива (сервер должен быть остановлен). Текущие `paths` не удаляются, а
/// переносятся в `<root>/.mctui-before-restore/<время>/` (хранятся последние 3). Возвращает эту папку.
pub fn restore(root: &Path, archive: &Path, paths: &[String], mut progress: impl FnMut(String)) -> Result<PathBuf> {
    // Проверяем архив до того, как что-то трогать.
    progress(crate::tr!("проверка архива…", "checking archive…").into());
    let open = || -> Result<tar::Archive<flate2::read::GzDecoder<std::fs::File>>> {
        let f = std::fs::File::open(archive).with_context(|| archive.display().to_string())?;
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
    let stash_root = root.join(".mctui-before-restore");
    let stash = stash_root.join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
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
    let mut done = 0u64;
    for e in open()?.entries()? {
        let mut e = e?;
        e.unpack_in(root)?;
        done += 1;
        if done.is_multiple_of(200) {
            progress(crate::tr!(format!("распаковка… {done}/{total}"), format!("extracting… {done}/{total}")));
        }
    }
    // Ротация старых копий.
    if let Ok(rd) = std::fs::read_dir(&stash_root) {
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        while dirs.len() > KEEP_BEFORE_RESTORE {
            let _ = std::fs::remove_dir_all(dirs.remove(0));
        }
    }
    Ok(stash)
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

pub fn list(dir: &Path, prefix: &str) -> Vec<BackupInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut v: Vec<BackupInfo> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&format!("{prefix}-")) && n.ends_with(".tar.gz")
        })
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some(BackupInfo { path: e.path(), size: m.len(), modified: m.modified().ok() })
        })
        .collect();
    v.sort_by(|a, b| b.path.cmp(&a.path));
    v
}

pub fn rotate(dir: &Path, prefix: &str, keep: usize) -> Result<()> {
    if keep == 0 {
        return Ok(());
    }
    for old in list(dir, prefix).into_iter().skip(keep) {
        std::fs::remove_file(&old.path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mctui-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
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
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prefix_is_sanitized() {
        assert_eq!(file_prefix("My Server/1"), "My_Server_1");
    }

    #[test]
    fn restore_moves_current_world_aside() {
        let root = std::env::temp_dir().join(format!("mctui-restore-{}", std::process::id()));
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
}
