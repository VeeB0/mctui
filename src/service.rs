//! Автозапуск фоновой службы при входе в систему и её запуск из окна.
//!
//! macOS — LaunchAgent (~/Library/LaunchAgents), Linux — systemd --user (иначе XDG autostart),
//! Windows — ключ HKCU\…\Run. Служба поднимает серверы с `autostart = true`.

use crate::tr;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(target_os = "macos")]
const LABEL: &str = "io.github.veeb0.mctui";

/// Путь к программе для автозапуска. Для Homebrew — стабильная ссылка `<prefix>/bin/mctui`
/// вместо пути внутри Cellar (он меняется с каждой версией).
pub fn exe_for_service() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let s = exe.to_string_lossy().to_string();
    if let Some(i) = s.find("/Cellar/mctui/") {
        let link = PathBuf::from(format!("{}/bin/mctui", &s[..i]));
        if link.exists() {
            return Ok(link);
        }
    }
    Ok(exe)
}

/// Сборка из `target/` (разработка) — автозапуск не ставим.
fn is_dev_build(exe: &Path) -> bool {
    let s = exe.to_string_lossy();
    s.contains("/target/debug/") || s.contains("/target/release/") || s.contains("\\target\\")
}

#[cfg(target_os = "macos")]
fn log_path() -> PathBuf {
    crate::config::state_dir(&crate::config::default_path()).join("daemon.log")
}

fn run_quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg(target_os = "macos")]
fn plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        plist_path().is_some_and(|p| p.exists())
    }
    #[cfg(target_os = "linux")]
    {
        linux_unit().is_some_and(|p| p.exists()) || linux_desktop().is_some_and(|p| p.exists())
    }
    #[cfg(windows)]
    {
        run_quiet(Command::new("reg").args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "mctui",
        ]))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn linux_unit() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("systemd/user/mctui.service"))
}
#[cfg(target_os = "linux")]
fn linux_desktop() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("autostart/mctui.desktop"))
}

/// Включить автозапуск. Возвращает true, если служба уже запущена системой (иначе её надо запустить самим).
pub fn install() -> Result<bool> {
    let exe = exe_for_service()?;
    if is_dev_build(&exe) {
        bail!(tr!("сборка из target/ — автозапуск не включается", "development build — autostart not enabled"));
    }
    let exe_s = exe.display().to_string();
    #[cfg(target_os = "macos")]
    {
        let plist = plist_path().context("HOME")?;
        let log = log_path();
        if let Some(d) = log.parent() {
            std::fs::create_dir_all(d)?;
        }
        let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
            xml_escape(&exe_s),
            xml_escape(&path_env),
            xml_escape(&log.display().to_string()),
            xml_escape(&log.display().to_string()),
        );
        let changed = std::fs::read_to_string(&plist).ok().as_deref() != Some(body.as_str());
        if changed {
            if let Some(d) = plist.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&plist, &body)?;
        }
        let domain = format!("gui/{}", uid());
        if changed {
            let _ = run_quiet(Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]));
        }
        let ok = run_quiet(Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist))
            || run_quiet(Command::new("launchctl").args(["kickstart", &format!("{domain}/{LABEL}")]));
        Ok(ok)
    }
    #[cfg(target_os = "linux")]
    {
        let has_systemd = run_quiet(Command::new("systemctl").args(["--user", "--version"]));
        if has_systemd {
            let unit = linux_unit().context("config dir")?;
            std::fs::create_dir_all(unit.parent().unwrap())?;
            std::fs::write(
                &unit,
                format!(
                    "[Unit]\nDescription=mctui — Minecraft servers in the background\n\n[Service]\nExecStart=\"{exe_s}\" daemon\nRestart=on-failure\nKillMode=mixed\nTimeoutStopSec=120\n\n[Install]\nWantedBy=default.target\n"
                ),
            )?;
            let _ = run_quiet(Command::new("systemctl").args(["--user", "daemon-reload"]));
            return Ok(run_quiet(Command::new("systemctl").args(["--user", "enable", "--now", "mctui.service"])));
        }
        let desktop = linux_desktop().context("config dir")?;
        std::fs::create_dir_all(desktop.parent().unwrap())?;
        std::fs::write(
            &desktop,
            format!(
                "[Desktop Entry]\nType=Application\nName=mctui\nExec=\"{exe_s}\" daemon\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n"
            ),
        )?;
        Ok(false)
    }
    #[cfg(windows)]
    {
        // Окно консоли при входе прячем через PowerShell -WindowStyle Hidden.
        let value = format!("powershell.exe -NoProfile -WindowStyle Hidden -Command \"& '{exe_s}' daemon\"");
        if !run_quiet(Command::new("reg").args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "mctui",
            "/t",
            "REG_SZ",
            "/d",
            &value,
            "/f",
        ])) {
            bail!("reg add failed");
        }
        Ok(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = exe_s;
        Ok(false)
    }
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if let Some(p) = plist_path() {
            let _ = run_quiet(Command::new("launchctl").args(["bootout", &format!("gui/{}/{LABEL}", uid())]));
            if p.exists() {
                std::fs::remove_file(p)?;
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = run_quiet(Command::new("systemctl").args(["--user", "disable", "mctui.service"]));
        for p in [linux_unit(), linux_desktop()].into_iter().flatten() {
            if p.exists() {
                std::fs::remove_file(p)?;
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = run_quiet(Command::new("reg").args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "mctui",
            "/f",
        ]));
    }
    Ok(())
}

/// Запустить службу отдельным процессом, не привязанным к терминалу.
pub fn spawn_detached(config_path: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = crate::config::state_dir(config_path).join("daemon.log");
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d)?;
    }
    let err = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").arg("--config").arg(config_path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
    }
    cmd.spawn().context(tr!("не удалось запустить службу", "could not start the service"))?;
    Ok(())
}

/// Запуск службы: через автозапуск системы (если это основной конфиг), иначе своим процессом.
pub fn start(config_path: &Path, default_config: bool) -> Result<()> {
    let allow_service = default_config && std::env::var_os("MCTUI_NO_SERVICE").is_none();
    if allow_service {
        match install() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(_) => {}
        }
    }
    spawn_detached(config_path)
}
