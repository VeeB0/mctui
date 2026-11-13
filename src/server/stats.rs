//! Разбор ответов `list`/`tps` и событий из строк лога.

use crate::log::strip_codes;

#[derive(Debug, PartialEq)]
pub struct ListInfo {
    pub online: u32,
    pub max: u32,
    pub players: Vec<String>,
}

/// `There are 1 of a max of 20 players online: Steve` и похожие варианты.
pub fn parse_list(resp: &str) -> Option<ListInfo> {
    let s = strip_codes(resp);
    let (head, tail) = match s.find(':') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s.as_str(), None),
    };
    let nums: Vec<u32> =
        head.split(|c: char| !c.is_ascii_digit()).filter(|t| !t.is_empty()).filter_map(|t| t.parse().ok()).collect();
    if nums.len() < 2 {
        return None;
    }
    let players = tail
        .map(|t| {
            t.split([',', '\n'])
                .map(|p| p.trim())
                // "group: name" (Essentials-подобный вывод) — берём часть после двоеточия
                .map(|p| p.rsplit(':').next().unwrap_or(p).trim())
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Some(ListInfo { online: nums[0], max: nums[1], players })
}

/// `TPS from last 1m, 5m, 15m: 20.0, *20.0, 19.9`
pub fn parse_tps(resp: &str) -> Option<[f64; 3]> {
    let s = strip_codes(resp);
    let line = s.lines().find(|l| l.contains("TPS"))?;
    let rest = match line.find("15m:") {
        Some(i) => &line[i + 4..],
        None => &line[line.rfind(':')? + 1..],
    };
    let v = numbers(rest);
    // Берём последние три значения (1m, 5m, 15m), даже если есть ещё 5s.
    let n = v.len();
    (n >= 3).then(|| [v[n - 3], v[n - 2], v[n - 1]])
}

/// Числа из строки вида `20.0, *19.5` или с десятичной запятой `20,0, 19,5`
/// (запятая с цифрой следом — дробная часть, с пробелом — разделитель).
fn numbers(s: &str) -> Vec<f64> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut num = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            num.push(chars[i]);
            i += 1;
        }
        if i + 1 < chars.len() && (chars[i] == '.' || chars[i] == ',') && chars[i + 1].is_ascii_digit() {
            num.push('.');
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                num.push(chars[i]);
                i += 1;
            }
        }
        if let Ok(v) = num.parse() {
            out.push(v);
        }
    }
    out
}

/// Текст сообщения после префикса `[..]: `.
fn message(line: &str) -> &str {
    let m = match line.find("]: ") {
        Some(i) => &line[i + 3..],
        None => line,
    };
    // Minecraft 26.x пишет системные сообщения в лог с префиксом «System chat: ».
    m.strip_prefix("System chat: ").unwrap_or(m)
}

/// Ник Java (до 16 символов `[A-Za-z0-9_]`); точка в начале — игроки Bedrock через Floodgate.
fn valid_name(n: &str) -> bool {
    (1..=17).contains(&n.len()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

#[derive(Debug, PartialEq)]
pub enum PlayerEvent {
    Joined(String),
    Left(String),
}

pub fn player_event(line: &str) -> Option<PlayerEvent> {
    let msg = message(line).trim();
    let ev = if let Some(name) = msg.strip_suffix(" joined the game") {
        PlayerEvent::Joined(name.into())
    } else if let Some(name) = msg.strip_suffix(" left the game") {
        PlayerEvent::Left(name.into())
    } else if let Some((head, _)) = msg.split_once("] logged in with entity id") {
        // «Name[/192.168.3.2:54235] logged in with entity id 29 at …»
        PlayerEvent::Joined(head.split_once("[/")?.0.into())
    } else if let Some((name, _)) = msg.split_once(" lost connection: ") {
        PlayerEvent::Left(name.into())
    } else {
        return None;
    };
    let (PlayerEvent::Joined(n) | PlayerEvent::Left(n)) = &ev;
    valid_name(n).then_some(ev)
}

pub fn is_done(line: &str) -> bool {
    let m = message(line);
    m.starts_with("Done (") && m.contains("For help")
}

pub fn is_stopping(line: &str) -> bool {
    message(line).starts_with("Stopping server")
}

pub fn version(line: &str) -> Option<String> {
    message(line).strip_prefix("Starting minecraft server version ").map(|v| v.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_variants() {
        assert_eq!(
            parse_list("There are 2 of a max of 20 players online: Steve, Alex"),
            Some(ListInfo { online: 2, max: 20, players: vec!["Steve".into(), "Alex".into()] })
        );
        assert_eq!(
            parse_list("§6There are §c0§6 out of maximum §c20§6 players online."),
            Some(ListInfo { online: 0, max: 20, players: vec![] })
        );
        assert_eq!(parse_list("There are 0 of a max of 20 players online: ").unwrap().players.len(), 0);
        assert_eq!(parse_list("Unknown command"), None);
    }

    #[test]
    fn tps_paper_format() {
        assert_eq!(parse_tps("§6TPS from last 1m, 5m, 15m: §a20.0, §a*20.0, §a19.95"), Some([20.0, 20.0, 19.95]));
        assert_eq!(
            parse_tps("TPS from last 5s, 1m, 5m, 15m: 20.0, 20.0, 20.0, 18.0\nCurrent Memory Usage: 1/2 mb"),
            Some([20.0, 20.0, 18.0])
        );
        assert_eq!(parse_tps("TPS from last 5s, 1m, 5m, 15m: 20,0, 19,5, *20,0, 18,25"), Some([19.5, 20.0, 18.25]));
        assert_eq!(parse_tps("nothing here"), None);
    }

    #[test]
    fn log_events_minecraft_26() {
        let j = |n: &str| Some(PlayerEvent::Joined(n.into()));
        let l = |n: &str| Some(PlayerEvent::Left(n.into()));
        assert_eq!(player_event("[02:46:48] [Server thread/INFO]: System chat: VeeBo_ joined the game"), j("VeeBo_"));
        assert_eq!(player_event("[02:46:48 INFO]: System chat: VeeBo_ joined the game"), j("VeeBo_"));
        assert_eq!(
            player_event(
                "[02:46:48 INFO]: VeeBo_[/192.168.3.2:54235] logged in with entity id 29 at ([minecraft:overworld]12.5, 63.0, -33.5)"
            ),
            j("VeeBo_")
        );
        assert_eq!(player_event("[02:50:00 INFO]: VeeBo_ lost connection: Disconnected"), l("VeeBo_"));
        assert_eq!(player_event("[02:50:00 INFO]: System chat: VeeBo_ left the game"), l("VeeBo_"));
        assert_eq!(player_event("[02:50:00 INFO]: System chat: There are 0 of a max of 20 players online: "), None);
        assert_eq!(player_event("[02:50:00 INFO]: <VeeBo_> он joined the game"), None);
    }

    #[test]
    fn log_events() {
        assert_eq!(player_event("[12:00:00 INFO]: Steve joined the game"), Some(PlayerEvent::Joined("Steve".into())));
        assert_eq!(
            player_event("[12:00:00] [Server thread/INFO]: Alex left the game"),
            Some(PlayerEvent::Left("Alex".into()))
        );
        assert_eq!(player_event("[12:00:00 INFO]: <Steve> I joined the game"), None);
        assert!(is_done("[02:03:04 INFO]: Done (11.873s)! For help, type \"help\""));
        assert!(!is_done("[02:03:04 INFO]: <Steve> Done (1s)!"));
        assert_eq!(version("[02:02:58 INFO]: Starting minecraft server version 26.3").as_deref(), Some("26.3"));
        assert!(is_stopping("[02:10:00 INFO]: Stopping server"));
    }
}
