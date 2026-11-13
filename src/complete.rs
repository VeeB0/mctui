//! Автодополнение команд консоли по Tab: имя команды, подкоманды и ники игроков онлайн.

/// Команды ванильного сервера и самых частых ядер/плагинов (Paper, Purpur, spark, EssentialsX).
const COMMANDS: &[&str] = &[
    "advancement",
    "attribute",
    "ban",
    "ban-ip",
    "banlist",
    "bossbar",
    "clear",
    "clone",
    "damage",
    "data",
    "datapack",
    "debug",
    "defaultgamemode",
    "deop",
    "difficulty",
    "effect",
    "enchant",
    "execute",
    "experience",
    "fill",
    "fillbiome",
    "forceload",
    "function",
    "gamemode",
    "gamerule",
    "give",
    "help",
    "item",
    "jfr",
    "kick",
    "kill",
    "list",
    "locate",
    "loot",
    "me",
    "msg",
    "op",
    "pardon",
    "pardon-ip",
    "particle",
    "perf",
    "place",
    "playsound",
    "plugins",
    "random",
    "recipe",
    "reload",
    "return",
    "ride",
    "rotate",
    "save-all",
    "save-off",
    "save-on",
    "say",
    "schedule",
    "scoreboard",
    "seed",
    "setblock",
    "setidletimeout",
    "setworldspawn",
    "spawnpoint",
    "spectate",
    "spreadplayers",
    "stop",
    "stopsound",
    "summon",
    "tag",
    "team",
    "teammsg",
    "teleport",
    "tell",
    "tellraw",
    "tick",
    "time",
    "title",
    "tm",
    "tp",
    "transfer",
    "trigger",
    "version",
    "w",
    "weather",
    "whitelist",
    "worldborder",
    "xp",
    // Paper / Purpur / Spigot
    "mspt",
    "paper",
    "purpur",
    "spark",
    "timings",
    "tps",
    "restart",
];

/// Подкоманды и ключевые слова: (команда, позиция аргумента с 1, варианты).
const ARGS: &[(&str, usize, &[&str])] = &[
    ("whitelist", 1, &["add", "remove", "list", "on", "off", "reload"]),
    ("gamemode", 1, &["survival", "creative", "adventure", "spectator"]),
    ("defaultgamemode", 1, &["survival", "creative", "adventure", "spectator"]),
    ("difficulty", 1, &["peaceful", "easy", "normal", "hard"]),
    ("weather", 1, &["clear", "rain", "thunder"]),
    ("time", 1, &["set", "add", "query"]),
    ("time", 2, &["day", "night", "noon", "midnight", "daytime", "gametime"]),
    ("banlist", 1, &["players", "ips"]),
    ("save-all", 1, &["flush"]),
    ("tick", 1, &["query", "rate", "freeze", "unfreeze", "step", "sprint"]),
    ("experience", 1, &["add", "set", "query"]),
    ("xp", 1, &["add", "set", "query"]),
    ("worldborder", 1, &["add", "center", "damage", "get", "set", "warning"]),
    ("datapack", 1, &["list", "enable", "disable"]),
    ("forceload", 1, &["add", "remove", "query"]),
    ("spark", 1, &["profiler", "tps", "health", "gc", "heapsummary"]),
    ("paper", 1, &["version", "heap", "entity", "reload", "dumpplugins"]),
];

/// Команды без ников в аргументах.
const NO_PLAYERS: &[&str] = &[
    "say",
    "me",
    "list",
    "stop",
    "restart",
    "save-all",
    "save-on",
    "save-off",
    "reload",
    "difficulty",
    "weather",
    "time",
    "seed",
    "plugins",
    "version",
    "tps",
    "mspt",
    "help",
    "datapack",
    "forceload",
    "worldborder",
    "banlist",
    "defaultgamemode",
    "tick",
    "spark",
    "paper",
    "purpur",
    "timings",
    "perf",
    "jfr",
    "debug",
    "gamerule",
    "function",
    "setidletimeout",
    "ban-ip",
    "pardon-ip",
    "setworldspawn",
];

/// Команды, где ник игрока — второй аргумент (после подкоманды).
const PLAYER_SECOND: &[&str] = &["whitelist", "gamemode", "experience", "xp", "effect", "enchant"];

/// Варианты для последнего слова строки `input`. Возвращает (начало слова в байтах, варианты).
pub fn candidates(input: &str, players: &[String]) -> (usize, Vec<String>) {
    let start = input.rfind(' ').map(|i| i + 1).unwrap_or(0);
    let word = &input[start..];
    let lower = word.to_lowercase();
    let words: Vec<&str> = input[..start].split_whitespace().collect();
    let mut out: Vec<String> = vec![];
    let push = |out: &mut Vec<String>, s: &str| {
        if s.to_lowercase().starts_with(&lower) && !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    };
    if words.is_empty() {
        let w = lower.trim_start_matches('/');
        for c in COMMANDS {
            if c.starts_with(w) {
                out.push(c.to_string());
            }
        }
        return (start, out);
    }
    let cmd = words[0].trim_start_matches('/').to_lowercase();
    let pos = words.len();
    for (c, p, opts) in ARGS {
        if *c == cmd && *p == pos {
            for o in *opts {
                push(&mut out, o);
            }
        }
    }
    let c = cmd.as_str();
    let wants_players = match pos {
        1 => !NO_PLAYERS.contains(&c) && !PLAYER_SECOND.contains(&c),
        2 => PLAYER_SECOND.contains(&c) || matches!(c, "tp" | "teleport"),
        _ => false,
    };
    if wants_players {
        if word.starts_with('@') {
            for sel in ["@a", "@p", "@r", "@s", "@e"] {
                push(&mut out, sel);
            }
        } else {
            for p in players {
                push(&mut out, p);
            }
        }
    }
    (start, out)
}

/// Общий префикс вариантов (без учёта регистра длина считается по символам первого).
pub fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else { return String::new() };
    let mut n = first.chars().count();
    for it in &items[1..] {
        n = n.min(first.chars().zip(it.chars()).take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase())).count());
    }
    first.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn players() -> Vec<String> {
        vec!["Steve".into(), "Alex".into(), "steve2".into()]
    }

    #[test]
    fn commands_and_players() {
        let (s, c) = candidates("wea", &players());
        assert_eq!((s, c), (0, vec!["weather".to_string()]));
        let (s, c) = candidates("kick st", &players());
        assert_eq!(s, 5);
        assert_eq!(c, vec!["Steve".to_string(), "steve2".to_string()]);
        assert_eq!(common_prefix(&c), "Steve");
        let (_, c) = candidates("whitelist a", &players());
        assert_eq!(c, vec!["add".to_string()]);
        let (_, c) = candidates("whitelist add A", &players());
        assert_eq!(c, vec!["Alex".to_string()]);
        let (_, c) = candidates("time set n", &players());
        assert_eq!(c, vec!["night".to_string(), "noon".to_string()]);
        let (_, c) = candidates("say he", &players());
        assert!(c.is_empty());
        let (_, c) = candidates("op @", &players());
        assert_eq!(c.len(), 5);
        let (_, c) = candidates("/sto", &players());
        assert_eq!(c, vec!["stop".to_string(), "stopsound".to_string()]);
    }
}
