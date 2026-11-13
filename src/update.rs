//! Самообновление из GitHub Releases. Настройки (servers.toml) лежат отдельно и не трогаются.
//!
//! Сеть — через системный `curl` (есть в macOS, Linux и Windows 10+), чтобы не тащить TLS-стек
//! в бинарник и спокойно собирать его под все платформы.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// owner/repo; в CI подставляется из `github.repository`.
pub const REPO: &str = match option_env!("MCTUI_REPO") {
    Some(r) => r,
    None => "VeeB0/mctui",
};

/// Имя файла сборки в релизе для текущей платформы.
pub fn asset_name() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("mctui-aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("mctui-x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("mctui-x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("mctui-aarch64-unknown-linux-musl")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("mctui-x86_64-pc-windows-msvc.exe")
    } else {
        None
    }
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

fn curl() -> Command {
    let mut c = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    c.args(["-fsSL", "--connect-timeout", "10", "--retry", "2"]);
    c
}

/// Последняя версия из GitHub: редирект /releases/latest → /releases/tag/vX.Y.Z.
pub fn latest_version() -> Result<String> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let out = curl()
        .args(["--max-time", "20", "-o", null_device(), "-w", "%{url_effective}", &url])
        .output()
        .context(crate::tr!("не удалось запустить curl", "could not run curl"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "{}",
            crate::tr!(
                format!("GitHub недоступен или релизов ещё нет ({e})"),
                format!("GitHub is unreachable or there are no releases yet ({e})")
            )
        );
    }
    let effective = String::from_utf8_lossy(&out.stdout).to_string();
    let tag = effective.rsplit("/tag/").next().filter(|_| effective.contains("/tag/"));
    match tag {
        Some(t) => Ok(t.trim().trim_start_matches('v').to_string()),
        None => bail!(
            "{}",
            crate::tr!(
                format!("не удалось определить последнюю версию по {effective}"),
                format!("could not find the latest version from {effective}")
            )
        ),
    }
}

fn parse(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect()
}

/// `candidate` новее `current`?
pub fn is_newer(candidate: &str, current: &str) -> bool {
    parse(candidate) > parse(current)
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let st = curl()
        .args(["-o"])
        .arg(dest)
        .arg(url)
        .status()
        .context(crate::tr!("не удалось запустить curl", "could not run curl"))?;
    if !st.success() {
        bail!("{}", crate::tr!(format!("не скачался {url}"), format!("download failed: {url}")));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Путь к самому себе (через симлинки).
fn self_path() -> Result<PathBuf> {
    let p = std::env::current_exe()?;
    Ok(p.canonicalize().unwrap_or(p))
}

/// Удаляет хвост прошлого обновления на Windows (старый exe нельзя удалить, пока он запущен).
pub fn cleanup_old() {
    if let Ok(p) = self_path() {
        let _ = std::fs::remove_file(old_path(&p));
    }
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("{}.old", exe.file_name().unwrap_or_default().to_string_lossy()))
}

/// Как установлен mctui — от этого зависит способ обновления.
pub enum Install {
    /// Через Homebrew: обновляет `brew upgrade`.
    Homebrew,
    /// Обычный файл: скачиваем сборку и подменяем его.
    Direct(PathBuf),
}

pub fn install_kind() -> Result<Install> {
    let exe = self_path()?;
    Ok(if exe.components().any(|c| c.as_os_str() == "Cellar") { Install::Homebrew } else { Install::Direct(exe) })
}

fn brew() -> PathBuf {
    ["/opt/homebrew/bin/brew", "/usr/local/bin/brew", "/home/linuxbrew/.linuxbrew/bin/brew"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("brew"))
}

/// `brew upgrade` для формулы mctui; вывод brew не печатается, в ошибке — его последние строки.
pub fn brew_upgrade() -> Result<()> {
    // Tap VeeB0/homebrew-mctui → формула VeeB0/mctui/mctui.
    let formula = format!("{REPO}/mctui");
    let out = Command::new(brew())
        .args(["upgrade", &formula])
        .output()
        .context(crate::tr!("не удалось запустить brew", "could not run brew"))?;
    if !out.status.success() {
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("brew upgrade: {}", tail.join(" / "));
    }
    Ok(())
}

/// Скачивает сборку `version`, проверяет SHA-256 и подменяет `exe`.
pub fn apply(exe: &Path, version: &str) -> Result<()> {
    let asset = asset_name()
        .context(crate::tr!("для этой платформы нет готовых сборок", "no prebuilt binaries for this platform"))?;
    let dir = exe.parent().context("exe dir")?;
    let new = dir.join(format!(".mctui-update-{}", std::process::id()));
    let sum = dir.join(format!(".mctui-update-{}.sha256", std::process::id()));
    let base = format!("https://github.com/{REPO}/releases/download/v{version}");
    let res = (|| {
        download(&format!("{base}/{asset}"), &new)?;
        download(&format!("{base}/{asset}.sha256"), &sum)?;
        let expected = std::fs::read_to_string(&sum)?.split_whitespace().next().unwrap_or("").to_lowercase();
        let actual = sha256_hex(&std::fs::read(&new)?);
        if expected != actual {
            bail!(
                "{}",
                crate::tr!(
                    format!("контрольная сумма не совпала (ожидалось {expected}, получено {actual})"),
                    format!("checksum mismatch (expected {expected}, got {actual})")
                )
            );
        }
        replace_self(exe, &new)
    })();
    let _ = std::fs::remove_file(&new);
    let _ = std::fs::remove_file(&sum);
    res
}

/// Обновление из окна: возвращает путь к новому бинарнику (для перезапуска окна).
pub fn update_to(version: &str) -> Result<PathBuf> {
    match install_kind()? {
        Install::Homebrew => {
            brew_upgrade()?;
            // Симлинк bin/mctui после upgrade указывает на новую версию.
            let link = brew().with_file_name("mctui");
            Ok(if link.exists() { link } else { std::env::current_exe()? })
        }
        Install::Direct(exe) => {
            apply(&exe, version)?;
            Ok(exe)
        }
    }
}

/// `mctui update`. Возвращает true, если программа заменена.
pub fn run_cli(force: bool) -> Result<bool> {
    let exe = match install_kind()? {
        Install::Homebrew => {
            println!(
                "{}",
                crate::tr!(
                    "mctui установлен через Homebrew — обновление: brew upgrade mctui",
                    "mctui was installed with Homebrew — update with: brew upgrade mctui"
                )
            );
            return Ok(false);
        }
        Install::Direct(exe) => exe,
    };
    asset_name()
        .context(crate::tr!("для этой платформы нет готовых сборок", "no prebuilt binaries for this platform"))?;
    println!(
        "{}",
        crate::tr!(
            format!("mctui {VERSION} — проверяю обновления ({REPO})…"),
            format!("mctui {VERSION} — checking for updates ({REPO})…")
        )
    );
    let latest = latest_version()?;
    if !force && !is_newer(&latest, VERSION) {
        println!(
            "{}",
            crate::tr!(
                format!("Установлена последняя версия ({VERSION})."),
                format!("You have the latest version ({VERSION}).")
            )
        );
        return Ok(false);
    }
    println!("{}", crate::tr!(format!("Скачиваю {latest}…"), format!("Downloading {latest}…")));
    apply(&exe, &latest)?;
    let cfg = crate::config::default_path();
    println!(
        "{}",
        crate::tr!(
            format!("Готово: mctui обновлён до {latest}. Настройки сохранены ({}).", cfg.display()),
            format!("Done: mctui updated to {latest}. Settings kept ({}).", cfg.display())
        )
    );
    Ok(true)
}

fn replace_self(exe: &Path, new: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        // Запущенный exe нельзя перезаписать, но можно переименовать.
        let old = old_path(exe);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).context(crate::tr!(
            "не удалось переименовать текущий mctui.exe",
            "could not rename the running mctui.exe"
        ))?;
    }
    std::fs::rename(new, exe).with_context(|| {
        crate::tr!(
            format!("нет прав на запись в {} — запустите обновление с нужными правами", exe.display()),
            format!("no write access to {} — run the update with the right permissions", exe.display())
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn sha256_known_value() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn has_asset_for_this_platform() {
        assert!(asset_name().is_some());
    }

    #[test]
    fn replace_self_swaps_file() {
        let d = std::env::temp_dir().join(format!("mctui-upd-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let exe = d.join("mctui-test-bin");
        let new = d.join("new-bin");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        replace_self(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
