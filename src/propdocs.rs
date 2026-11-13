//! Справочник по ключам server.properties: короткое двуязычное описание, тип значения,
//! когда изменение вступает в силу. Плюс проверка значений и переключение вариантов (Пробел).
//!
//! Покрывает ванильный server.properties 1.21+/26.x и несколько устаревших ключей, которые ещё
//! встречаются в файлах серверов старых версий (pvp, spawn-monsters…).

use crate::tr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    /// Фиксированный набор значений (первое — значение по умолчанию у Mojang не обязательно).
    Enum(&'static [&'static str]),
    String,
    /// TCP/UDP-порт 1–65534.
    Port,
}

/// Когда изменение начинает действовать.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// После перезапуска сервера (почти все ключи).
    Restart,
    /// Только для нового мира: на существующий мир не влияет (сид, тип мира, хардкор…).
    NewWorld,
}

#[derive(Debug)]
pub struct PropDoc {
    pub key: &'static str,
    pub ru: &'static str,
    pub en: &'static str,
    pub kind: Kind,
    pub effect: Effect,
    /// Показывать в начале списка настроек.
    pub important: bool,
    /// Секрет (пароль/токен): в интерфейсе лучше маскировать.
    pub secret: bool,
    /// Ключ из старых версий (в новых удалён или стал gamerule).
    pub legacy: bool,
}

impl PropDoc {
    /// Описание на текущем языке интерфейса.
    pub fn description(&self) -> &'static str {
        tr!(self.ru, self.en)
    }
}

const DIFFICULTY: &[&str] = &["peaceful", "easy", "normal", "hard"];
const GAMEMODE: &[&str] = &["survival", "creative", "adventure", "spectator"];
const LEVEL_TYPE: &[&str] = &[
    "minecraft:normal",
    "minecraft:flat",
    "minecraft:large_biomes",
    "minecraft:amplified",
    "minecraft:single_biome_surface",
];
const COMPRESSION: &[&str] = &["deflate", "lz4", "none"];

const I32MAX: i64 = i32::MAX as i64;

use Effect::{NewWorld, Restart};
use Kind::{Bool, Enum, Int, Port, String as Str};

macro_rules! doc {
    ($key:literal, $kind:expr, $ru:literal, $en:literal $(, $flag:ident)*) => {{
        #[allow(unused_mut)]
        let mut d = PropDoc {
            key: $key,
            ru: $ru,
            en: $en,
            kind: $kind,
            effect: Restart,
            important: false,
            secret: false,
            legacy: false,
        };
        $( doc!(@flag d, $flag); )*
        d
    }};
    (@flag $d:ident, important) => { $d.important = true; };
    (@flag $d:ident, secret) => { $d.secret = true; };
    (@flag $d:ident, legacy) => { $d.legacy = true; };
    (@flag $d:ident, new_world) => { $d.effect = NewWorld; };
}

/// Все известные ключи. Порядок: сначала важные (в порядке показа), затем остальные по алфавиту.
pub static DOCS: &[PropDoc] = &[
    // ── Важные ─────────────────────────────────────────────────────────────────────────────
    doc!(
        "motd",
        Str,
        "Описание сервера в списке серверов (до ~59 символов)",
        "Message shown in the server list (keep it under ~59 chars)",
        important
    ),
    doc!("server-port", Port, "Порт сервера (TCP), по умолчанию 25565", "Server port (TCP), default 25565", important),
    doc!(
        "max-players",
        Int { min: 0, max: I32MAX },
        "Максимум игроков одновременно",
        "Maximum number of players online at once",
        important
    ),
    doc!(
        "online-mode",
        Bool,
        "Проверять лицензию Minecraft; false — можно войти под любым ником",
        "Verify Minecraft accounts; false lets anyone join under any name",
        important
    ),
    doc!(
        "white-list",
        Bool,
        "Белый список: входить могут только игроки из whitelist.json",
        "Whitelist: only players listed in whitelist.json can join",
        important
    ),
    doc!(
        "difficulty",
        Enum(DIFFICULTY),
        "Сложность: peaceful, easy, normal, hard",
        "Difficulty: peaceful, easy, normal, hard",
        important
    ),
    doc!(
        "gamemode",
        Enum(GAMEMODE),
        "Режим игры по умолчанию для новых игроков",
        "Default game mode for new players",
        important
    ),
    doc!(
        "hardcore",
        Bool,
        "Хардкор: после смерти — режим наблюдателя (только для нового мира)",
        "Hardcore: spectator mode after death (new worlds only)",
        important,
        new_world
    ),
    doc!(
        "pvp",
        Bool,
        "Урон между игроками (в 1.21.9+ — gamerule pvp)",
        "Players can damage each other (1.21.9+: gamerule pvp)",
        important,
        legacy
    ),
    doc!(
        "view-distance",
        Int { min: 2, max: 32 },
        "Дальность прорисовки в чанках (2–32); больше — тяжелее серверу",
        "View distance in chunks (2–32); higher costs more CPU/RAM",
        important
    ),
    doc!(
        "simulation-distance",
        Int { min: 2, max: 32 },
        "Радиус в чанках, где живут мобы и растения (2–32)",
        "Radius in chunks where entities and crops are ticked (2–32)",
        important
    ),
    doc!(
        "level-name",
        Str,
        "Имя папки мира; новое имя — новый мир",
        "World folder name; a new name creates a new world",
        important
    ),
    doc!(
        "level-seed",
        Str,
        "Сид мира; пусто — случайный (только для нового мира)",
        "World seed; empty means random (new worlds only)",
        important,
        new_world
    ),
    doc!(
        "spawn-protection",
        Int { min: 0, max: I32MAX },
        "Радиус защиты спавна от не-операторов; 0 — выкл.",
        "Spawn protection radius for non-ops; 0 disables it",
        important
    ),
    doc!(
        "allow-flight",
        Bool,
        "Не кикать за полёт в выживании (нужно для модов/плагинов на полёт)",
        "Do not kick for flying in survival (needed for flight mods)",
        important
    ),
    doc!(
        "enable-rcon",
        Bool,
        "Удалённая консоль RCON (pechka берёт по ней TPS и игроков)",
        "Remote console RCON (pechka uses it for TPS and players)",
        important
    ),
    doc!(
        "server-ip",
        Str,
        "Адрес для прослушивания; оставьте пустым (все интерфейсы)",
        "Address to bind to; leave empty to listen on all interfaces",
        important
    ),
    // ── Остальные (по алфавиту) ────────────────────────────────────────────────────────────
    doc!(
        "accepts-transfers",
        Bool,
        "Принимать игроков, переданных с другого сервера (/transfer)",
        "Accept players transferred from another server (/transfer)"
    ),
    doc!(
        "allow-nether",
        Bool,
        "Разрешить Незер (в 1.21.9+ — gamerule)",
        "Allow the Nether (1.21.9+: a game rule)",
        legacy
    ),
    doc!(
        "allowed-connection-ids",
        Str,
        "ID подключений через запятую, которым разрешён вход; пусто — всем",
        "Comma-separated connection IDs allowed to join; empty allows all"
    ),
    doc!(
        "broadcast-console-to-ops",
        Bool,
        "Показывать операторам в чате вывод команд из консоли",
        "Show console command output to online operators"
    ),
    doc!(
        "broadcast-rcon-to-ops",
        Bool,
        "Показывать операторам в чате вывод команд RCON",
        "Show RCON command output to online operators"
    ),
    doc!(
        "bug-report-link",
        Str,
        "Ссылка «Сообщить об ошибке» в меню клиента; пусто — нет",
        "\"Report bug\" link shown to clients; empty means none"
    ),
    doc!(
        "chat-spam-threshold-seconds",
        Int { min: 0, max: I32MAX },
        "Порог антиспама чата в секундах; 0 — выкл.",
        "Chat spam kick threshold in seconds; 0 disables it"
    ),
    doc!(
        "command-spam-threshold-seconds",
        Int { min: 0, max: I32MAX },
        "Порог антиспама команд в секундах; 0 — выкл.",
        "Command spam kick threshold in seconds; 0 disables it"
    ),
    doc!("debug", Bool, "Режим отладки (подробные логи)", "Debug mode (more verbose logging)"),
    doc!(
        "enable-code-of-conduct",
        Bool,
        "Показывать правила сервера из папки codeofconduct",
        "Show the code of conduct from the codeofconduct folder"
    ),
    doc!(
        "enable-command-block",
        Bool,
        "Командные блоки (в 1.21.9+ — gamerule)",
        "Enable command blocks (1.21.9+: a game rule)",
        legacy
    ),
    doc!(
        "enable-jmx-monitoring",
        Bool,
        "JMX-метрики времени тика (нужны флаги JVM)",
        "Expose tick time via JMX (needs JVM flags)"
    ),
    doc!(
        "enable-legacy-status",
        Bool,
        "Отвечать на запросы статуса клиентов до 1.7",
        "Answer status requests from pre-1.7 clients"
    ),
    doc!(
        "enable-query",
        Bool,
        "Протокол Query (UDP) для мониторингов серверов",
        "Query protocol (UDP) for server list sites"
    ),
    doc!(
        "enable-status",
        Bool,
        "Показывать сервер онлайн в списке серверов",
        "Show the server as online in the server list"
    ),
    doc!("enforce-secure-profile", Bool, "Требовать подписанные Mojang ключи чата", "Require Mojang-signed chat keys"),
    doc!(
        "enforce-whitelist",
        Bool,
        "Кикать игроков не из белого списка при его перезагрузке",
        "Kick non-whitelisted players when the whitelist reloads"
    ),
    doc!(
        "entity-broadcast-range-percentage",
        Int { min: 10, max: 1000 },
        "Дальность видимости сущностей в процентах (10–1000)",
        "Entity visibility range in percent (10–1000)"
    ),
    doc!(
        "force-gamemode",
        Bool,
        "При входе всегда ставить режим игры по умолчанию",
        "Force the default game mode on every join"
    ),
    doc!(
        "function-permission-level",
        Int { min: 1, max: 4 },
        "Уровень прав для функций датапаков (1–4)",
        "Permission level for datapack functions (1–4)"
    ),
    doc!(
        "generate-structures",
        Bool,
        "Генерировать деревни, храмы и т. п. (только для нового мира)",
        "Generate villages, temples, etc. (new worlds only)",
        new_world
    ),
    doc!(
        "generator-settings",
        Str,
        "JSON-настройки генерации (для flat и single_biome_surface)",
        "JSON world generation settings (for flat and single biome)",
        new_world
    ),
    doc!(
        "hide-online-players",
        Bool,
        "Скрывать список игроков в ответе статуса",
        "Hide the player list in the server status"
    ),
    doc!(
        "initial-disabled-packs",
        Str,
        "Датапаки, не включаемые при создании мира (через запятую)",
        "Datapacks not enabled when the world is created (comma list)",
        new_world
    ),
    doc!(
        "initial-enabled-packs",
        Str,
        "Датапаки, включаемые при создании мира (через запятую)",
        "Datapacks enabled when the world is created (comma list)",
        new_world
    ),
    doc!(
        "level-type",
        Enum(LEVEL_TYPE),
        "Тип мира: normal, flat, large_biomes, amplified… (новый мир)",
        "World type: normal, flat, large_biomes, amplified… (new worlds)",
        new_world
    ),
    doc!("log-ips", Bool, "Писать IP игроков в лог", "Write player IP addresses to the log"),
    doc!(
        "management-server-allowed-origins",
        Str,
        "Разрешённые Origin для протокола управления (через запятую)",
        "Allowed origins for the management protocol (comma list)"
    ),
    doc!(
        "management-server-enabled",
        Bool,
        "Протокол управления сервером (WebSocket API)",
        "Server management protocol (WebSocket API)"
    ),
    doc!(
        "management-server-host",
        Str,
        "Адрес протокола управления (по умолчанию localhost)",
        "Management protocol bind address (default localhost)"
    ),
    doc!(
        "management-server-port",
        Int { min: 0, max: 65535 },
        "Порт протокола управления; 0 — выбрать автоматически",
        "Management protocol port; 0 picks one automatically"
    ),
    doc!(
        "management-server-secret",
        Str,
        "Секрет протокола управления (40 символов); пусто — создать",
        "Management protocol secret (40 chars); empty generates one",
        secret
    ),
    doc!("management-server-tls-enabled", Bool, "TLS для протокола управления", "Use TLS for the management protocol"),
    doc!("management-server-tls-keystore", Str, "Путь к хранилищу ключей TLS", "Path to the TLS keystore"),
    doc!(
        "management-server-tls-keystore-password",
        Str,
        "Пароль хранилища ключей TLS",
        "TLS keystore password",
        secret
    ),
    doc!(
        "max-build-height",
        Int { min: 1, max: 256 },
        "Максимальная высота строительства (до 1.17)",
        "Maximum build height (before 1.17)",
        legacy
    ),
    doc!(
        "max-chained-neighbor-updates",
        Int { min: i32::MIN as i64, max: I32MAX },
        "Лимит цепочки обновлений блоков; отрицательное — без лимита",
        "Limit on chained block updates; negative means no limit"
    ),
    doc!(
        "max-tick-time",
        Int { min: -1, max: i64::MAX },
        "Watchdog: тик дольше (мс) — аварийная остановка; -1 — выкл.",
        "Watchdog: stop the server if a tick takes longer (ms); -1 off"
    ),
    doc!(
        "max-world-size",
        Int { min: 1, max: 29_999_984 },
        "Радиус границы мира в блоках",
        "World border radius in blocks"
    ),
    doc!(
        "network-compression-threshold",
        Int { min: -1, max: I32MAX },
        "Сжимать пакеты больше N байт; -1 — не сжимать",
        "Compress packets larger than N bytes; -1 disables compression"
    ),
    doc!(
        "op-permission-level",
        Int { min: 0, max: 4 },
        "Уровень прав, выдаваемый командой /op (0–4)",
        "Permission level granted by /op (0–4)"
    ),
    doc!(
        "pause-when-empty-seconds",
        Int { min: -1, max: I32MAX },
        "Ставить сервер на паузу через N секунд без игроков; ≤0 — нет",
        "Pause the server after N seconds with no players; ≤0 never"
    ),
    doc!(
        "player-idle-timeout",
        Int { min: 0, max: I32MAX },
        "Кикать бездействующих через N минут; 0 — никогда",
        "Kick idle players after N minutes; 0 never"
    ),
    doc!(
        "prevent-proxy-connections",
        Bool,
        "Кикать, если провайдер игрока не совпадает с сервером авторизации",
        "Kick players whose ISP differs from the auth server's view"
    ),
    doc!("query.port", Port, "UDP-порт протокола Query", "UDP port for the Query protocol"),
    doc!(
        "rate-limit",
        Int { min: 0, max: I32MAX },
        "Лимит пакетов в секунду на игрока; 0 — без лимита",
        "Packets per second per player before kick; 0 disables it"
    ),
    doc!(
        "rcon.password",
        Str,
        "Пароль RCON; без пароля RCON не запустится",
        "RCON password; RCON will not start without one",
        secret
    ),
    doc!(
        "rcon.port",
        Port,
        "TCP-порт RCON (не открывайте его в интернет)",
        "TCP port for RCON (do not expose it to the internet)"
    ),
    doc!(
        "region-file-compression",
        Enum(COMPRESSION),
        "Сжатие файлов мира: deflate, lz4 (быстрее) или none",
        "World file compression: deflate, lz4 (faster) or none"
    ),
    doc!(
        "require-resource-pack",
        Bool,
        "Кикать игроков, отказавшихся от ресурспака",
        "Kick players who decline the resource pack"
    ),
    doc!(
        "resource-pack",
        Str,
        "Ссылка на ресурспак сервера (zip, до 250 МиБ)",
        "URL of the server resource pack (zip, up to 250 MiB)"
    ),
    doc!(
        "resource-pack-id",
        Str,
        "UUID ресурспака (для кэша у клиентов)",
        "Resource pack UUID (used by client caching)"
    ),
    doc!(
        "resource-pack-prompt",
        Str,
        "Текст запроса на установку ресурспака",
        "Custom text of the resource pack prompt"
    ),
    doc!(
        "resource-pack-sha1",
        Str,
        "SHA-1 ресурспака (40 hex-символов) для проверки",
        "SHA-1 of the resource pack (40 hex chars) for verification"
    ),
    doc!("server-name", Str, "Имя сервера (используется некоторыми плагинами)", "Server name (used by some plugins)"),
    doc!(
        "snooper-enabled",
        Bool,
        "Отправка статистики Mojang (до 1.18)",
        "Send usage statistics to Mojang (before 1.18)",
        legacy
    ),
    doc!("spawn-animals", Bool, "Спавн животных (до 1.21.2)", "Spawn animals (before 1.21.2)", legacy),
    doc!(
        "spawn-monsters",
        Bool,
        "Спавн монстров (в 1.21.9+ — gamerule)",
        "Spawn monsters (1.21.9+: a game rule)",
        legacy
    ),
    doc!("spawn-npcs", Bool, "Спавн жителей (до 1.21.2)", "Spawn villagers (before 1.21.2)", legacy),
    doc!(
        "status-contact-details",
        Str,
        "Контакты владельца сервера (клиентам не показываются)",
        "Server owner contact details (not shown to players)"
    ),
    doc!(
        "status-heartbeat-interval",
        Int { min: 0, max: I32MAX },
        "Интервал heartbeat протокола управления (сек); 0 — выкл.",
        "Management protocol heartbeat interval (s); 0 disables it"
    ),
    doc!(
        "sync-chunk-writes",
        Bool,
        "Синхронная запись чанков: надёжнее при сбоях, но медленнее",
        "Synchronous chunk writes: safer on crashes, but slower"
    ),
    doc!(
        "text-filtering-config",
        Str,
        "Настройки фильтра чата (для Realms)",
        "Chat text filter config (used by Realms)"
    ),
    doc!(
        "text-filtering-version",
        Int { min: 0, max: 1 },
        "Версия формата фильтра чата (0 или 1)",
        "Chat text filter config version (0 or 1)"
    ),
    doc!(
        "use-native-transport",
        Bool,
        "Быстрый сетевой ввод-вывод Linux (epoll)",
        "Optimized Linux network I/O (epoll)"
    ),
];

/// Описание ключа.
pub fn doc(key: &str) -> Option<&'static PropDoc> {
    DOCS.iter().find(|d| d.key == key)
}

/// Ключ для сортировки списка настроек: важные — в порядке таблицы, остальные — по алфавиту,
/// неизвестные — в конце.
pub fn sort_rank(key: &str) -> (u8, usize) {
    match DOCS.iter().position(|d| d.key == key) {
        Some(i) if DOCS[i].important => (0, i),
        Some(i) => (1, i),
        None => (2, 0),
    }
}

/// Значение из файла без экранирования `.properties` (`minecraft\:normal` → `minecraft:normal`).
pub fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(n) => out.push(n),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Индекс значения в перечислении: без учёта регистра, `minecraft:` можно опускать,
/// у difficulty/gamemode допустимы старые числовые значения 0–3.
fn enum_index(key: &str, values: &[&str], v: &str) -> Option<usize> {
    let v = v.trim().to_ascii_lowercase();
    let bare = |s: &str| s.strip_prefix("minecraft:").unwrap_or(s).to_string();
    if let Some(i) = values.iter().position(|x| bare(x) == bare(&v)) {
        return Some(i);
    }
    if matches!(key, "difficulty" | "gamemode")
        && let Ok(n) = v.parse::<usize>()
        && n < values.len()
    {
        return Some(n);
    }
    // Старые имена типов мира (до 1.19).
    if key == "level-type" {
        return match v.as_str() {
            "default" => Some(0),
            "largebiomes" => Some(2),
            _ => None,
        };
    }
    None
}

/// Проверка значения. Неизвестные ключи и пустые строки у строковых ключей — всегда Ok.
pub fn validate(key: &str, value: &str) -> Result<(), String> {
    let Some(d) = doc(key) else {
        return Ok(());
    };
    let raw = unescape(value);
    let v = raw.trim();
    match d.kind {
        Kind::Bool => match parse_bool(v) {
            Some(_) => Ok(()),
            None => Err(tr!("нужно true или false".to_string(), "must be true or false".to_string())),
        },
        Kind::Int { min, max } => match v.parse::<i64>() {
            Ok(n) if (min..=max).contains(&n) => Ok(()),
            Ok(_) => Err(tr!(format!("число от {min} до {max}"), format!("must be between {min} and {max}"))),
            Err(_) => Err(tr!("нужно целое число".to_string(), "must be a whole number".to_string())),
        },
        Kind::Port => match v.parse::<u32>() {
            Ok(n) if (1..=65534).contains(&n) => Ok(()),
            _ => {
                Err(tr!("порт — число от 1 до 65534".to_string(), "port must be a number from 1 to 65534".to_string()))
            }
        },
        Kind::Enum(values) => {
            if enum_index(key, values, v).is_some() {
                return Ok(());
            }
            // Типы мира из модов/датапаков: `namespace:id`.
            if key == "level-type" && is_resource_id(v) && !v.to_ascii_lowercase().starts_with("minecraft:") {
                return Ok(());
            }
            let list = values.join(", ");
            Err(tr!(format!("допустимо: {list}"), format!("allowed: {list}")))
        }
        Kind::String => validate_string(key, v),
    }
}

fn is_resource_id(v: &str) -> bool {
    let Some((ns, path)) = v.split_once(':') else {
        return false;
    };
    let ok = |s: &str, extra: &str| {
        !s.is_empty()
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.".contains(c) || extra.contains(c))
    };
    ok(ns, "") && ok(path, "/")
}

fn validate_string(key: &str, v: &str) -> Result<(), String> {
    if v.is_empty() {
        return Ok(());
    }
    match key {
        "resource-pack-sha1" if !(v.len() == 40 && v.chars().all(|c| c.is_ascii_hexdigit())) => {
            Err(tr!("SHA-1 — 40 шестнадцатеричных символов".to_string(), "SHA-1 must be 40 hex characters".to_string()))
        }
        "resource-pack-id" if !is_uuid(v) => Err(tr!(
            "нужен UUID вида 01234567-89ab-cdef-0123-456789abcdef".to_string(),
            "must be a UUID like 01234567-89ab-cdef-0123-456789abcdef".to_string()
        )),
        "resource-pack" if !(v.starts_with("http://") || v.starts_with("https://")) => {
            Err(tr!("нужна ссылка http(s)://…".to_string(), "must be an http(s):// URL".to_string()))
        }
        "management-server-secret" if !(v.len() == 40 && v.chars().all(|c| c.is_ascii_alphanumeric())) => Err(tr!(
            "секрет — 40 латинских букв и цифр (или пусто)".to_string(),
            "secret must be 40 letters and digits (or empty)".to_string()
        )),
        "server-ip" if v.parse::<std::net::IpAddr>().is_err() && v.contains(char::is_whitespace) => {
            Err(tr!("нужен IP-адрес или пусто".to_string(), "must be an IP address or empty".to_string()))
        }
        "generator-settings" if !(v.starts_with('{') || v.starts_with('[')) => {
            Err(tr!("нужен JSON, например {}".to_string(), "must be JSON, e.g. {}".to_string()))
        }
        _ => Ok(()),
    }
}

fn is_uuid(v: &str) -> bool {
    let parts: Vec<&str> = v.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Следующее значение для Bool/Enum (переключение Пробелом). Для остальных типов — None.
/// Если в файле двоеточие было экранировано (`minecraft\:normal`), результат экранируется так же.
pub fn next_choice(key: &str, current: &str) -> Option<String> {
    let d = doc(key)?;
    match d.kind {
        Kind::Bool => Some(if parse_bool(&unescape(current)) == Some(true) { "false" } else { "true" }.to_string()),
        Kind::Enum(values) => {
            let next = match enum_index(key, values, &unescape(current)) {
                Some(i) => values[(i + 1) % values.len()],
                None => values[0],
            };
            Some(if current.contains("\\:") { next.replace(':', "\\:") } else { next.to_string() })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ключи настоящего server.properties сервера 26.x (Paper), на котором проверялся справочник.
    const REAL_KEYS: &[&str] = &[
        "accepts-transfers",
        "allow-flight",
        "broadcast-console-to-ops",
        "broadcast-rcon-to-ops",
        "bug-report-link",
        "chat-spam-threshold-seconds",
        "command-spam-threshold-seconds",
        "debug",
        "difficulty",
        "enable-code-of-conduct",
        "enable-jmx-monitoring",
        "enable-query",
        "enable-rcon",
        "enable-status",
        "enforce-secure-profile",
        "enforce-whitelist",
        "entity-broadcast-range-percentage",
        "force-gamemode",
        "function-permission-level",
        "gamemode",
        "generate-structures",
        "generator-settings",
        "hardcore",
        "hide-online-players",
        "initial-disabled-packs",
        "initial-enabled-packs",
        "level-name",
        "level-seed",
        "level-type",
        "log-ips",
        "management-server-allowed-origins",
        "management-server-enabled",
        "management-server-host",
        "management-server-port",
        "management-server-secret",
        "management-server-tls-enabled",
        "management-server-tls-keystore",
        "management-server-tls-keystore-password",
        "max-chained-neighbor-updates",
        "max-players",
        "max-tick-time",
        "max-world-size",
        "motd",
        "network-compression-threshold",
        "online-mode",
        "op-permission-level",
        "pause-when-empty-seconds",
        "player-idle-timeout",
        "prevent-proxy-connections",
        "query.port",
        "rate-limit",
        "rcon.password",
        "rcon.port",
        "region-file-compression",
        "require-resource-pack",
        "resource-pack",
        "resource-pack-id",
        "resource-pack-prompt",
        "resource-pack-sha1",
        "server-ip",
        "server-name",
        "server-port",
        "simulation-distance",
        "spawn-protection",
        "status-heartbeat-interval",
        "sync-chunk-writes",
        "text-filtering-config",
        "text-filtering-version",
        "use-native-transport",
        "view-distance",
        "white-list",
    ];

    #[test]
    fn every_known_key_has_a_doc() {
        let missing: Vec<_> = REAL_KEYS.iter().filter(|k| doc(k).is_none()).collect();
        assert!(missing.is_empty(), "no doc for {missing:?}");
    }

    #[test]
    fn real_file_keys_if_present() {
        // Настоящий файл рядом с репозиторием (в т. ч. из git worktree): только сообщаем о новых ключах.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let Some(text) =
            root.ancestors().take(6).find_map(|d| std::fs::read_to_string(d.join("server.properties")).ok())
        else {
            return;
        };
        let props = crate::properties::Properties::parse(&text);
        let mut missing = vec![];
        for (k, v) in props.pairs() {
            match doc(&k) {
                None => missing.push(k),
                Some(_) => {
                    // Значения из рабочего файла сервера должны проходить проверку.
                    assert!(validate(&k, &v).is_ok(), "{k}={v}: {:?}", validate(&k, &v));
                }
            }
        }
        if !missing.is_empty() {
            println!("server.properties keys without a doc (new version?): {missing:?}");
        }
    }

    #[test]
    fn table_is_sane() {
        let mut seen = std::collections::HashSet::new();
        for d in DOCS {
            assert!(seen.insert(d.key), "duplicate {}", d.key);
            assert!(d.ru.chars().count() <= 90, "ru too long: {}", d.key);
            assert!(d.en.chars().count() <= 90, "en too long: {}", d.key);
            assert!(!d.ru.is_empty() && !d.en.is_empty());
        }
        // Неважные ключи идут по алфавиту.
        let rest: Vec<_> = DOCS.iter().filter(|d| !d.important).map(|d| d.key).collect();
        let mut sorted = rest.clone();
        sorted.sort();
        assert_eq!(rest, sorted);
        assert!(sort_rank("motd") < sort_rank("accepts-transfers"));
        assert!(sort_rank("zzz-unknown") > sort_rank("white-list"));
    }

    #[test]
    fn validation() {
        assert!(validate("online-mode", "true").is_ok());
        assert!(validate("online-mode", "FALSE").is_ok());
        assert!(validate("online-mode", "yes").is_err());
        assert!(validate("max-players", "20").is_ok());
        assert!(validate("max-players", "-1").is_err());
        assert!(validate("max-players", "abc").is_err());
        assert!(validate("view-distance", "33").is_err());
        assert!(validate("view-distance", "12").is_ok());
        assert!(validate("op-permission-level", "5").is_err());
        assert!(validate("server-port", "25565").is_ok());
        assert!(validate("server-port", "0").is_err());
        assert!(validate("server-port", "70000").is_err());
        assert!(validate("difficulty", "Hard").is_ok());
        assert!(validate("difficulty", "2").is_ok());
        assert!(validate("difficulty", "insane").is_err());
        assert!(validate("level-type", "minecraft\\:normal").is_ok());
        assert!(validate("level-type", "flat").is_ok());
        assert!(validate("level-type", "default").is_ok());
        assert!(validate("level-type", "terralith:overworld").is_ok());
        assert!(validate("level-type", "minecraft:nope").is_err());
        assert!(validate("region-file-compression", "lz4").is_ok());
        assert!(validate("region-file-compression", "zstd").is_err());
        assert!(validate("max-tick-time", "-1").is_ok());
        assert!(validate("pause-when-empty-seconds", "-1").is_ok());
        assert!(validate("resource-pack-sha1", "").is_ok());
        assert!(validate("resource-pack-sha1", "abc").is_err());
        assert!(validate("resource-pack-sha1", &"a".repeat(40)).is_ok());
        assert!(validate("resource-pack-id", "01234567-89ab-cdef-0123-456789abcdef").is_ok());
        assert!(validate("resource-pack-id", "nope").is_err());
        assert!(validate("resource-pack", "https://example.com/p.zip").is_ok());
        assert!(validate("resource-pack", "example.com/p.zip").is_err());
        assert!(validate("motd", "anything §agoes").is_ok());
        assert!(validate("some-plugin-key", "whatever").is_ok());
        let err = validate("max-players", "x").unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn cycling() {
        assert_eq!(next_choice("online-mode", "true").as_deref(), Some("false"));
        assert_eq!(next_choice("online-mode", "false").as_deref(), Some("true"));
        assert_eq!(next_choice("online-mode", "garbage").as_deref(), Some("true"));
        assert_eq!(next_choice("difficulty", "normal").as_deref(), Some("hard"));
        assert_eq!(next_choice("difficulty", "hard").as_deref(), Some("peaceful"));
        assert_eq!(next_choice("difficulty", "1").as_deref(), Some("normal"));
        assert_eq!(next_choice("gamemode", "spectator").as_deref(), Some("survival"));
        assert_eq!(next_choice("level-type", "minecraft\\:normal").as_deref(), Some("minecraft\\:flat"));
        assert_eq!(next_choice("level-type", "minecraft:flat").as_deref(), Some("minecraft:large_biomes"));
        assert_eq!(next_choice("region-file-compression", "none").as_deref(), Some("deflate"));
        assert_eq!(next_choice("motd", "x"), None);
        assert_eq!(next_choice("max-players", "20"), None);
        assert_eq!(next_choice("unknown", "true"), None);
    }

    #[test]
    fn unescape_properties() {
        assert_eq!(unescape("minecraft\\:normal"), "minecraft:normal");
        assert_eq!(unescape("a\\\\b"), "a\\b");
        assert_eq!(unescape("plain"), "plain");
    }
}
