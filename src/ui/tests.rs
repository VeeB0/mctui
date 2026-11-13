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
use crate::proto::{Msg, NoticeKind, RconView, Request, ServerView};
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
    app_with(servers, Client::detached())
}

fn app_with(servers: Vec<ServerView>, client: Client) -> App {
    App::for_test(PathBuf::from(NOWHERE).join("servers.toml"), client, servers)
}

fn key(app: &mut App, c: char) {
    app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
}

fn press(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
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
    key(&mut a, ']');
    assert_eq!(a.addons.sub, crate::app::AddonsSub::Plugins, "и подвкладки не листаются");
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

/// Временная папка сервера (`server.properties` с `level-name=world`), удаляется в Drop.
struct TempServer(PathBuf);

impl TempServer {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("pechka-ui-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("server.properties"), "level-name=world\n").unwrap();
        TempServer(dir)
    }

    fn level(&self) -> PathBuf {
        self.0.join("world")
    }

    /// zip-датапак в `world/datapacks/`: `pack.mcmeta` и пустые файлы `files`.
    fn pack(&self, file_name: &str, files: &[&str]) {
        let mut entries = vec![("pack.mcmeta", r#"{"pack":{"description":"test"}}"#)];
        entries.extend(files.iter().map(|f| (*f, "{}")));
        crate::datapacks::tests::make_zip(&self.level().join("datapacks").join(file_name), &entries);
    }

    /// Локальный сервер в этой папке: `Some(игроки)` — запущен, `None` — остановлен.
    fn view(&self, players: Option<&[&str]>) -> ServerView {
        let cfg = ServerConfig::new_local("Sandbox", self.0.clone(), vec!["./start.sh".into()], vec!["world".into()]);
        let mut v = ServerState::new(cfg, 0).view();
        v.version = Some("26.3".into());
        if let Some(p) = players {
            v.status = Status::Running;
            v.pid = Some(4242);
            v.players = p.iter().map(|s| s.to_string()).collect();
        }
        v
    }
}

impl Drop for TempServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `4`, ↓, → — подвкладка «Датапаки»; ещё ↓ — к записям (если они есть).
fn open_datapacks(a: &mut App) {
    key(a, '4');
    for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down] {
        press(a, code);
    }
    assert_eq!(a.addons.sub, crate::app::AddonsSub::Datapacks);
}

/// Консольные команды, ушедшие службе с прошлого вызова.
fn sent_commands(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Request>) -> Vec<String> {
    let mut v = vec![];
    while let Ok(r) = rx.try_recv() {
        if let Request::Command { cmd, .. } = r {
            v.push(cmd);
        }
    }
    v
}

fn toast_text(a: &App) -> String {
    a.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default()
}

#[tokio::test]
async fn datapack_toggle_with_players_asks_before_reload() {
    let srv = TempServer::new("dp-players");
    srv.pack("a.zip", &["data/ns/function/hi.mcfunction"]);
    with_lang(Lang::En, || {
        let (client, mut rx) = Client::recording();
        let mut a = app_with(vec![srv.view(Some(&["Alex"]))], client);
        open_datapacks(&mut a);
        key(&mut a, ' ');
        assert!(srv.level().join("datapacks.disabled/a.zip").is_file());
        assert!(!srv.level().join("datapacks/a.zip").exists());
        let Some(Popup::Confirm { text, action: Action::ReloadDatapacks(0), .. }) = &a.popup else {
            panic!("ждали вопрос о перезагрузке датапаков");
        };
        assert!(text.contains("Alex"), "{text}");
        assert!(toast_text(&a).contains("minecraft:reload"), "{}", toast_text(&a));
        assert!(sent_commands(&mut rx).is_empty(), "до ответа ничего не отправляется");
        press(&mut a, KeyCode::Enter);
        assert_eq!(sent_commands(&mut rx), ["minecraft:reload"]);
        assert!(toast_text(&a).contains("datapacks are reloading"), "{}", toast_text(&a));
    });
}

#[tokio::test]
async fn datapack_toggle_without_players_reloads() {
    let srv = TempServer::new("dp-empty");
    srv.pack("a.zip", &["data/ns/function/hi.mcfunction"]);
    with_lang(Lang::En, || {
        let (client, mut rx) = Client::recording();
        let mut a = app_with(vec![srv.view(Some(&[]))], client);
        open_datapacks(&mut a);
        key(&mut a, ' ');
        assert!(srv.level().join("datapacks.disabled/a.zip").is_file());
        assert!(a.popup.is_none());
        assert!(toast_text(&a).contains("datapacks are reloading"), "{}", toast_text(&a));
        assert_eq!(sent_commands(&mut rx), ["minecraft:reload"]);
        // И обратно: пак возвращается в datapacks/, снова перезагрузка.
        key(&mut a, ' ');
        assert!(srv.level().join("datapacks/a.zip").is_file());
        assert!(!srv.level().join("datapacks.disabled/a.zip").exists());
        assert_eq!(sent_commands(&mut rx), ["minecraft:reload"]);
    });
}

#[tokio::test]
async fn worldgen_datapack_asks_then_needs_restart() {
    let srv = TempServer::new("dp-worldgen");
    srv.pack("wg.zip", &["data/ns/worldgen/biome/x.json"]);
    with_lang(Lang::En, || {
        let (client, mut rx) = Client::recording();
        let mut a = app_with(vec![srv.view(Some(&["Alex"]))], client);
        open_datapacks(&mut a);
        key(&mut a, ' ');
        let Some(Popup::Confirm { action: Action::DatapackToggle(path), .. }) = &a.popup else {
            panic!("выключение пака генерации мира — только с подтверждением");
        };
        assert_eq!(*path, srv.level().join("datapacks/wg.zip"));
        assert!(srv.level().join("datapacks/wg.zip").is_file(), "до ответа файл на месте");
        press(&mut a, KeyCode::Enter);
        assert!(srv.level().join("datapacks.disabled/wg.zip").is_file());
        assert!(a.popup.is_none(), "перезагрузку не предлагаем");
        assert!(toast_text(&a).contains("after a restart"), "{}", toast_text(&a));
        assert!(sent_commands(&mut rx).is_empty(), "minecraft:reload не отправляется");
    });
}

#[tokio::test]
async fn datapack_disabled_in_game_on_stopped_server() {
    let srv = TempServer::new("dp-ingame-off");
    srv.pack("a.zip", &["data/ns/function/hi.mcfunction"]);
    crate::datapacks::tests::level_dat(&srv.level().join("level.dat"), &["file/a.zip"]);
    with_lang(Lang::En, || {
        let (client, mut rx) = Client::recording();
        let mut a = app_with(vec![srv.view(None)], client);
        open_datapacks(&mut a);
        assert!(a.datapacks.items[0].disabled_in_game);
        key(&mut a, ' ');
        assert!(srv.level().join("datapacks/a.zip").is_file(), "файл не переносится");
        assert!(toast_text(&a).contains("/datapack"), "{}", toast_text(&a));
        assert!(sent_commands(&mut rx).is_empty());
    });
}

#[tokio::test]
async fn datapack_disabled_in_game_on_running_server_enables_in_game() {
    let srv = TempServer::new("dp-ingame-on");
    srv.pack("a.zip", &["data/ns/function/hi.mcfunction"]);
    srv.pack("b.zip", &["data/ns/enchantment/x.json"]);
    crate::datapacks::tests::level_dat(&srv.level().join("level.dat"), &["file/a.zip", "file/b.zip"]);
    with_lang(Lang::En, || {
        let (client, mut rx) = Client::recording();
        let mut a = app_with(vec![srv.view(Some(&[]))], client);
        open_datapacks(&mut a);
        key(&mut a, ' ');
        assert!(a.popup.is_none(), "игроков нет — без вопроса");
        assert!(srv.level().join("datapacks/a.zip").is_file(), "файл не переносится");
        assert_eq!(sent_commands(&mut rx), [r#"datapack enable "file/a.zip""#]);
        assert_eq!(toast_text(&a), "a: enabled in game");
        assert!(!a.datapacks.items[0].disabled_in_game, "строка больше не «выкл в игре»");
        notice(&mut a, r#"sent: datapack enable "file/a.zip""#, NoticeKind::Ok);
        assert_eq!(toast_text(&a), "a: enabled in game");
        // Пак, которому нужен перезапуск: команда та же, но сообщение честно говорит о перезапуске.
        press(&mut a, KeyCode::Down);
        key(&mut a, ' ');
        assert_eq!(sent_commands(&mut rx), [r#"datapack enable "file/b.zip""#]);
        assert_eq!(toast_text(&a), "b: enabled in game — takes effect after a restart");
        assert!(!a.datapacks.items[1].disabled_in_game);
    });
}

#[tokio::test]
async fn datapack_enable_in_game_with_players_asks() {
    let srv = TempServer::new("dp-ingame-players");
    srv.pack("a.zip", &["data/ns/function/hi.mcfunction"]);
    crate::datapacks::tests::level_dat(&srv.level().join("level.dat"), &["file/a.zip"]);
    with_lang(Lang::En, || {
        let (client, mut rx) = Client::recording();
        let mut a = app_with(vec![srv.view(Some(&["Alex"]))], client);
        open_datapacks(&mut a);
        key(&mut a, ' ');
        let Some(Popup::Confirm { text, action: Action::EnableDatapackInGame(0, d), .. }) = &a.popup else {
            panic!("datapack enable перезагружает датапаки — при игроках сначала вопрос");
        };
        assert_eq!(d.file_name, "a.zip");
        assert!(text.contains("Alex") && text.contains("A reload causes a brief lag."), "{text}");
        assert!(sent_commands(&mut rx).is_empty(), "до ответа ничего не отправляется");
        press(&mut a, KeyCode::Esc);
        assert!(sent_commands(&mut rx).is_empty(), "отказ — ничего не отправляется");
        assert!(a.datapacks.items[0].disabled_in_game);
        key(&mut a, ' ');
        press(&mut a, KeyCode::Enter);
        assert_eq!(sent_commands(&mut rx), [r#"datapack enable "file/a.zip""#]);
        assert_eq!(toast_text(&a), "a: enabled in game");
        assert!(!a.datapacks.items[0].disabled_in_game, "строка больше не «выкл в игре»");
        assert!(srv.level().join("datapacks/a.zip").is_file(), "файл не переносится");
    });
}

#[tokio::test]
async fn datapack_reload_declined_leaves_a_hint() {
    let srv = TempServer::new("dp-declined");
    srv.pack("a.zip", &["data/ns/function/hi.mcfunction"]);
    with_lang(Lang::En, || {
        let (client, mut rx) = Client::recording();
        let mut a = app_with(vec![srv.view(Some(&["Alex"]))], client);
        open_datapacks(&mut a);
        key(&mut a, ' ');
        assert!(matches!(a.popup, Some(Popup::Confirm { action: Action::ReloadDatapacks(0), .. })));
        // Подсказка живёт в самом вопросе: тост исчезнет через 5 с, а вопрос может висеть дольше.
        let Some(Popup::Confirm { text, .. }) = &a.popup else { unreachable!() };
        assert!(text.contains("If you decline") && text.contains("minecraft:reload"), "{text}");
        press(&mut a, KeyCode::Esc);
        assert!(a.popup.is_none());
        assert!(sent_commands(&mut rx).is_empty());
        let t = toast_text(&a);
        assert!(t.starts_with("a: disabled") && t.contains("minecraft:reload") && t.contains("restart"), "{t}");
    });
}

#[tokio::test]
async fn datapack_toggle_waits_for_download() {
    let srv = TempServer::new("dp-busy");
    srv.pack("a.zip", &["data/ns/function/hi.mcfunction"]);
    with_lang(Lang::En, || {
        let (client, mut rx) = Client::recording();
        let mut a = app_with(vec![srv.view(Some(&[]))], client);
        open_datapacks(&mut a);
        a.datapacks.busy = Some("…".into());
        key(&mut a, ' ');
        assert!(toast_text(&a).contains("please wait"), "{}", toast_text(&a));
        assert!(srv.level().join("datapacks/a.zip").is_file(), "пока идёт загрузка, файлы не трогаем");
        assert!(sent_commands(&mut rx).is_empty());
    });
}

#[tokio::test]
async fn datapack_check_with_empty_list_says_so() {
    let srv = TempServer::new("dp-check-empty");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        open_datapacks(&mut a);
        key(&mut a, 'c');
        assert_eq!(toast_text(&a), "no datapacks");
        assert!(!a.datapacks.checking);
    });
}

#[tokio::test]
async fn addons_lists_footer_mentions_update_all() {
    use crate::app::SubLevel;
    with_lang(Lang::En, || {
        let mut a = app(vec![local_running()]);
        key(&mut a, '4');
        press(&mut a, KeyCode::Down);
        assert_eq!(a.addons.level, SubLevel::Lists);
        let screen = render(&a, 240, 30);
        let footer = screen.lines().rev().find(|l| !l.is_empty()).unwrap_or_default();
        assert!(footer.contains("U update all"), "{footer}");
    });
}

#[tokio::test]
async fn levels_reset_when_focus_leaves_content() {
    use crate::app::{AddonsSub, SubLevel};
    let mut a = app(vec![local_running()]);
    // Esc — к списку серверов; → возвращает фокус; ещё → листает вкладки окна, а не подвкладки.
    key(&mut a, '4');
    press(&mut a, KeyCode::Down);
    assert_eq!(a.addons.level, SubLevel::Lists);
    press(&mut a, KeyCode::Esc);
    assert_eq!(a.addons.level, SubLevel::Tabs, "фокус ушёл с содержимого — уровень сброшен");
    press(&mut a, KeyCode::Right);
    press(&mut a, KeyCode::Right);
    assert_eq!((a.tab, a.addons.sub), (Tab::Settings, AddonsSub::Plugins));
    // Tab — к строке ввода, Shift+Tab — обратно.
    key(&mut a, '4');
    press(&mut a, KeyCode::Down);
    press(&mut a, KeyCode::Tab);
    press(&mut a, KeyCode::BackTab);
    press(&mut a, KeyCode::Right);
    assert_eq!(a.tab, Tab::Settings, "Tab/Shift+Tab: → снова листает вкладки окна");
    // Shift+Tab с содержимого — к списку серверов.
    key(&mut a, '4');
    press(&mut a, KeyCode::Down);
    press(&mut a, KeyCode::BackTab);
    press(&mut a, KeyCode::Right);
    press(&mut a, KeyCode::Right);
    assert_eq!(a.tab, Tab::Settings);
    // То же на «Доступе».
    key(&mut a, '3');
    press(&mut a, KeyCode::Down);
    assert_eq!(a.access.level, SubLevel::Lists);
    press(&mut a, KeyCode::Esc);
    assert_eq!(a.access.level, SubLevel::Tabs);
    press(&mut a, KeyCode::Right);
    press(&mut a, KeyCode::Right);
    assert_eq!(a.tab, Tab::Addons);
}

#[tokio::test]
async fn datapack_update_confirm_lists_versions_and_worldgen() {
    use crate::plugins::UpdateInfo;
    let srv = TempServer::new("dp-update");
    srv.pack("fn.zip", &["data/ns/function/hi.mcfunction"]);
    srv.pack("wg.zip", &["data/ns/worldgen/biome/x.json"]);
    let upd = |v: &str| UpdateInfo {
        version: v.into(),
        file_name: "x.zip".into(),
        url: "https://example.invalid/x.zip".into(),
        sha1: None,
        page: String::new(),
    };
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        open_datapacks(&mut a);
        let (f, w) = (srv.level().join("datapacks/fn.zip"), srv.level().join("datapacks/wg.zip"));
        a.datapacks.updates.insert(f.clone(), upd("2.0"));
        a.datapacks.updates.insert(w.clone(), upd("3.0"));
        key(&mut a, 'U');
        let Some(Popup::Confirm { text, action: Action::ApplyDatapackUpdates(paths), .. }) = &a.popup else {
            panic!("ждали подтверждение обновления");
        };
        assert_eq!(*paths, [f.clone(), w.clone()]);
        for part in ["fn → 2.0", "wg → 3.0", "datapacks.disabled/.pechka-old/", "⚠ wg changes world generation"] {
            assert!(text.contains(part), "{part}: {text}");
        }
        press(&mut a, KeyCode::Esc);
        press(&mut a, KeyCode::Enter);
        let Some(Popup::Confirm { text, action: Action::ApplyDatapackUpdates(paths), .. }) = &a.popup else {
            panic!("Enter — обновить выбранный");
        };
        assert_eq!(*paths, [f]);
        assert!(!text.contains('⚠'), "{text}");
        // Пока идёт другая загрузка, второе задание не запускается (временные файлы общие).
        a.datapacks.busy = Some("…".into());
        press(&mut a, KeyCode::Enter);
        assert!(toast_text(&a).contains("please wait"), "{}", toast_text(&a));
        assert_eq!(a.datapacks.busy.as_deref(), Some("…"));
    });
}

#[tokio::test]
async fn datapack_search_works_without_core() {
    let srv = TempServer::new("dp-search");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        assert!(a.popup.is_none(), "плагины на ванильном сервере не ищутся");
        open_datapacks(&mut a);
        key(&mut a, 'i');
        let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель поиска") };
        assert_eq!(p.title, " Datapack search · Minecraft 26.3 ");
        assert!(p.lines[0].text.contains("Source: Modrinth."), "{}", p.lines[0].text);
    });
}

fn addons_screen(a: &mut App) -> String {
    key(a, '4');
    render(a, 160, 30)
}

#[tokio::test]
async fn fabric_without_mods_is_called_mods() {
    let srv = TempServer::new("fabric-mods");
    std::fs::write(srv.0.join("fabric-server-launch.jar"), b"").unwrap();
    std::fs::create_dir_all(srv.0.join("mods")).unwrap();
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        let screen = addons_screen(&mut a);
        assert!(screen.contains("Mods (0)"), "{screen}");
        assert!(screen.contains("No mods · i to find and install"), "{screen}");
    });
}

#[tokio::test]
async fn paper_is_called_plugins() {
    let srv = paper_search("paper-plugins");
    std::fs::create_dir_all(srv.0.join("plugins")).unwrap();
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        let screen = addons_screen(&mut a);
        assert!(screen.contains("Plugins (0)"), "{screen}");
        assert!(!screen.contains("No mods"), "{screen}");
    });
}

#[tokio::test]
async fn vanilla_says_only_datapacks() {
    let srv = TempServer::new("vanilla-addons");
    crate::datapacks::tests::make_zip(&srv.0.join("server.jar"), &[("version.json", r#"{"id":"26.3"}"#)]);
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        let screen = addons_screen(&mut a);
        let text = "The vanilla core supports no plugins or mods — only datapacks";
        assert!(screen.contains(text), "{screen}");
        key(&mut a, 'i');
        assert!(a.popup.is_none());
        assert_eq!(toast_text(&a), text);
    });
}

fn search_req(a: &App) -> u64 {
    let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель поиска") };
    let crate::app::PanelKind::PluginSearch { req, .. } = &p.kind else { panic!("ждали поиск") };
    *req
}

fn paper_search(tag: &str) -> TempServer {
    let srv = TempServer::new(tag);
    std::fs::write(srv.0.join("paper.jar"), b"").unwrap();
    srv
}

fn one_hit(title: &str) -> anyhow::Result<crate::plugin_search::SearchResults> {
    hit_from(crate::plugin_search::Source::Modrinth, title)
}

fn hit_from(source: crate::plugin_search::Source, title: &str) -> anyhow::Result<crate::plugin_search::SearchResults> {
    use crate::plugin_search::{SearchHit, SearchResults};
    Ok(SearchResults {
        hits: vec![SearchHit {
            source,
            id: format!("{source:?}-{title}"),
            slug: title.to_lowercase(),
            title: title.into(),
            description: String::new(),
            author: "me".into(),
            downloads: 1,
            latest_game_version: None,
            page: String::new(),
        }],
        ..Default::default()
    })
}

#[tokio::test]
async fn addon_search_loads_popular_on_open() {
    let srv = paper_search("popular-open");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель поиска") };
        assert_eq!(p.busy.as_deref(), Some("searching…"));
        assert!(p.input.as_ref().unwrap().value.is_empty());
        assert!(p.input_focus);
        assert!(p.lines[0].text.contains("Showing popular ones."), "{}", p.lines[0].text);
    });
}

#[tokio::test]
async fn addon_search_ignores_stale_popular() {
    let srv = paper_search("popular-stale");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        let old_req = search_req(&a);
        for c in "chunky".chars() {
            key(&mut a, c);
        }
        press(&mut a, KeyCode::Enter);
        a.apply_plugin_search(old_req, one_hit("Old"));
        let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель поиска") };
        assert!(p.items.iter().all(|r| !r.contains("Old")), "{:?}", p.items);
        assert!(p.busy.is_some(), "новый запрос ещё идёт");
        a.apply_plugin_search(search_req(&a), one_hit("Chunky"));
        let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель поиска") };
        assert!(p.items.iter().any(|r| r.contains("Chunky")), "{:?}", p.items);
        assert!(p.busy.is_none());
    });
}

fn more_hit(title: &str) -> anyhow::Result<crate::plugin_search::SearchResults> {
    one_hit(title).map(|r| crate::plugin_search::SearchResults { more: true, ..r })
}

fn search_page(a: &App) -> usize {
    let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель поиска") };
    let crate::app::PanelKind::PluginSearch { page, .. } = &p.kind else { panic!("ждали поиск") };
    *page
}

#[tokio::test]
async fn addon_search_ignores_more_while_busy() {
    let srv = paper_search("more-busy");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        a.apply_plugin_search(search_req(&a), more_hit("First"));
        press(&mut a, KeyCode::Down);
        key(&mut a, 'n');
        key(&mut a, 'n');
        assert_eq!(search_page(&a), 1);
        a.apply_plugin_search(search_req(&a), one_hit("Second"));
        let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель поиска") };
        assert!(p.items.iter().any(|r| r.contains("Second")), "{:?}", p.items);
        assert_eq!(search_page(&a), 1);
    });
}

#[tokio::test]
async fn addon_search_failed_next_page_is_retried() {
    let srv = paper_search("more-fail");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        a.apply_plugin_search(search_req(&a), more_hit("First"));
        press(&mut a, KeyCode::Down);
        key(&mut a, 'n');
        assert_eq!(search_page(&a), 1);
        a.apply_plugin_search(search_req(&a), Err(anyhow::anyhow!("boom")));
        let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель поиска") };
        assert!(p.busy.is_none());
        assert!(p.lines[0].text.contains("boom"), "{}", p.lines[0].text);
        assert_eq!(search_page(&a), 0);
    });
}

#[tokio::test]
async fn addons_help_follows_sub_tab() {
    let tab_help =
        |a: &App| a.help_sections()[0].keys.iter().map(|(k, d)| format!("{k} {d}")).collect::<Vec<_>>().join("\n");
    with_lang(Lang::En, || {
        let mut a = app(vec![local_running()]);
        key(&mut a, '4');
        assert!(tab_help(&a).contains("find & install (Modrinth, Hangar)"), "{}", tab_help(&a));
        key(&mut a, ']');
        let h = tab_help(&a);
        assert!(h.contains("find & install (Modrinth)") && !h.contains("Hangar"), "{h}");
        assert!(h.contains("enable / disable the datapack"), "{h}");
    });
}

fn notice(a: &mut App, text: &str, kind: NoticeKind) {
    a.on_event(AppEvent::Daemon(Box::new(Msg::Notice { text: text.into(), kind })));
}

#[tokio::test]
async fn datapack_toast_survives_daemon_sent_notice() {
    let srv = TempServer::new("dp-notice");
    srv.pack("a.zip", &["data/ns/function/hi.mcfunction"]);
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(Some(&[]))]);
        open_datapacks(&mut a);
        key(&mut a, ' ');
        let ours = toast_text(&a);
        assert!(ours.contains("a: disabled · datapacks are reloading"), "{ours}");
        // Служба на каждую команду отвечает «отправлено: …» — наше подробное сообщение остаётся.
        notice(&mut a, "sent: minecraft:reload", NoticeKind::Ok);
        assert_eq!(toast_text(&a), ours);
        // Один раз на команду; чужие команды — как раньше.
        notice(&mut a, "sent: minecraft:reload", NoticeKind::Ok);
        assert_eq!(toast_text(&a), "sent: minecraft:reload");
        notice(&mut a, "sent: list", NoticeKind::Ok);
        assert_eq!(toast_text(&a), "sent: list");
        // Ошибки показываются всегда; служба может говорить по-русски.
        key(&mut a, ' ');
        notice(&mut a, "sent: minecraft:reload", NoticeKind::Error);
        assert_eq!(toast_text(&a), "sent: minecraft:reload");
        notice(&mut a, "отправлено: minecraft:reload", NoticeKind::Ok);
        assert_eq!(toast_text(&a), "sent: minecraft:reload", "своя отметка по-русски тоже глушится");
    });
}

fn panel_of(a: &App) -> &crate::app::Panel {
    let Some(Popup::Panel(p)) = &a.popup else { panic!("ждали панель") };
    p
}

#[tokio::test]
async fn addon_search_drops_result_for_a_closed_window() {
    let srv = paper_search("stale-window");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        let old = search_req(&a);
        press(&mut a, KeyCode::Esc);
        assert!(a.popup.is_none());
        // Другое окно (датапаки) не принимает ответ прежнего.
        key(&mut a, ']');
        key(&mut a, 'i');
        assert_ne!(search_req(&a), old);
        a.apply_plugin_search(old, one_hit("Plugin"));
        let p = panel_of(&a);
        assert!(p.items.is_empty(), "{:?}", p.items);
        assert!(p.busy.is_some());
    });
}

#[tokio::test]
async fn addon_search_focus_follows_query() {
    let srv = paper_search("focus");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        a.apply_plugin_search(search_req(&a), one_hit("Popular"));
        let p = panel_of(&a);
        assert!(p.input_focus, "пустой запрос оставляет фокус в строке ввода");
        assert!(p.lines[0].text.contains("popular: 1 · type a name and press Enter"), "{}", p.lines[0].text);
        key(&mut a, 'x');
        press(&mut a, KeyCode::Enter);
        a.apply_plugin_search(search_req(&a), one_hit("Xray"));
        let p = panel_of(&a);
        assert!(!p.input_focus, "найденное — фокус на списке");
        assert!(p.lines[0].text.starts_with("found: 1"), "{}", p.lines[0].text);
    });
}

#[tokio::test]
async fn addon_search_failed_popular_offers_retry() {
    let srv = paper_search("retry");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        a.apply_plugin_search(search_req(&a), Err(anyhow::anyhow!("boom")));
        let p = panel_of(&a);
        assert!(p.lines[0].text.ends_with("boom · Enter to retry"), "{}", p.lines[0].text);
    });
}

#[tokio::test]
async fn addon_search_failed_next_page_has_no_retry_hint() {
    use crate::plugin_search::Source;
    let srv = paper_search("retry-next-page");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        let more = hit_from(Source::Modrinth, "First").map(|r| crate::plugin_search::SearchResults { more: true, ..r });
        a.apply_plugin_search(search_req(&a), more);
        press(&mut a, KeyCode::Down);
        key(&mut a, 'n');
        a.apply_plugin_search(search_req(&a), Err(anyhow::anyhow!("boom")));
        let p = panel_of(&a);
        assert!(p.lines[0].text.contains("boom"), "{}", p.lines[0].text);
        assert!(!p.lines[0].text.contains("Enter to retry"), "{}", p.lines[0].text);
    });
}

#[tokio::test]
async fn addon_search_dedupes_project_across_pages() {
    use crate::plugin_search::Source;
    let srv = paper_search("dedupe-pages");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        let more =
            hit_from(Source::Hangar, "ViaVersion").map(|r| crate::plugin_search::SearchResults { more: true, ..r });
        a.apply_plugin_search(search_req(&a), more);
        press(&mut a, KeyCode::Down);
        key(&mut a, 'n');
        a.apply_plugin_search(search_req(&a), hit_from(Source::Modrinth, "ViaVersion"));
        let p = panel_of(&a);
        assert_eq!(p.items.iter().filter(|r| r.contains("ViaVersion")).count(), 1, "{:?}", p.items);
    });
}

#[tokio::test]
async fn addon_search_enter_ignored_while_busy() {
    let srv = paper_search("enter-busy");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        key(&mut a, 'i');
        a.apply_plugin_search(search_req(&a), one_hit("First"));
        press(&mut a, KeyCode::Down);
        if let Some(Popup::Panel(p)) = &mut a.popup {
            p.busy = Some("searching…".into());
        }
        press(&mut a, KeyCode::Enter);
        assert_eq!(panel_of(&a).busy.as_deref(), Some("searching…"));
    });
}

#[tokio::test]
async fn label_follows_contents_when_list_is_not_empty() {
    let jar = |srv: &TempServer, dir: &str| {
        std::fs::create_dir_all(srv.0.join(dir)).unwrap();
        std::fs::write(srv.0.join(dir).join("a.jar"), b"").unwrap();
    };
    let both = TempServer::new("label-both");
    std::fs::write(both.0.join("fabric-server-launch.jar"), b"").unwrap();
    jar(&both, "plugins");
    jar(&both, "mods");
    let only_plugin = TempServer::new("label-plugin");
    std::fs::write(only_plugin.0.join("fabric-server-launch.jar"), b"").unwrap();
    jar(&only_plugin, "plugins");
    with_lang(Lang::En, || {
        let mut a = app(vec![both.view(None)]);
        let screen = addons_screen(&mut a);
        assert!(screen.contains("Plugins and mods (2)"), "{screen}");
        let mut a = app(vec![only_plugin.view(None)]);
        let screen = addons_screen(&mut a);
        assert!(screen.contains("Plugins (1)"), "{screen}");
    });
}

#[tokio::test]
async fn cached_vanilla_verdict_does_not_block_detected_core() {
    let srv = paper_search("stale-vanilla");
    with_lang(Lang::En, || {
        let mut a = app(vec![srv.view(None)]);
        key(&mut a, '4');
        a.plugins.core = Some(crate::app::AddonsCore::Vanilla);
        key(&mut a, 'i');
        assert!(matches!(a.popup, Some(Popup::Panel(_))), "поиск открывается, ядро Paper определено заново");
    });
}

/// Вкладка «Мир» с уже прочитанным миром (без ожидания фонового обхода).
fn open_world(a: &mut App, srv: &TempServer) {
    key(a, '7');
    press(a, KeyCode::Down);
    a.world.info = Some(crate::world::scan(&srv.0, "world"));
    a.world.sel = 0;
}

#[tokio::test]
async fn overworld_reset_needs_confirm_and_world_name() {
    let srv = TempServer::new("reset-overworld");
    for f in ["level.dat", "region/r.0.0.mca", "playerdata/a.dat"] {
        let p = srv.level().join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"x").unwrap();
    }
    with_lang(Lang::En, || {
        // Сервер запущен — даже не спрашиваем.
        let mut a = app(vec![srv.view(Some(&[]))]);
        open_world(&mut a, &srv);
        key(&mut a, 'R');
        assert!(a.popup.is_none());
        assert!(toast_text(&a).contains("Stop the server first"), "{}", toast_text(&a));

        let mut a = app(vec![srv.view(None)]);
        open_world(&mut a, &srv);
        key(&mut a, 'R');
        let Some(Popup::Confirm { text, action: Action::AskOverworldName, .. }) = &a.popup else {
            panic!("expected the Overworld warning")
        };
        assert!(text.contains("same seed") && text.contains(".pechka-before-reset/"), "{text}");
        // Esc — отмена.
        press(&mut a, KeyCode::Esc);
        assert!(a.popup.is_none());
        // Enter в предупреждении ведёт ко вводу имени, неверное имя ничего не трогает.
        key(&mut a, 'R');
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.popup, Some(Popup::Edit { target: crate::app::EditTarget::ResetOverworld { .. }, .. })));
        for c in "wrld".chars() {
            key(&mut a, c);
        }
        press(&mut a, KeyCode::Enter);
        assert!(a.popup.is_none());
        assert!(toast_text(&a).contains("did not match"), "{}", toast_text(&a));
        // Верное имя — ландшафт уходит в копию, игроки остаются.
        key(&mut a, 'R');
        press(&mut a, KeyCode::Enter);
        for c in "world".chars() {
            key(&mut a, c);
        }
        press(&mut a, KeyCode::Enter);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while srv.level().join("region").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!srv.level().join("region").exists());
    assert!(srv.level().join("playerdata/a.dat").exists());
    assert!(srv.level().join("level.dat").exists());
    let stash = crate::world::reset_stashes(&srv.0);
    assert_eq!(stash.len(), 1);
    assert!(stash[0].join("world/region/r.0.0.mca").exists());
}
