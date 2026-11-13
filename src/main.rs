#[allow(dead_code)] // подключается к интерфейсу отдельно
mod access;
mod app;
mod backup;
mod config;
mod event;
#[allow(dead_code)] // используется по мере перевода интерфейса
mod i18n;
mod input;
mod log;
mod properties;
mod server;
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

const USAGE: &str = "mctui — TUI для серверов Minecraft

Использование:
  mctui                     открыть TUI
  mctui --config <файл>     использовать другой файл настроек
  mctui update              обновить mctui до последней версии (настройки сохраняются)
  mctui update --force      переустановить последнюю версию
  mctui --version           показать версию

Настройки: ";

struct Args {
    config: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut config = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => config = args.next().map(PathBuf::from),
            "-h" | "--help" | "help" => {
                println!("{USAGE}{}", config::default_path().display());
                std::process::exit(0)
            }
            "-V" | "--version" | "version" => {
                println!("mctui {}", update::VERSION);
                std::process::exit(0)
            }
            "update" | "upgrade" | "self-update" => {
                let force = args.any(|a| a == "--force" || a == "-f");
                match update::run_cli(force) {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("mctui: {e:#}");
                        std::process::exit(1)
                    }
                }
            }
            other => {
                eprintln!("неизвестный аргумент: {other}\n\n{USAGE}{}", config::default_path().display());
                std::process::exit(2);
            }
        }
    }
    Args { config }
}

fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
}

#[tokio::main]
async fn main() -> Result<()> {
    update::cleanup_old();
    let args = parse_args();
    let (cfg, cfg_path) = match config::Config::load(args.config) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mctui: {e:#}");
            std::process::exit(1);
        }
    };
    i18n::init(cfg.language.as_deref());

    let mut terminal = ratatui::init();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        hook(info);
    }));
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;

    let res = run(&mut terminal, cfg, cfg_path).await;
    restore_terminal();
    if let Err(e) = &res {
        eprintln!("mctui: {e:#}");
    }
    // Не ждём фоновых задач (проверка обновлений через медленную сеть) — серверы уже остановлены.
    std::process::exit(if res.is_ok() { 0 } else { 1 })
}

/// Ждёт сигнала завершения от ОС (закрытие терминала, kill).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close};
        let (Ok(mut close), Ok(mut brk)) = (ctrl_close(), ctrl_break()) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = close.recv() => {}
            _ = brk.recv() => {}
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

async fn run(terminal: &mut ratatui::DefaultTerminal, cfg: config::Config, cfg_path: PathBuf) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    check_update(tx.clone());
    let mut app = app::App::new(cfg, cfg_path, tx);
    let mut events = EventStream::new();
    let mut frame_tick = tokio::time::interval(Duration::from_millis(50));
    let mut sec_tick = tokio::time::interval(Duration::from_secs(1));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut dirty = true;

    loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(Event::Key(k))) => { app.on_key(k); dirty = true; }
                Some(Ok(Event::Mouse(m))) => { app.on_mouse(m); dirty = true; }
                Some(Ok(Event::Paste(s))) => { app.on_paste(&s); dirty = true; }
                Some(Ok(Event::Resize(_, _))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(_)) | None => { if !app.quitting { app.quit_stop_all(); } }
            },
            Some(ev) = rx.recv() => { app.on_event(ev); dirty = true; }
            _ = sec_tick.tick() => { app.on_tick(); dirty = true; }
            _ = &mut shutdown, if !app.quitting => app.quit_stop_all(),
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
    Ok(())
}
