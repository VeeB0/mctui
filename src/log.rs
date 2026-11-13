//! Кольцевой буфер строк лога, очистка ANSI/§-кодов, определение уровня.

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warn,
    Error,
    Other,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub text: String,
    pub level: Level,
}

/// Убирает ANSI escape-последовательности и коды форматирования Minecraft (`§x`).
pub fn strip_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI: ESC [ ... final byte (0x40..=0x7e); OSC: ESC ] ... BEL/ST
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for c2 in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&c2) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(c2) = chars.next() {
                            if c2 == '\x07' {
                                break;
                            }
                            if c2 == '\x1b' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some(_) => {
                        chars.next();
                    }
                    None => {}
                }
            }
            '§' => {
                chars.next();
            }
            '\r' => {}
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Убирает приглашение консоли сервера (`> `), прилипшее к началу строки лога.
fn strip_prompt(s: &str) -> &str {
    let mut t = s;
    while let Some(rest) = t.strip_prefix('>') {
        let rest = rest.trim_start();
        if rest.starts_with('[') || rest.starts_with('>') {
            t = rest;
        } else {
            break;
        }
    }
    t
}

/// Определяет уровень по префиксу вида `[12:00:00 WARN]:` или `[12:00:00] [Thread/WARN]:`.
pub fn detect_level(s: &str) -> Level {
    let head: String = s.chars().take(100).collect();
    if head.contains("ERROR]") || head.contains("FATAL]") || head.contains("SEVERE]") {
        Level::Error
    } else if head.contains("WARN]") || head.contains("WARNING]") {
        Level::Warn
    } else if head.contains("INFO]") {
        Level::Info
    } else {
        Level::Other
    }
}

pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    cap: usize,
    /// Сколько строк было вытеснено из начала (для стабильных индексов при прокрутке).
    pub dropped: u64,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self { lines: VecDeque::with_capacity(cap.min(1024)), cap, dropped: 0 }
    }

    pub fn push(&mut self, raw: &str) -> &LogLine {
        // `\r` в выводе — перерисовка строки: видимой остаётся последняя непустая часть.
        let raw = raw.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or(raw);
        let text = strip_prompt(&strip_codes(raw)).to_string();
        let mut level = detect_level(&text);
        // Строки стектрейса и продолжения наследуют уровень предыдущей строки.
        if level == Level::Other
            && let Some(prev) = self.lines.back()
        {
            let t = text.trim_start();
            if text.starts_with(' ') || t.starts_with("at ") || t.starts_with("Caused by") || t.starts_with("...") {
                level = prev.level;
            }
        }
        if self.lines.len() >= self.cap {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(LogLine { text, level });
        self.lines.back().unwrap()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &LogLine> + ExactSizeIterator {
        self.lines.iter()
    }

    pub fn to_vec(&self) -> Vec<LogLine> {
        self.lines.iter().cloned().collect()
    }
}

/// Разбор целого текста лог-файла в строки.
pub fn parse_text(text: &str, cap: usize) -> Vec<LogLine> {
    let mut buf = LogBuffer::new(cap);
    for l in text.lines() {
        buf.push(l);
    }
    buf.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_section_codes() {
        assert_eq!(strip_codes("\x1b[32mhello\x1b[0m §aworld§r"), "hello world");
        assert_eq!(strip_codes("a\x1b]0;title\x07b"), "ab");
        assert_eq!(strip_codes("line\r"), "line");
    }

    #[test]
    fn detects_levels_in_both_formats() {
        assert_eq!(detect_level("[02:03:04 INFO]: Done (11.873s)!"), Level::Info);
        assert_eq!(detect_level("[02:02:53] [ServerMain/WARN]: something"), Level::Warn);
        assert_eq!(detect_level("[02:02:53 ERROR]: boom"), Level::Error);
        assert_eq!(detect_level("Starting org.bukkit.craftbukkit.Main"), Level::Other);
    }

    #[test]
    fn prompt_and_carriage_return() {
        let mut b = LogBuffer::new(10);
        assert_eq!(b.push(">   [02:23:05 INFO]: Default game type").text, "[02:23:05 INFO]: Default game type");
        assert_eq!(b.push("> > [02:23:05 INFO]: x").text, "[02:23:05 INFO]: x");
        assert_eq!(b.push("> \r[02:23:05 INFO]: y").text, "[02:23:05 INFO]: y");
        assert_eq!(b.push("> say hi").text, "> say hi");
    }

    #[test]
    fn stacktrace_inherits_level_and_cap_works() {
        let mut b = LogBuffer::new(3);
        b.push("[00:00:00 ERROR]: Exception");
        let l = b.push("\tat foo.Bar(Bar.java:1)").level;
        assert_eq!(l, Level::Error);
        b.push("x");
        b.push("y");
        assert_eq!(b.len(), 3);
        assert_eq!(b.dropped, 1);
    }
}
