//! Снимки экрана: окно рисуется в `TestBackend` и сравнивается с текстом в `src/ui/snapshots/*.txt`.
//!
//! Изменили интерфейс намеренно — пересоздать снимки и просмотреть разницу в git diff:
//! `PECHKA_UPDATE_SNAPSHOTS=1 cargo test ui::tests`.
//! Номер версии в тексте заменяется на `X.Y.Z`, чтобы снимки не менялись с каждым релизом.
//! На Windows не запускаются: там другие разделители путей и подсказки (`C:\…`).

use crate::app::{Action, App, Popup, Screen, Tab};
use crate::client::Client;
use crate::config::ServerConfig;
use crate::event::AppEvent;
use crate::i18n::{Lang, with_lang};
use crate::proto::{Msg, RconView, ServerView};
use crate::server::{ServerState, Status};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::{Path, PathBuf};

/// Папка, которой нет: окно не найдёт там ни server.properties, ни логов — снимок не зависит от машины.
const NOWHERE: &str = "/nonexistent/pechka-snapshot";

fn local_running() -> ServerView {
    let cfg = ServerConfig::new_local(
        "Survival",
        PathBuf::from(NOWHERE).join("survival"),
        vec!["./start.sh".into()],
        vec!["world".into()],
    );
    let mut v = ServerState::new(cfg, 0).view();
    v.status = Status::Running;
    v.pid = Some(4242);
    v.players = vec!["Alex".into(), "Steve".into()];
    v.ops = vec!["alex".into()];
    v.max_players = Some(20);
    v.tps = Some([19.9, 20.0, 20.0]);
    v.cpu = Some(12.5);
    v.mem = Some(2_147_483_648);
    v.version = Some("Purpur 26.3".into());
    v.last_backup = Some("survival-20261113-050000.tar.gz".into());
    v
}

fn rcon_unreachable() -> ServerView {
    let mut v = ServerState::new(ServerConfig::new_rcon("Lobby", "play.example.org", 25575, "p"), 1).view();
    v.status = Status::Unreachable;
    v.rcon = RconView::Error("connection refused".into());
    v
}

fn app(servers: Vec<ServerView>) -> App {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let config = PathBuf::from(NOWHERE).join("servers.toml");
    let mut app = App::new(config.clone(), Client::detached(), tx);
    app.on_event(AppEvent::Daemon(Box::new(Msg::Welcome {
        version: crate::update::VERSION.into(),
        protocol: crate::proto::PROTOCOL,
        pid: 1,
        config_path: config,
        language: None,
        can_detach: true,
    })));
    app.on_event(AppEvent::Daemon(Box::new(Msg::State { servers })));
    app
}

fn key(app: &mut App, c: char) {
    app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
}

fn render(app: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(app, f)).unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.replace(crate::update::VERSION, "X.Y.Z")
}

fn check(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/snapshots").join(format!("{name}.txt"));
    if std::env::var_os("PECHKA_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("no snapshot {} — create it: PECHKA_UPDATE_SNAPSHOTS=1 cargo test ui::tests", path.display())
    });
    if expected != actual {
        let diff: Vec<String> = expected
            .lines()
            .zip(actual.lines())
            .enumerate()
            .filter(|(_, (e, a))| e != a)
            .take(8)
            .map(|(i, (e, a))| format!("line {}:\n  expected: {e}\n  actual:   {a}", i + 1))
            .collect();
        panic!(
            "snapshot {name} differs (PECHKA_UPDATE_SNAPSHOTS=1 cargo test ui::tests to accept):\n{}\n--- actual ---\n{actual}",
            diff.join("\n")
        );
    }
}

#[tokio::test]
async fn main_status() {
    with_lang(Lang::En, || {
        let a = app(vec![local_running(), rcon_unreachable()]);
        check("main_status_en", &render(&a, 120, 34));
    });
    with_lang(Lang::Ru, || {
        let a = app(vec![local_running(), rcon_unreachable()]);
        check("main_status_ru", &render(&a, 120, 34));
    });
}

#[tokio::test]
async fn main_narrow() {
    with_lang(Lang::Ru, || {
        let a = app(vec![local_running(), rcon_unreachable()]);
        check("main_narrow_80x24_ru", &render(&a, 80, 24));
    });
}

#[tokio::test]
async fn players_tab() {
    with_lang(Lang::En, || {
        let mut a = app(vec![local_running()]);
        key(&mut a, '2');
        assert_eq!(a.tab, Tab::Players);
        check("players_en", &render(&a, 120, 30));
    });
}

#[tokio::test]
async fn rcon_server_selected() {
    with_lang(Lang::En, || {
        let mut a = app(vec![local_running(), rcon_unreachable()]);
        a.selected = 1;
        check("rcon_unreachable_en", &render(&a, 120, 30));
    });
}

#[tokio::test]
async fn logs_screen() {
    with_lang(Lang::En, || {
        let mut a = app(vec![local_running()]);
        let lines = [
            "[05:19:12] [Server thread/INFO]: Starting minecraft server version 26.3",
            "[05:19:15] [Server thread/INFO]: Done (7.568s)! For help, type \"help\"",
            "[05:20:01] [Server thread/INFO]: Alex joined the game",
            "[05:20:07] [Async Chat Thread - #0/INFO]: <Alex> hi!",
            "[05:21:30] [Server thread/WARN]: Can't keep up! Is the server overloaded? Running 2034ms or 40 ticks behind",
        ];
        a.on_event(AppEvent::Daemon(Box::new(Msg::Log {
            id: 0,
            lines: lines.iter().map(|s| s.to_string()).collect(),
            reset: true,
        })));
        key(&mut a, 'L');
        assert_eq!(a.screen, Screen::Logs);
        check("logs_en", &render(&a, 120, 20));
    });
}

#[tokio::test]
async fn confirm_popup() {
    with_lang(Lang::Ru, || {
        let mut a = app(vec![local_running()]);
        a.popup = Some(Popup::Confirm {
            title: "Остановить сервер?".into(),
            text: "Сервер «Survival» будет остановлен принудительно (kill).\nНесохранённое пропадёт.".into(),
            action: Action::Kill(0),
        });
        check("confirm_popup_ru", &render(&a, 100, 28));
    });
}

#[tokio::test]
async fn no_servers_yet() {
    with_lang(Lang::En, || {
        let a = app(vec![]);
        check("no_servers_en", &render(&a, 100, 28));
    });
}

#[tokio::test]
async fn help_popup() {
    with_lang(Lang::Ru, || {
        let mut a = app(vec![local_running()]);
        key(&mut a, '2');
        key(&mut a, '?');
        assert!(matches!(a.popup, Some(Popup::Help { scroll: 0 })));
        check("help_players_ru", &render(&a, 100, 40));
        a.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.popup.is_none());
        // `,` — клавиша `?` в русской раскладке; F1 открывает справку откуда угодно.
        key(&mut a, ',');
        assert!(matches!(a.popup, Some(Popup::Help { .. })));
        key(&mut a, '?');
        assert!(a.popup.is_none());
        // В строке команды `?` — обычный символ.
        key(&mut a, ':');
        key(&mut a, '?');
        assert!(a.popup.is_none());
        assert_eq!(a.input.value, "?");
        a.on_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        assert!(matches!(a.popup, Some(Popup::Help { .. })));
    });
}

#[tokio::test]
async fn help_on_small_screen_scrolls() {
    with_lang(Lang::En, || {
        let mut a = app(vec![local_running()]);
        key(&mut a, 'L');
        key(&mut a, '?');
        for _ in 0..100 {
            a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        // Прокрутка дальше конца не уезжает в пустоту: последний раздел виден.
        check("help_logs_scrolled_en", &render(&a, 80, 24));
    });
}

#[tokio::test]
async fn status_without_backups() {
    with_lang(Lang::En, || {
        let mut a = app(vec![local_running()]);
        a.servers[0].view.cfg.backup_every_hours = 0;
        a.servers[0].view.last_backup = None;
        a.servers[0].newest_backup = Some(None);
        check("status_no_backups_en", &render(&a, 120, 34));
    });
}

#[tokio::test]
async fn stop_asks_only_when_players_online() {
    let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    with_lang(Lang::Ru, || {
        let mut a = app(vec![local_running()]);
        a.on_key(ctrl('x'));
        let Some(Popup::Confirm { text, action: Action::Stop { id: 0, restart: false }, .. }) = &a.popup else {
            panic!("ждали подтверждение остановки");
        };
        assert!(text.contains("2 игрока: Alex, Steve"), "{text}");
        check("stop_with_players_ru", &render(&a, 100, 28));
        a.popup = None;
        a.on_key(ctrl('r'));
        assert!(matches!(a.popup, Some(Popup::Confirm { action: Action::Stop { restart: true, .. }, .. })));
        a.popup = None;

        a.servers[0].view.players.clear();
        a.on_key(ctrl('x'));
        assert!(a.popup.is_none(), "пустой сервер останавливается без вопросов");
    });
}

#[tokio::test]
async fn arrows_switch_tabs_on_access_tab() {
    use crate::access::List;
    let arrow = |a: &mut App, code| a.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    let mut a = app(vec![local_running()]);
    key(&mut a, '3');
    assert_eq!(a.tab, Tab::Access);
    arrow(&mut a, KeyCode::Right);
    assert_eq!(a.tab, Tab::Addons, "→ уходит с «Доступа» на следующую вкладку");
    arrow(&mut a, KeyCode::Left);
    arrow(&mut a, KeyCode::Left);
    assert_eq!(a.tab, Tab::Players, "← уходит с «Доступа» на предыдущую вкладку");

    key(&mut a, '3');
    key(&mut a, ']');
    assert_eq!(a.access.list, List::Ops, "] — следующий список");
    key(&mut a, 'х');
    assert_eq!(a.access.list, List::Whitelist, "х ([ в русской раскладке) — предыдущий список");
    assert_eq!(a.tab, Tab::Access);
}

#[tokio::test]
async fn access_tab_down_enters_lists_up_leaves() {
    use crate::access::{Entry, List};
    use crate::app::SubLevel;
    let arrow = |a: &mut App, code| a.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    let mut a = app(vec![local_running()]);
    key(&mut a, '3');
    assert_eq!(a.access.level, SubLevel::Tabs);

    arrow(&mut a, KeyCode::Down);
    assert_eq!(a.access.level, SubLevel::Lists, "↓ спускается к спискам");
    arrow(&mut a, KeyCode::Right);
    assert_eq!((a.tab, a.access.list), (Tab::Access, List::Ops), "в списках → листает списки, а не вкладки");
    arrow(&mut a, KeyCode::Left);
    assert_eq!((a.tab, a.access.list), (Tab::Access, List::Whitelist));
    arrow(&mut a, KeyCode::Down);
    assert_eq!(a.access.level, SubLevel::Lists, "в пустой список не спуститься");

    let entry = |name: &str| Entry {
        name: name.into(),
        uuid: None,
        level: None,
        reason: None,
        created: None,
        expires: None,
        source: None,
    };
    a.access.entries = vec![entry("Alex"), entry("Steve")];
    arrow(&mut a, KeyCode::Down);
    assert_eq!((a.access.level, a.access.sel), (SubLevel::Entries, 0), "↓ спускается в записи");
    arrow(&mut a, KeyCode::Down);
    assert_eq!(a.access.sel, 1);
    arrow(&mut a, KeyCode::Up);
    arrow(&mut a, KeyCode::Up);
    assert_eq!(a.access.level, SubLevel::Lists, "↑ с первой записи — обратно к спискам");
    arrow(&mut a, KeyCode::Up);
    assert_eq!(a.access.level, SubLevel::Tabs, "↑ из списков — обратно к вкладкам");
    arrow(&mut a, KeyCode::Right);
    assert_eq!(a.tab, Tab::Addons, "наверху → снова листает вкладки окна");
    key(&mut a, '3');
    assert_eq!(a.access.level, SubLevel::Tabs, "вернулись на «Доступ» — снова наверху");
}

#[tokio::test]
async fn addons_tab_levels_and_subtabs() {
    use crate::app::{AddonsSub, SubLevel};
    let arrow = |a: &mut App, code| a.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    let mut a = app(vec![local_running()]);
    key(&mut a, '4');
    assert_eq!((a.tab, a.addons.level, a.addons.sub), (Tab::Addons, SubLevel::Tabs, AddonsSub::Plugins));
    arrow(&mut a, KeyCode::Right);
    assert_eq!(a.tab, Tab::Settings, "→ на верхнем уровне листает вкладки окна");
    arrow(&mut a, KeyCode::Left);
    assert_eq!(a.tab, Tab::Addons);
    arrow(&mut a, KeyCode::Down);
    assert_eq!(a.addons.level, SubLevel::Lists);
    arrow(&mut a, KeyCode::Right);
    assert_eq!((a.tab, a.addons.sub), (Tab::Addons, AddonsSub::Datapacks));
    arrow(&mut a, KeyCode::Left);
    assert_eq!(a.addons.sub, AddonsSub::Plugins);
    key(&mut a, ']');
    assert_eq!(a.addons.sub, AddonsSub::Datapacks, "] листает подвкладки");
    arrow(&mut a, KeyCode::Up);
    assert_eq!(a.addons.level, SubLevel::Tabs);
    key(&mut a, '4');
    assert_eq!((a.addons.sub, a.addons.level), (AddonsSub::Plugins, SubLevel::Tabs), "повторный 4 — всё сброшено");
}

#[tokio::test]
async fn addons_on_rcon_server_arrows_switch_tabs() {
    use crate::app::SubLevel;
    let arrow = |a: &mut App, code| a.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    let mut a = app(vec![rcon_unreachable()]);
    key(&mut a, '4');
    arrow(&mut a, KeyCode::Down);
    assert_eq!(a.addons.level, SubLevel::Tabs, "на RCON-сервере уровни не меняются");
    arrow(&mut a, KeyCode::Right);
    assert_eq!(a.tab, Tab::Settings);
}

#[tokio::test]
async fn addons_datapacks_snapshot() {
    use crate::datapacks::DatapackInfo;
    let arrow = |a: &mut App, code| a.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    let pack = |name: &str, enabled: bool, worldgen: bool, desc: Option<&str>| DatapackInfo {
        path: PathBuf::from(NOWHERE).join(format!("{name}.zip")),
        file_name: format!("{name}.zip"),
        name: name.into(),
        is_zip: true,
        enabled,
        disabled_in_game: false,
        description: desc.map(Into::into),
        worldgen,
        needs_restart: worldgen,
        sha1: String::new(),
    };
    with_lang(Lang::En, || {
        let mut a = app(vec![local_running()]);
        key(&mut a, '4');
        arrow(&mut a, KeyCode::Down);
        arrow(&mut a, KeyCode::Right);
        let terralith = pack("Terralith", true, true, Some("Overhauls overworld generation"));
        let vein = pack("VeinMiner", false, false, Some("Mine the whole vein"));
        a.datapacks.versions.insert(terralith.path.clone(), "2.6.5+26.3".into());
        a.datapacks.items = vec![terralith, vein];
        arrow(&mut a, KeyCode::Down);
        check("addons_datapacks_en", &render(&a, 120, 34));
    });
}
