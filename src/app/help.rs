//! Справка по клавишам (`?` или `F1`): сначала клавиши того места, где вы сейчас, потом общие.

use super::*;

pub struct HelpSection {
    pub title: String,
    pub keys: Vec<(&'static str, &'static str)>,
}

fn section(title: impl Into<String>, keys: Vec<(&'static str, &'static str)>) -> HelpSection {
    HelpSection { title: title.into(), keys }
}

fn tab_keys(tab: Tab) -> Vec<(&'static str, &'static str)> {
    match tab {
        Tab::Status => vec![
            ("f", tr!("как друзьям зайти: адреса, порт, проверка", "how friends join: addresses, port check")),
            ("U", tr!("обновить ядро (с проверкой плагинов)", "update the core (with a plugin check)")),
            ("T", tr!("тестовая копия сервера на других портах", "test copy of the server on other ports")),
            ("J", tr!("Java и память (флаги Aikar, -Xmx)", "Java & memory (Aikar flags, -Xmx)")),
            ("v", tr!("связка прокси Velocity с серверами", "link a Velocity proxy with servers")),
        ],
        Tab::Players => vec![
            ("↑↓", tr!("выбрать игрока", "select a player")),
            ("k / b", tr!("кикнуть / забанить", "kick / ban")),
            ("o / d", tr!("выдать / снять оператора", "op / deop")),
            ("w", tr!("в белый список", "add to the whitelist")),
            ("s", tr!("статистика: наиграно, входы, активность", "statistics: playtime, logins, activity")),
        ],
        Tab::Access => vec![
            (
                "↓ / ↑",
                tr!(
                    "спуститься к спискам и записям / подняться к вкладкам",
                    "go down to the lists and entries / back up to the tabs"
                ),
            ),
            (
                tr!("←→ (в списках) / [ ]", "←→ (in the lists) / [ ]"),
                tr!("список: белый, операторы, баны, IP", "list: whitelist, ops, bans, IP bans"),
            ),
            ("a / Enter", tr!("добавить", "add")),
            ("d / Del", tr!("убрать из списка", "remove from the list")),
            ("w", tr!("включить / выключить белый список", "turn the whitelist on / off")),
            ("u", tr!("перечитать", "reload")),
        ],
        Tab::Plugins => vec![
            ("i", tr!("найти и установить (Modrinth, Hangar)", "find & install (Modrinth, Hangar)")),
            (tr!("Пробел", "Space"), tr!("включить / выключить плагин", "enable / disable the plugin")),
            ("c", tr!("проверить обновления", "check for updates")),
            ("Enter / U", tr!("обновить выбранный / все", "update the selected one / all")),
            ("u", tr!("перечитать папку", "rescan the folder")),
        ],
        Tab::Settings => vec![
            ("Enter", tr!("изменить значение", "edit the value")),
            (tr!("Пробел", "Space"), tr!("да/нет и варианты из списка", "yes/no and listed choices")),
            ("r", tr!("включить RCON (TPS, точный список игроков)", "enable RCON (TPS, exact player list)")),
            ("u", tr!("перечитать server.properties", "reload server.properties")),
        ],
        Tab::Backups => vec![
            ("n / Ctrl+B", tr!("сделать бэкап сейчас", "make a backup now")),
            ("r", tr!("восстановить выбранный", "restore the selected one")),
            ("d", tr!("удалить выбранный", "delete the selected one")),
            ("u", tr!("обновить список", "refresh the list")),
        ],
        Tab::World => vec![
            ("Enter", tr!("сменить основной мир / генерация чанков", "switch the main world / pregenerate")),
            ("R", tr!("сбросить Незер или Энд", "reset the Nether or the End")),
            ("p", tr!("сгенерировать чанки (Chunky)", "pregenerate chunks (Chunky)")),
            ("P / C", tr!("пауза / продолжить генерацию", "pause / continue pregeneration")),
            ("x", tr!("отменить генерацию", "cancel pregeneration")),
            ("u", tr!("пересчитать размеры", "rescan sizes")),
        ],
    }
}

fn logs_keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("↑↓ PgUp PgDn", tr!("прокрутка", "scroll")),
        ("g / G", tr!("в начало / в конец", "to the start / to the end")),
        ("←→", tr!("сдвиг длинных строк", "scroll long lines sideways")),
        ("/", tr!("поиск в открытом логе", "search the open log")),
        ("n / N", tr!("совпадение выше / ниже", "previous / next match")),
        ("F", tr!("поиск по всем логам и crash-reports", "search all logs and crash reports")),
        ("f", tr!("фильтр: всё / предупреждения / ошибки", "filter: all / warnings / errors")),
        ("c", tr!("чат игроков", "player chat")),
        ("m / Enter", tr!("написать в чат (say)", "write to chat (say)")),
        ("Tab", tr!("список источников: живой лог, файлы, архивы", "sources: live log, files, archives")),
        ("r", tr!("перечитать источники", "rescan sources")),
        ("Esc / L", tr!("назад к серверам", "back to the servers")),
    ]
}

fn input_keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Enter", tr!("отправить команду", "send the command")),
        ("Tab", tr!("дополнить команду или ник", "complete a command or a name")),
        ("↑↓", tr!("история команд", "command history")),
        ("Esc", tr!("выйти из строки", "leave the input")),
    ]
}

fn main_keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("↑↓", tr!("выбрать сервер (или строку во вкладке)", "select a server (or a row in the tab)")),
        ("Tab / Shift+Tab", tr!("фокус: серверы → вкладка → команда", "focus: servers → tab → command")),
        ("1–7 / ←→", tr!("вкладки", "tabs")),
        ("Enter / :", tr!("ввести команду серверу", "type a server command")),
        ("a", tr!("добавить сервер (в списке серверов)", "add a server (in the server list)")),
        ("Del", tr!("убрать сервер из pechka (файлы остаются)", "remove the server from pechka (files stay)")),
        ("L", tr!("логи, чат, crash-reports", "logs, chat, crash reports")),
        ("f", tr!("как друзьям зайти", "how friends join")),
    ]
}

fn global_keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Ctrl+S", tr!("запустить сервер", "start the server")),
        ("Ctrl+X", tr!("остановить (повторно — kill)", "stop (again — kill)")),
        ("Ctrl+R", tr!("перезапустить", "restart")),
        ("Ctrl+B", tr!("бэкап сейчас", "backup now")),
        ("Ctrl+L", tr!("логи", "logs")),
        ("Ctrl+U", tr!("обновить pechka", "update pechka")),
        ("? / F1", tr!("эта справка", "this help")),
        ("Ctrl+Q", tr!("выход (серверы продолжат работать)", "quit (servers keep running)")),
    ]
}

impl App {
    /// Разделы справки для текущего места: оно первым.
    pub fn help_sections(&self) -> Vec<HelpSection> {
        let mut v = vec![];
        match (self.screen, self.focus) {
            (Screen::Logs, _) => v.push(section(tr!("Логи", "Logs"), logs_keys())),
            (_, Focus::Input) => v.push(section(tr!("Строка команды", "Command line"), input_keys())),
            (_, Focus::Content) => v.push(section(
                tr!(format!("Вкладка «{}»", self.tab.title()), format!("{} tab", self.tab.title())),
                tab_keys(self.tab),
            )),
            (_, Focus::Sidebar) => {}
        }
        v.push(section(tr!("Главный экран", "Main screen"), main_keys()));
        v.push(section(tr!("Везде", "Everywhere"), global_keys()));
        v.push(section(
            tr!("Подсказка", "Tip"),
            vec![(
                tr!("Раскладка", "Layout"),
                tr!("хоткеи работают и в русской раскладке", "hotkeys also work in the Russian layout"),
            )],
        ));
        v
    }

    pub(super) fn open_help(&mut self) {
        self.popup = Some(Popup::Help { scroll: 0 });
    }

    pub(super) fn help_key(&mut self, scroll: u16, k: KeyEvent) {
        let scroll = match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) => return,
            KeyCode::Char(c) if matches!(latin(c), '?' | 'q') || c == ',' => return,
            KeyCode::Up => scroll.saturating_sub(1),
            KeyCode::Down => scroll.saturating_add(1),
            KeyCode::PageUp => scroll.saturating_sub(10),
            KeyCode::PageDown => scroll.saturating_add(10),
            KeyCode::Home => 0,
            _ => scroll,
        };
        self.popup = Some(Popup::Help { scroll });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};

    #[test]
    fn every_tab_has_keys_in_both_languages() {
        for lang in [Lang::Ru, Lang::En] {
            with_lang(lang, || {
                for t in TABS {
                    assert!(!tab_keys(t).is_empty(), "{t:?}");
                }
                for (k, d) in [logs_keys(), input_keys(), main_keys(), global_keys()].concat() {
                    assert!(!k.is_empty() && !d.is_empty());
                }
            });
        }
    }
}
