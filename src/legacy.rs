//! Переход со старого имени: до 0.7 программа называлась `mctui`.
//!
//! - переменные окружения `MCTUI_*` по-прежнему читаются, если нет `PECHKA_*`;
//! - папка настроек `~/.config/mctui` переносится в `~/.config/pechka`, на старом месте
//!   остаётся ссылка (Windows: настройки остаются в `%APPDATA%\mctui`, пока там нет новой папки);
//! - файл программы `mctui` переименовывается в `pechka`, а `mctui` остаётся синонимом;
//! - автозапуск со старым именем снимает [`crate::service`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Прежнее имя программы.
pub const OLD_NAME: &str = "mctui";
/// Имя программы.
pub const NAME: &str = "pechka";

/// Переменная окружения `PECHKA_<name>`, а если её нет — прежняя `MCTUI_<name>`.
pub fn env_os(name: &str) -> Option<OsString> {
    std::env::var_os(format!("PECHKA_{name}")).or_else(|| std::env::var_os(format!("MCTUI_{name}")))
}

/// Как [`env_os`], но строкой.
pub fn env(name: &str) -> Option<String> {
    env_os(name).and_then(|v| v.into_string().ok())
}

/// Папка настроек внутри `base`: новая, а если её ещё нет, но есть старая — старая.
pub fn settings_dir(base: &Path) -> PathBuf {
    let new = base.join(NAME);
    let old = base.join(OLD_NAME);
    if !new.exists() && old.is_dir() { old } else { new }
}

/// Переносит `<base>/mctui` в `<base>/pechka` и оставляет на старом месте ссылку: служба
/// старой версии, которая ещё работает, пишет туда же. Только unix — на Windows папку держит
/// открытый лог службы, а ссылки там требуют прав; настройки остаются на старом месте.
/// Возвращает true, если папка перенесена.
pub fn migrate_settings(base: &Path) -> bool {
    #[cfg(unix)]
    {
        let new = base.join(NAME);
        let old = base.join(OLD_NAME);
        let old_is_real_dir = std::fs::symlink_metadata(&old).is_ok_and(|m| m.is_dir());
        if new.exists() || !old_is_real_dir {
            return false;
        }
        if std::fs::rename(&old, &new).is_err() {
            return false;
        }
        let _ = std::os::unix::fs::symlink(NAME, &old);
        true
    }
    #[cfg(not(unix))]
    {
        let _ = base;
        false
    }
}

/// Файл программы называется по-старому (`mctui`, `mctui.exe`)?
fn is_old_exe(exe: &Path) -> bool {
    exe.file_stem().is_some_and(|s| s == OLD_NAME)
}

/// Переименовывает установленный файл `mctui` в `pechka` рядом с ним и оставляет `mctui`
/// синонимом: ссылкой (unix) или `mctui.cmd` (Windows). Сборки из `target/`, Homebrew
/// и уже переименованные установки не трогает. Возвращает новый путь, если переименовал.
pub fn migrate_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    migrate_exe_at(&exe)
}

fn migrate_exe_at(exe: &Path) -> Option<PathBuf> {
    let s = exe.to_string_lossy();
    if !is_old_exe(exe) || s.contains("/target/") || s.contains("\\target\\") || s.contains("/Cellar/") {
        return None;
    }
    // Уже синоним (ссылка на pechka) — ничего не делаем.
    if std::fs::symlink_metadata(exe).ok()?.file_type().is_symlink() {
        return None;
    }
    let new = exe.with_file_name(format!("{NAME}{}", std::env::consts::EXE_SUFFIX));
    if new.exists() {
        return None;
    }
    // Работающий файл можно переименовать и на Windows.
    std::fs::rename(exe, &new).ok()?;
    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(&*new.file_name()?.to_string_lossy(), exe).is_err() {
            let _ = std::fs::rename(&new, exe);
            return None;
        }
    }
    #[cfg(windows)]
    {
        let shim = exe.with_file_name(format!("{OLD_NAME}.cmd"));
        let _ = std::fs::write(shim, format!("@\"%~dp0{NAME}.exe\" %*\r\n"));
        let _ = std::fs::remove_file(exe.with_file_name(format!("{OLD_NAME}.exe.old")));
    }
    Some(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pechka-legacy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn settings_dir_prefers_new_then_old() {
        let base = tmp("dir");
        assert_eq!(settings_dir(&base), base.join(NAME));
        std::fs::create_dir_all(base.join(OLD_NAME)).unwrap();
        assert_eq!(settings_dir(&base), base.join(OLD_NAME));
        std::fs::create_dir_all(base.join(NAME)).unwrap();
        assert_eq!(settings_dir(&base), base.join(NAME));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn migrate_settings_moves_and_links() {
        let base = tmp("move");
        let old = base.join(OLD_NAME);
        std::fs::create_dir_all(old.join("run")).unwrap();
        std::fs::write(old.join("servers.toml"), "x = 1\n").unwrap();
        assert!(migrate_settings(&base));
        assert_eq!(std::fs::read_to_string(base.join(NAME).join("servers.toml")).unwrap(), "x = 1\n");
        assert!(std::fs::symlink_metadata(&old).unwrap().file_type().is_symlink());
        // через ссылку старая служба видит те же файлы
        assert!(old.join("run").is_dir());
        assert_eq!(settings_dir(&base), base.join(NAME));
        // повторно — ничего не делает
        assert!(!migrate_settings(&base));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn migrate_settings_keeps_existing_new_dir() {
        let base = tmp("both");
        std::fs::create_dir_all(base.join(OLD_NAME)).unwrap();
        std::fs::create_dir_all(base.join(NAME)).unwrap();
        assert!(!migrate_settings(&base));
        assert!(std::fs::symlink_metadata(base.join(OLD_NAME)).unwrap().is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn migrate_exe_renames_and_leaves_alias() {
        let d = tmp("exe");
        let old = d.join(OLD_NAME);
        std::fs::write(&old, b"bin").unwrap();
        let new = migrate_exe_at(&old).unwrap();
        assert_eq!(new, d.join(NAME));
        assert_eq!(std::fs::read(&new).unwrap(), b"bin");
        assert!(std::fs::symlink_metadata(&old).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&old).unwrap(), b"bin");
        // повторно: mctui уже ссылка
        assert!(migrate_exe_at(&old).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn migrate_exe_skips_other_names_and_existing_target() {
        let d = tmp("skip");
        let other = d.join("something");
        std::fs::write(&other, b"bin").unwrap();
        assert!(migrate_exe_at(&other).is_none());
        let old = d.join(format!("{OLD_NAME}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(d.join(format!("{NAME}{}", std::env::consts::EXE_SUFFIX)), b"new").unwrap();
        assert!(migrate_exe_at(&old).is_none());
        assert_eq!(std::fs::read(&old).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&d);
    }
}
