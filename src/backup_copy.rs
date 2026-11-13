//! Копирование готового бэкапа в другие места (`backup_copy_to`):
//! - путь к папке — другой диск, NAS, синхронизируемая папка Dropbox / iCloud / Яндекс.Диска;
//!   снимки там тоже складываются с жёсткими ссылками на предыдущую копию;
//! - `rclone:<remote>:<path>` — любое облако через [rclone](https://rclone.org) (нужен `rclone` в PATH).
//!
//! В каждом месте хранятся последние `backup_keep` бэкапов этого сервера. Удаляется только то, что
//! строго совпадает с `<prefix>-YYYYmmdd-HHMMSS.tar.gz` / `.snap` (и недописанные копии этого сервера) —
//! чужие файлы в папке не трогаются. Ошибки копирования не делают бэкап неудачным: они возвращаются
//! как предупреждения.

use crate::backup::{self, BackupKind};
use crate::tr;
use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyTarget {
    /// Папка (абсолютный путь; `~` раскрыт, относительный — от папки сервера).
    Folder(PathBuf),
    /// `<remote>:<path>` для rclone.
    Rclone(String),
}

/// Где искать программы (подменяется в тестах).
#[derive(Debug, Clone, Default)]
pub struct CopyEnv {
    pub path_var: Option<OsString>,
    /// Дополнительные папки поиска: служба при входе в систему часто получает урезанный PATH
    /// без /opt/homebrew/bin.
    pub extra_dirs: Vec<PathBuf>,
}

impl CopyEnv {
    pub fn system() -> Self {
        let mut extra = vec![];
        if cfg!(unix) {
            extra.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from));
            if let Some(h) = dirs::home_dir() {
                extra.push(h.join(".local/bin"));
                extra.push(h.join("bin"));
            }
        }
        Self { path_var: std::env::var_os("PATH"), extra_dirs: extra }
    }
}

/// Строка из конфига → цель. Пустая строка — None.
pub fn parse_target(s: &str, root: &Path) -> Option<CopyTarget> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(r) = t.strip_prefix("rclone:") {
        let r = r.trim();
        return (!r.is_empty()).then(|| CopyTarget::Rclone(r.to_string()));
    }
    let p = if t == "~" || t.starts_with("~/") || t.starts_with("~\\") {
        match dirs::home_dir() {
            Some(h) => h.join(t.trim_start_matches('~').trim_start_matches(['/', '\\'])),
            None => PathBuf::from(t),
        }
    } else {
        PathBuf::from(t)
    };
    Some(CopyTarget::Folder(if p.is_absolute() { p } else { root.join(p) }))
}

/// Копирует бэкап `backup` во все цели. Возвращает предупреждения (пустой список — всё хорошо).
pub fn copy_all(
    root: &Path,
    targets: &[String],
    backup: &Path,
    prefix: &str,
    keep: usize,
    progress: impl FnMut(String),
) -> Vec<String> {
    copy_all_with(&CopyEnv::system(), root, targets, backup, prefix, keep, progress)
}

pub fn copy_all_with(
    env: &CopyEnv,
    root: &Path,
    targets: &[String],
    backup: &Path,
    prefix: &str,
    keep: usize,
    mut progress: impl FnMut(String),
) -> Vec<String> {
    let mut warnings = vec![];
    let mut rclone: Option<Option<PathBuf>> = None;
    for raw in targets {
        let Some(target) = parse_target(raw, root) else { continue };
        let label = raw.trim();
        progress(tr!(format!("копирование в {label}…"), format!("copying to {label}…")));
        let res = match &target {
            CopyTarget::Folder(dir) => copy_to_folder(backup, dir, prefix, keep, &mut |n| {
                progress(tr!(format!("копирование в {label}… {n} файлов"), format!("copying to {label}… {n} files")))
            }),
            CopyTarget::Rclone(remote) => {
                match rclone.get_or_insert_with(|| find_program("rclone", env.path_var.as_deref(), &env.extra_dirs)) {
                    Some(exe) => copy_to_rclone(exe, backup, remote, prefix, keep).map(|()| None),
                    None => Err(rclone_missing()),
                }
            }
        };
        match res {
            Ok(None) => {}
            Ok(Some(w)) => warnings.push(tr!(format!("копия в {label}: {w}"), format!("copy to {label}: {w}"))),
            Err(e) => {
                warnings.push(tr!(format!("не скопировано в {label}: {e:#}"), format!("not copied to {label}: {e:#}")))
            }
        }
    }
    warnings
}

pub fn rclone_missing() -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        tr!(
            "rclone не найден. Установите его: brew install rclone (macOS) или https://rclone.org/install/, \
             затем настройте облако: rclone config",
            "rclone was not found. Install it: brew install rclone (macOS) or https://rclone.org/install/, \
             then set up the cloud: rclone config"
        )
    )
}

/// Ищет программу в `path_var` и `extra` (на Windows — с `.exe`).
pub fn find_program(name: &str, path_var: Option<&OsStr>, extra: &[PathBuf]) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(extra.iter().cloned())
        .collect();
    dirs.into_iter().map(|d| d.join(&file)).find(|p| p.is_file())
}

/// Копия в папку. Ok(Some(_)) — скопировано, но с предупреждением.
pub fn copy_to_folder(
    backup: &Path,
    dir: &Path,
    prefix: &str,
    keep: usize,
    progress: &mut dyn FnMut(u64),
) -> Result<Option<String>> {
    if !dir.is_dir() {
        // Не создаём: если диск не подключён, папка «/Volumes/Disk/…» оказалась бы на системном диске.
        bail!(
            "{}",
            tr!(
                format!("папка {} не найдена (диск не подключён?) — создайте её", dir.display()),
                format!("folder {} not found (disk not connected?) — create it first", dir.display())
            )
        );
    }
    let src_dir = backup.parent().unwrap_or(Path::new("."));
    if let (Ok(a), Ok(b)) = (dir.canonicalize(), src_dir.canonicalize())
        && a == b
    {
        bail!("{}", tr!("это та же папка, что и backup_dir", "this is the same folder as backup_dir"));
    }
    let name = backup.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let Some(kind) = backup::backup_kind(name, prefix) else {
        bail!("{}", tr!(format!("это не бэкап: {name}"), format!("not a backup: {name}")));
    };
    let dest = dir.join(name);
    let mut warning = None;
    if !dest.exists() {
        match kind {
            BackupKind::Full => {
                let part = dir.join(format!("{name}.part"));
                std::fs::copy(backup, &part).with_context(|| part.display().to_string())?;
                if let Err(e) = std::fs::rename(&part, &dest) {
                    let _ = std::fs::remove_file(&part);
                    return Err(e).with_context(|| dest.display().to_string());
                }
            }
            BackupKind::Incremental => {
                let prev = backup::latest_snapshot(dir, prefix);
                let st = backup::copy_snapshot(backup, &dest, prev.as_deref(), progress)?;
                warning = st.link_error.map(|e| backup::link_warning(&e));
            }
        }
    }
    backup::rotate(dir, prefix, keep)?;
    Ok(warning)
}

/// `remote:` + `name` → `remote:name`, `remote:path` + `name` → `remote:path/name`.
fn rjoin(base: &str, name: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with(':') { format!("{b}{name}") } else { format!("{b}/{name}") }
}

fn rclone_cmd(exe: &Path, args: &[&OsStr]) -> Result<String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().with_context(|| exe.display().to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        let what = args.first().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
        bail!("rclone {what}: {} ({})", tail.join(" | "), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Копия через rclone. Снимок загружается в `<name>.partial`, затем переименовывается (`moveto`), чтобы
/// прерванная загрузка не выглядела готовым бэкапом. Жёстких ссылок в облаке нет: каждый снимок
/// загружается целиком.
pub fn copy_to_rclone(exe: &Path, backup: &Path, remote: &str, prefix: &str, keep: usize) -> Result<()> {
    let name = backup.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let Some(kind) = backup::backup_kind(name, prefix) else {
        bail!("{}", tr!(format!("это не бэкап: {name}"), format!("not a backup: {name}")));
    };
    let s = |x: &str| OsString::from(x);
    match kind {
        BackupKind::Full => {
            rclone_cmd(exe, &[OsStr::new("copyto"), backup.as_os_str(), &s(&rjoin(remote, name))])?;
        }
        BackupKind::Incremental => {
            let partial = rjoin(remote, &format!("{name}.partial"));
            rclone_cmd(exe, &[OsStr::new("copy"), backup.as_os_str(), &s(&partial)])?;
            rclone_cmd(exe, &[OsStr::new("moveto"), &s(&partial), &s(&rjoin(remote, name))])?;
        }
    }
    rotate_rclone(exe, remote, prefix, keep)
}

/// Что удалить в облаке: (имя, это папка). Только строгие имена этого сервера.
pub fn rclone_to_delete(lsf: &str, prefix: &str, keep: usize) -> Vec<(String, bool)> {
    let mut valid = vec![];
    let mut out = vec![];
    for line in lsf.lines() {
        let line = line.trim_end_matches('\r');
        let (n, is_dir) = match line.strip_suffix('/') {
            Some(n) => (n, true),
            None => (line, false),
        };
        if n.is_empty() || n.contains('/') {
            continue;
        }
        match backup::backup_kind(n, prefix) {
            Some(BackupKind::Incremental) if is_dir => valid.push(n.to_string()),
            Some(BackupKind::Full) if !is_dir => valid.push(n.to_string()),
            Some(_) => {}
            // недописанная загрузка снимка этого сервера
            None if is_dir
                && n.strip_suffix(".partial")
                    .is_some_and(|b| backup::backup_kind(b, prefix) == Some(BackupKind::Incremental)) =>
            {
                out.push((n.to_string(), true))
            }
            None => {}
        }
    }
    if keep > 0 {
        valid.sort_by(|a, b| b.cmp(a));
        for n in valid.into_iter().skip(keep) {
            let is_dir = n.ends_with(".snap");
            out.push((n, is_dir));
        }
    }
    out
}

fn rotate_rclone(exe: &Path, remote: &str, prefix: &str, keep: usize) -> Result<()> {
    let lsf = rclone_cmd(exe, &[OsStr::new("lsf"), OsStr::new("--max-depth"), OsStr::new("1"), OsStr::new(remote)])?;
    for (name, is_dir) in rclone_to_delete(&lsf, prefix, keep) {
        let path = OsString::from(rjoin(remote, &name));
        let op = if is_dir { "purge" } else { "deletefile" };
        rclone_cmd(exe, &[OsStr::new(op), &path])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::tests::{make_world, tmpdir};
    use crate::backup::{create_archive, create_snapshot, latest_snapshot, list};

    #[test]
    fn parses_targets() {
        let root = Path::new("/srv/mc");
        assert_eq!(parse_target("  ", root), None);
        assert_eq!(
            parse_target("rclone:yadisk:mc/backups", root),
            Some(CopyTarget::Rclone("yadisk:mc/backups".into()))
        );
        assert_eq!(parse_target("rclone:", root), None);
        assert_eq!(parse_target("copies", root), Some(CopyTarget::Folder(root.join("copies"))));
        let home = dirs::home_dir().unwrap();
        assert_eq!(parse_target("~/Dropbox/mc", root), Some(CopyTarget::Folder(home.join("Dropbox/mc"))));
        assert_eq!(rjoin("gd:", "a.tar.gz"), "gd:a.tar.gz");
        assert_eq!(rjoin("gd:mc/", "a.tar.gz"), "gd:mc/a.tar.gz");
    }

    #[test]
    fn folder_target_copies_links_and_rotates_safely() {
        let root = tmpdir("copy");
        make_world(&root);
        let out = root.join("backups");
        let target = root.join("other-disk");
        std::fs::create_dir_all(&target).unwrap();
        // чужие файлы в папке назначения
        let foreign = [
            "other-20260101-000000.tar.gz",
            "srv-2-20260101-000000.tar.gz",
            "srv-notes.txt",
            "srv-20260101-000000.tar.gz.bak",
            "photo.jpg",
        ];
        for f in foreign {
            std::fs::write(target.join(f), b"keep me").unwrap();
        }
        std::fs::create_dir_all(target.join("srv-old.snap")).unwrap();
        let targets = vec![target.display().to_string()];
        let env = CopyEnv::default();
        let w = ["world".to_string()];

        let a1 = out.join("srv-20260101-000000.tar.gz");
        create_archive(&root, &w, &a1, |_| {}).unwrap();
        let warn = copy_all_with(&env, &root, &targets, &a1, "srv", 2, |_| {});
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(std::fs::read(target.join("srv-20260101-000000.tar.gz")).unwrap(), std::fs::read(&a1).unwrap());

        let s1 = out.join("srv-20260102-000000.snap");
        create_snapshot(&root, &w, &s1, None, &[], |_| {}).unwrap();
        assert!(copy_all_with(&env, &root, &targets, &s1, "srv", 2, |_| {}).is_empty());
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("world/region/r.0.1.mca"), vec![9u8; 300_000]).unwrap();
        let s2 = out.join("srv-20260103-000000.snap");
        create_snapshot(&root, &w, &s2, latest_snapshot(&out, "srv").as_deref(), &[], |_| {}).unwrap();
        let mut msgs = vec![];
        assert!(copy_all_with(&env, &root, &targets, &s2, "srv", 2, |m| msgs.push(m)).is_empty());
        assert!(!msgs.is_empty());

        let t2 = target.join("srv-20260103-000000.snap");
        let t1 = target.join("srv-20260102-000000.snap");
        assert_eq!(std::fs::read(t2.join("world/region/r.0.1.mca")).unwrap(), vec![9u8; 300_000]);
        assert_eq!(std::fs::read(t1.join("world/region/r.0.1.mca")).unwrap(), vec![2u8; 300_000]);
        #[cfg(unix)]
        {
            use crate::backup::tests::ino;
            assert_eq!(ino(&t1.join("world/region/r.0.0.mca")), ino(&t2.join("world/region/r.0.0.mca")));
            assert_ne!(ino(&t1.join("world/region/r.0.1.mca")), ino(&t2.join("world/region/r.0.1.mca")));
            // копия на «другом диске» — не ссылка на исходный снимок
            assert_ne!(ino(&s2.join("world/region/r.0.0.mca")), ino(&t2.join("world/region/r.0.0.mca")));
        }
        let meta = crate::backup::read_meta(&t2).unwrap();
        assert_eq!(meta.files, 3);
        assert_eq!(meta.new_bytes, 300_000);

        // keep = 2: архив от 01.01 удалён, чужое на месте
        let names: Vec<_> =
            list(&target, "srv").iter().map(|b| b.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["srv-20260103-000000.snap", "srv-20260102-000000.snap"]);
        for f in foreign {
            assert_eq!(std::fs::read(target.join(f)).unwrap(), b"keep me", "{f}");
        }
        assert!(target.join("srv-old.snap").is_dir());

        // повторная копия того же бэкапа ничего не ломает; копия в саму папку бэкапов — ошибка
        assert!(copy_all_with(&env, &root, &targets, &s2, "srv", 2, |_| {}).is_empty());
        let same = copy_all_with(&env, &root, &["backups".into()], &s2, "srv", 2, |_| {});
        assert_eq!(same.len(), 1);
        assert!(out.join("srv-20260101-000000.tar.gz").exists());
        // папки нет — не создаём, предупреждение
        let missing = root.join("no-such-disk/mc");
        let warn = copy_all_with(&env, &root, &[missing.display().to_string()], &s2, "srv", 2, |_| {});
        assert_eq!(warn.len(), 1);
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rclone_missing_is_reported() {
        let root = tmpdir("rclone");
        make_world(&root);
        let a = root.join("backups/srv-20260101-000000.tar.gz");
        create_archive(&root, &["world".into()], &a, |_| {}).unwrap();
        let empty = root.join("empty-path");
        std::fs::create_dir_all(&empty).unwrap();
        let env = CopyEnv { path_var: Some(empty.into_os_string()), extra_dirs: vec![] };
        assert!(find_program("rclone", env.path_var.as_deref(), &[]).is_none());
        let warn = copy_all_with(&env, &root, &["rclone:gd:mc".into()], &a, "srv", 2, |_| {});
        assert_eq!(warn.len(), 1);
        assert!(warn[0].contains("brew install rclone"), "{}", warn[0]);
        assert!(warn[0].contains("rclone.org"), "{}", warn[0]);
        assert!(a.exists(), "сам бэкап остаётся");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rclone_rotation_only_touches_own_names() {
        let lsf = "srv-20260101-000000.tar.gz\nsrv-20260102-000000.snap/\nsrv-20260103-000000.snap/\n\
                   srv-20260104-000000.tar.gz\r\nsrv-20260105-000000.snap.partial/\nsrv-notes.txt\nother-20260101-000000.tar.gz\n\
                   srv-2-20260101-000000.tar.gz\nsrv-20260101-000000.snap\nsrv-20260101-000000.tar.gz/\nphotos/\n";
        let mut del = rclone_to_delete(lsf, "srv", 2);
        del.sort();
        assert_eq!(
            del,
            vec![
                ("srv-20260101-000000.tar.gz".to_string(), false),
                ("srv-20260102-000000.snap".to_string(), true),
                ("srv-20260105-000000.snap.partial".to_string(), true),
            ]
        );
        assert_eq!(rclone_to_delete(lsf, "srv", 0), vec![("srv-20260105-000000.snap.partial".to_string(), true)]);
    }
}
