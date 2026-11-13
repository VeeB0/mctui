mod access;
mod app;
mod backup;
mod backup_copy;
mod client;
mod complete;
mod config;
mod core;
mod create;
mod daemon;
mod event;
mod i18n;
mod input;
mod log;
mod plugins;
mod properties;
mod proto;
mod server;
mod service;
mod setup;
mod ui;
mod update;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream,
};
use crossterm::execute;
use futures::StreamExt;
use std::path::PathBuf;
use std::time::Duration;

fn usage() -> String {
    let body = tr!(
        "mctui — TUI для серверов Minecraft

Использование:
  mctui                       открыть окно (серверы работают в фоновой службе)
  mctui --config <файл>       другой файл настроек
  mctui daemon                запустить фоновую службу в этом терминале
  mctui daemon status         состояние службы и серверов
  mctui daemon stop           остановить все серверы и службу
  mctui daemon restart        перезапустить службу (серверы будут остановлены)
  mctui autostart on|off      запуск службы при входе в систему
  mctui update [--force]      обновить mctui (настройки сохраняются)
  mctui --version             версия

Язык: MCTUI_LANG=en|ru или language в servers.toml.
Настройки: ",
        "mctui — a TUI for Minecraft servers

Usage:
  mctui                       open the window (servers run in a background service)
  mctui --config <file>       use another settings file
  mctui daemon                run the background service in this terminal
  mctui daemon status         show the service and its servers
  mctui daemon stop           stop all servers and the service
  mctui daemon restart        restart the service (servers will be stopped)
  mctui autostart on|off      start the service when you log in
  mctui update [--force]      update mctui (settings are kept)
  mctui --version             show the version

Language: MCTUI_LANG=en|ru or `language` in servers.toml.
Settings: "
    );
    format!("{body}{}", config::default_path().display())
}

enum Cmd {
    Tui,
    Daemon,
    DaemonStatus,
    DaemonStop,
    DaemonRestart,
    Autostart(bool),
    Update { force: bool },
}

struct Args {
    config: Option<PathBuf>,
    cmd: Cmd,
}

fn die(msg: impl std::fmt::Display, code: i32) -> ! {
    eprintln!("{msg}");
    std::process::exit(code)
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut config = None;
    let mut cmd = Cmd::Tui;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => match args.next() {
                Some(p) => config = Some(PathBuf::from(p)),
                None => die(tr!("--config: нужен путь к файлу", "--config: a file path is required"), 2),
            },
            "-h" | "--help" | "help" => {
                println!("{}", usage());
                std::process::exit(0)
            }
            "-V" | "--version" | "version" => {
                println!("mctui {}", update::VERSION);
                std::process::exit(0)
            }
            "update" | "upgrade" | "self-update" => cmd = Cmd::Update { force: false },
            "--force" | "-f" if matches!(cmd, Cmd::Update { .. }) => cmd = Cmd::Update { force: true },
            "daemon" | "service" => cmd = Cmd::Daemon,
            "run" if matches!(cmd, Cmd::Daemon) => {}
            "status" if matches!(cmd, Cmd::Daemon) => cmd = Cmd::DaemonStatus,
            "stop" if matches!(cmd, Cmd::Daemon) => cmd = Cmd::DaemonStop,
            "restart" if matches!(cmd, Cmd::Daemon) => cmd = Cmd::DaemonRestart,
            "autostart" => cmd = Cmd::Autostart(true),
            "on" | "enable" if matches!(cmd, Cmd::Autostart(_)) => cmd = Cmd::Autostart(true),
            "off" | "disable" if matches!(cmd, Cmd::Autostart(_)) => cmd = Cmd::Autostart(false),
            other => die(format!("{}: {other}\n\n{}", tr!("неизвестный аргумент", "unknown argument"), usage()), 2),
        }
    }
    Args { config, cmd }
}

fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
}

fn main() {
    update::cleanup_old();
    // Язык до разбора аргументов — для справки; потом уточняется по servers.toml.
    i18n::init(None);
    let args = parse_args();
    // Основной конфиг (не --config и не MCTUI_CONFIG) — только для него ставится автозапуск системы.
    let default_config = args.config.is_none() && std::env::var_os("MCTUI_CONFIG").is_none();
    let (cfg, cfg_path) = match config::Config::load(args.config) {
        Ok(v) => v,
        Err(e) => die(format!("mctui: {e:#}"), 1),
    };
    i18n::init(cfg.language.as_deref());
    let cfg_path = std::path::absolute(&cfg_path).unwrap_or(cfg_path);

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio");
    let code = match args.cmd {
        Cmd::Tui => rt.block_on(tui(cfg_path, default_config)),
        Cmd::Daemon => match rt.block_on(daemon::run(cfg_path)) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("mctui daemon: {e:#}");
                1
            }
        },
        Cmd::DaemonStatus => rt.block_on(daemon_status(&cfg_path)),
        Cmd::DaemonStop => rt.block_on(daemon_stop(&cfg_path, true)),
        Cmd::DaemonRestart => {
            let c = rt.block_on(daemon_stop(&cfg_path, false));
            if c != 0 {
                c
            } else {
                match service::start(&cfg_path, default_config) {
                    Ok(()) => {
                        println!("{}", tr!("Служба запущена.", "Service started."));
                        0
                    }
                    Err(e) => {
                        eprintln!("mctui: {e:#}");
                        1
                    }
                }
            }
        }
        Cmd::Autostart(on) => autostart(on, &cfg_path, default_config),
        Cmd::Update { force } => match update::run_cli(force) {
            Ok(updated) => {
                if updated {
                    rt.block_on(after_update(&cfg_path));
                }
                0
            }
            Err(e) => {
                eprintln!("mctui: {e:#}");
                1
            }
        },
    };
    // Не ждём фоновых задач (медленная сеть при проверке обновлений).
    rt.shutdown_timeout(Duration::from_millis(100));
    std::process::exit(code)
}

async fn daemon_status(cfg_path: &std::path::Path) -> i32 {
    match client::status(cfg_path).await {
        Ok(Some((proto::Msg::Welcome { version, pid, .. }, servers))) => {
            println!("{} {version} (pid {pid}) — {}", tr!("Служба mctui", "mctui service"), cfg_path.display());
            let auto = if service::installed() { tr!("включён", "on") } else { tr!("выключен", "off") };
            println!("{}: {auto}", tr!("Автозапуск при входе", "Start at login"));
            for s in servers {
                let players =
                    if s.running() || !s.is_local() { format!(" · {} 👤", s.players.len()) } else { String::new() };
                println!("  {} — {}{players}", s.cfg.name, s.status.label());
            }
            0
        }
        Ok(_) => {
            println!("{}", tr!("Служба не запущена.", "The service is not running."));
            3
        }
        Err(e) => {
            eprintln!("mctui: {e:#}");
            1
        }
    }
}

/// Остановить службу (со всеми серверами) и дождаться, пока она выйдет.
async fn daemon_stop(cfg_path: &std::path::Path, verbose: bool) -> i32 {
    match client::oneshot(cfg_path, proto::Request::Shutdown).await {
        Ok(Some(_)) => {
            println!("{}", tr!("Останавливаю серверы и службу…", "Stopping servers and the service…"));
            for _ in 0..1200 {
                if !daemon::alive(cfg_path).await {
                    if verbose {
                        println!("{}", tr!("Служба остановлена.", "Service stopped."));
                    }
                    return 0;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            eprintln!("{}", tr!("служба не завершилась за 5 минут", "the service did not exit within 5 minutes"));
            1
        }
        Ok(None) => {
            if verbose {
                println!("{}", tr!("Служба не запущена.", "The service is not running."));
            }
            0
        }
        Err(e) => {
            eprintln!("mctui: {e:#}");
            1
        }
    }
}

fn autostart(on: bool, cfg_path: &std::path::Path, default_config: bool) -> i32 {
    if on && !default_config {
        eprintln!(
            "{}",
            tr!(
                "автозапуск работает только с основным файлом настроек (без --config / MCTUI_CONFIG)",
                "autostart only works with the main settings file (no --config / MCTUI_CONFIG)"
            )
        );
        return 2;
    }
    let res = if on { service::install().map(|_| ()) } else { service::uninstall() };
    match res {
        Ok(()) => {
            if on {
                println!(
                    "{}",
                    tr!(
                        "Автозапуск включён: служба стартует при входе в систему.",
                        "Autostart enabled: the service starts when you log in."
                    )
                );
            } else {
                println!(
                    "{}",
                    tr!(
                        "Автозапуск выключен. Служба запустится при открытии mctui.",
                        "Autostart disabled. The service will start when you open mctui."
                    )
                );
            }
            let _ = cfg_path;
            0
        }
        Err(e) => {
            eprintln!("mctui: {e:#}");
            1
        }
    }
}

/// После обновления: свободную службу перезапускаем на новой версии, занятую — не трогаем.
async fn after_update(cfg_path: &std::path::Path) {
    match client::status(cfg_path).await {
        Ok(Some((_, servers))) if servers.iter().any(|s| s.running()) => println!(
            "{}",
            tr!(
                "Служба со старой версией продолжает работать, пока запущены серверы. Когда будет удобно: mctui daemon restart",
                "The old service keeps running while servers are up. When convenient: mctui daemon restart"
            )
        ),
        Ok(Some(_)) => {
            let _ = client::oneshot(cfg_path, proto::Request::Shutdown).await;
            println!(
                "{}",
                tr!(
                    "Служба перезапустится на новой версии при открытии mctui.",
                    "The service will restart on the new version when you open mctui."
                )
            );
        }
        _ => {}
    }
}

async fn tui(cfg_path: PathBuf, default_config: bool) -> i32 {
    let mut terminal = ratatui::init();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        hook(info);
    }));
    if let Err(e) = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste) {
        restore_terminal();
        eprintln!("mctui: {e}");
        return 1;
    }
    let res = run(&mut terminal, cfg_path, default_config).await;
    restore_terminal();
    match res {
        Ok(msg) => {
            if let Some(m) = msg {
                println!("{m}");
            }
            0
        }
        Err(e) => {
            eprintln!("mctui: {e:#}");
            1
        }
    }
}

/// Фоновая проверка новой версии на GitHub (раз за запуск, тихо при ошибках).
fn check_update(tx: tokio::sync::mpsc::UnboundedSender<event::AppEvent>) {
    if update::REPO.starts_with("__") || std::env::var_os("MCTUI_NO_UPDATE_CHECK").is_some() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        if let Ok(latest) = update::latest_version()
            && update::is_newer(&latest, update::VERSION)
        {
            let _ = tx.send(event::AppEvent::UpdateAvailable(latest));
        }
    });
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    cfg_path: PathBuf,
    default_config: bool,
) -> Result<Option<String>> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    check_update(tx.clone());
    let client = client::start(cfg_path.clone(), default_config, tx.clone());
    let mut app = app::App::new(cfg_path, client, tx);
    let mut events = EventStream::new();
    let mut frame_tick = tokio::time::interval(Duration::from_millis(50));
    let mut sec_tick = tokio::time::interval(Duration::from_secs(1));
    let mut dirty = true;

    loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(Event::Key(k))) => { app.on_key(k); dirty = true; }
                Some(Ok(Event::Mouse(m))) => { app.on_mouse(m); dirty = true; }
                Some(Ok(Event::Paste(s))) => { app.on_paste(&s); dirty = true; }
                Some(Ok(Event::Resize(_, _))) => dirty = true,
                Some(Ok(_)) => {}
                // Терминал закрыт — окно просто уходит, серверы остаются в службе.
                Some(Err(_)) | None => break,
            },
            Some(ev) = rx.recv() => { app.on_event(ev); dirty = true; }
            _ = sec_tick.tick() => { app.on_tick(); dirty = true; }
            _ = frame_tick.tick() => {
                if dirty {
                    app.area = terminal.size().map(|s| ratatui::layout::Rect::new(0, 0, s.width, s.height)).unwrap_or_default();
                    terminal.draw(|f| ui::draw(&app, f))?;
                    dirty = false;
                }
            }
        }
        if app.should_quit {
            break;
        }
    }
    Ok(app.exit_message.take())
}
