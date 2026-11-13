//! Сеть серверов за прокси Velocity: настройка velocity.toml и игровых серверов за ним.
//!
//! [`link`] прописывает серверы в `[servers]` и `try` velocity.toml, включает modern forwarding
//! (`player-info-forwarding-mode = "modern"`) с общим секретом (`forwarding.secret`), а на серверах:
//! - Paper / Purpur — `config/paper-global.yml`: `proxies.velocity.enabled: true`, `online-mode: true`,
//!   `secret: '<секрет>'` (файл появляется после первого запуска; если его нет — предупреждение);
//! - Fabric — нужен мод FabricProxy-Lite: `config/FabricProxy-Lite.toml` с `secret = "<секрет>"`;
//! - Vanilla — не поддерживается (modern forwarding есть только в модах/форках);
//! - во всех поддержанных — `online-mode=false` в server.properties (авторизацию делает прокси).
//!
//! Файлы правятся построчно: комментарии, порядок ключей и остальные настройки сохраняются.
//! Серверы и прокси должны быть остановлены (вызывающая сторона), изменения вступят в силу при запуске.

use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::create::Core;
use crate::properties::Properties;
use crate::tr;

/// Имя файла настроек Velocity в папке прокси.
pub const CONFIG: &str = "velocity.toml";
const DEFAULT_SECRET_FILE: &str = "forwarding.secret";
const PAPER_GLOBAL: &str = "config/paper-global.yml";
const FABRIC_PROXY_CONFIG: &str = "config/FabricProxy-Lite.toml";

/// Игровой сервер за прокси.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    /// Имя в velocity.toml (`/server <name>`): латиница, цифры, `-`, `_` — см. [`sanitize_name`].
    pub name: String,
    pub dir: PathBuf,
    /// Порт сервера (`server-port`); прокси подключается к `127.0.0.1:<port>`.
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkReport {
    /// Что изменено (по строке на изменение, на языке интерфейса).
    pub changes: Vec<String>,
    /// Что не удалось или требует действий пользователя.
    pub warnings: Vec<String>,
    /// Записанные файлы.
    pub files: Vec<PathBuf>,
    /// Серверы, которые после перезапуска будут пускать игроков через прокси.
    pub linked: Vec<String>,
}

// ───────────────────────────── имена ─────────────────────────────

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "try"
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Имя сервера для velocity.toml из произвольного названия: `Мой сервер` → `server`, `Lobby 1` → `lobby-1`.
pub fn sanitize_name(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else if (c == '-' || c.is_whitespace() || c == '.') && !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').chars().take(32).collect::<String>();
    if out.is_empty() || out == "try" { "server".into() } else { out }
}

// ───────────────────────────── TOML построчно ─────────────────────────────

/// Строка TOML в двойных кавычках.
fn toml_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        k.to_string()
    } else {
        toml_str(k)
    }
}

fn unquote_key(k: &str) -> String {
    let k = k.trim();
    if k.len() >= 2 && ((k.starts_with('"') && k.ends_with('"')) || (k.starts_with('\'') && k.ends_with('\''))) {
        k[1..k.len() - 1].to_string()
    } else {
        k.to_string()
    }
}

/// Значимая часть строки (без комментария) и изменение глубины скобок `[]`/`{}`.
/// Строки в кавычках не разбираются на скобки; многострочные `"""` не поддерживаются (в velocity.toml их нет).
fn scan_line(line: &str) -> (&str, i32) {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' && q == '"' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '#' => return (&line[..i], depth),
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    (line, depth)
}

/// Позиция `=` вне кавычек.
fn find_eq(line: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '=') => return Some(i),
            (None, '#') => return None,
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone)]
struct Entry {
    key: String,
    /// Строки `start..end` (значение-массив может занимать несколько строк).
    start: usize,
    end: usize,
    /// Текст значения без комментариев.
    value: String,
}

impl Entry {
    fn parse_value(&self) -> Option<toml::Value> {
        toml::from_str::<toml::Table>(&format!("v = {}", self.value)).ok()?.remove("v")
    }
}

#[derive(Debug, Clone)]
struct TomlDoc {
    lines: Vec<String>,
}

impl TomlDoc {
    fn parse(text: &str) -> Self {
        Self { lines: text.lines().map(str::to_string).collect() }
    }

    fn render(&self) -> String {
        let mut s = self.lines.join("\n");
        s.push('\n');
        s
    }

    fn header(line: &str) -> Option<String> {
        let t = line.trim();
        if !t.starts_with('[') || t.starts_with("[[") {
            return None;
        }
        let (body, _) = scan_line(t);
        let inner = body.trim().strip_prefix('[')?.strip_suffix(']')?;
        Some(unquote_key(inner))
    }

    /// Строки раздела: `None` — корень (до первой таблицы); иначе после заголовка `[name]` до следующего.
    fn section(&self, name: Option<&str>) -> Option<(usize, usize)> {
        let headers: Vec<(usize, String)> =
            self.lines.iter().enumerate().filter_map(|(i, l)| Self::header(l).map(|h| (i, h))).collect();
        match name {
            None => Some((0, headers.first().map_or(self.lines.len(), |h| h.0))),
            Some(n) => {
                let pos = headers.iter().position(|(_, h)| h == n)?;
                let start = headers[pos].0 + 1;
                let end = headers.get(pos + 1).map_or(self.lines.len(), |h| h.0);
                Some((start, end))
            }
        }
    }

    fn entries(&self, (start, end): (usize, usize)) -> Vec<Entry> {
        let mut out = Vec::new();
        let mut i = start;
        while i < end {
            let line = &self.lines[i];
            let t = line.trim_start();
            let Some(eq) = (!t.is_empty() && !t.starts_with('#')).then(|| find_eq(line)).flatten() else {
                i += 1;
                continue;
            };
            let key = unquote_key(&line[..eq]);
            let first = &line[eq + 1..];
            let (part, mut depth) = scan_line(first);
            let mut value = part.trim().to_string();
            let mut j = i + 1;
            while depth > 0 && j < end {
                let (part, d) = scan_line(&self.lines[j]);
                depth += d;
                value.push('\n');
                value.push_str(part.trim());
                j += 1;
            }
            out.push(Entry { key, start: i, end: j, value });
            i = j;
        }
        out
    }

    fn get(&self, table: Option<&str>, key: &str) -> Option<toml::Value> {
        let sec = self.section(table)?;
        self.entries(sec).into_iter().find(|e| e.key == key)?.parse_value()
    }

    /// Позиция для вставки в конец раздела: перед пустыми строками, которыми он заканчивается.
    fn insert_pos(&self, (start, end): (usize, usize)) -> usize {
        let mut pos = end;
        while pos > start && self.lines[pos - 1].trim().is_empty() {
            pos -= 1;
        }
        pos
    }

    /// Ставит `key = literal` в разделе (корень — `None`). Возвращает старое значение (текстом), если было.
    fn set(&mut self, table: Option<&str>, key: &str, literal: &str) -> Option<String> {
        let sec = match self.section(table) {
            Some(s) => s,
            None => {
                self.add_table(table.unwrap_or_default());
                self.section(table).expect("table just added")
            }
        };
        let line = format!("{} = {literal}", toml_key(key));
        if let Some(e) = self.entries(sec).into_iter().find(|e| e.key == key) {
            let indent: String = self.lines[e.start].chars().take_while(|c| c.is_whitespace()).collect();
            self.lines.splice(e.start..e.end, [format!("{indent}{line}")]);
            return Some(e.value);
        }
        // В корне — после последнего ключа (перед комментариями к первой таблице); иначе в конец таблицы.
        let pos = match (table, self.entries(sec).last()) {
            (None, Some(last)) => last.end,
            _ => self.insert_pos(sec),
        };
        self.lines.insert(pos, line);
        None
    }

    fn add_table(&mut self, name: &str) {
        while self.lines.last().is_some_and(|l| l.trim().is_empty()) {
            self.lines.pop();
        }
        if !self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.lines.push(format!("[{}]", toml_key(name)));
    }

    /// Удаляет записи раздела, для которых `drop` вернул true. Возвращает удалённые.
    fn remove_where(&mut self, table: Option<&str>, mut drop: impl FnMut(&Entry) -> bool) -> Vec<Entry> {
        let Some(sec) = self.section(table) else { return Vec::new() };
        let gone: Vec<Entry> = self.entries(sec).into_iter().filter(|e| drop(e)).collect();
        for e in gone.iter().rev() {
            self.lines.drain(e.start..e.end);
        }
        gone
    }

    /// Заменяет все серверы в `[servers]` на `servers` и ставит `try`. Комментарии раздела остаются;
    /// серверы встают на место первого старого (или перед `try`). Возвращает удалённые записи (имя, адрес).
    fn set_servers(&mut self, servers: &[(String, String)], try_order: &[String]) -> Vec<(String, String)> {
        if self.section(Some("servers")).is_none() {
            self.add_table("servers");
        }
        let sec = self.section(Some("servers")).expect("servers table");
        let entries = self.entries(sec);
        let removed: Vec<(String, String)> = entries
            .iter()
            .filter(|e| e.key != "try")
            .map(|e| (e.key.clone(), e.parse_value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()))
            .collect();
        // Куда встанут серверы: на место первого старого; если их не было — перед комментарием к `try`.
        let mut gap_after = false;
        let mut anchor = entries.iter().find(|e| e.key != "try").map(|e| e.start).or_else(|| {
            let t = entries.iter().find(|e| e.key == "try")?;
            let mut a = t.start;
            while a > sec.0 && self.lines[a - 1].trim_start().starts_with('#') {
                a -= 1;
            }
            gap_after = true;
            Some(a)
        });
        // Убираем все записи снизу вверх, сдвигая якорь.
        for e in entries.iter().rev() {
            self.lines.drain(e.start..e.end);
            if let Some(a) = anchor.as_mut()
                && e.start < *a
            {
                *a -= e.end - e.start;
            }
        }
        let mut block: Vec<String> =
            servers.iter().map(|(n, addr)| format!("{} = {}", toml_key(n), toml_str(addr))).collect();
        if gap_after && !block.is_empty() {
            block.push(String::new());
        }
        let pos = anchor.unwrap_or_else(|| self.insert_pos(self.section(Some("servers")).expect("servers table")));
        self.lines.splice(pos..pos, block);
        // try — в конец раздела (после комментария «In what order…»).
        let try_list = try_order.iter().map(|n| format!("    {}", toml_str(n))).collect::<Vec<_>>().join(",\n");
        let try_line = if try_order.is_empty() { "try = []".to_string() } else { format!("try = [\n{try_list}\n]") };
        let end = self.insert_pos(self.section(Some("servers")).expect("servers table"));
        let mut try_lines: Vec<String> = try_line.lines().map(String::from).collect();
        if anchor.is_none() && !servers.is_empty() {
            try_lines.insert(0, String::new());
        }
        self.lines.splice(end..end, try_lines);
        removed
    }
}

// ───────────────────────────── YAML построчно (paper-global.yml) ─────────────────────────────

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Ключ строки вида `  key: value` (без списков `- …` и комментариев).
fn yaml_key(line: &str) -> Option<(usize, String, &str)> {
    let ind = indent_of(line);
    let t = &line[ind..];
    if t.is_empty() || t.starts_with('#') || t.starts_with('-') {
        return None;
    }
    let (key, rest) = if let Some(q @ ('"' | '\'')) = t.chars().next() {
        let close = t[1..].find(q)? + 1;
        (t[1..close].to_string(), &t[close + 1..])
    } else {
        let colon = t.find(':')?;
        (t[..colon].trim_end().to_string(), &t[colon..])
    };
    let rest = rest.strip_prefix(':')?;
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    Some((ind, key, rest.trim()))
}

/// Конец блока ключа на строке `idx` с отступом `ind`.
fn yaml_block_end(lines: &[String], idx: usize, ind: usize) -> usize {
    let mut j = idx + 1;
    while j < lines.len() {
        let l = &lines[j];
        if !l.trim().is_empty() {
            let li = indent_of(l);
            let list_item = li == ind && l[li..].starts_with('-');
            if li < ind || (li == ind && !list_item) {
                break;
            }
        }
        j += 1;
    }
    j
}

/// Ищет ключ `key` среди прямых потомков в строках `from..to` (отступ потомков > `parent`).
fn yaml_find(lines: &[String], from: usize, to: usize, parent: Option<usize>, key: &str) -> (Option<usize>, usize) {
    let child = lines[from..to]
        .iter()
        .filter_map(|l| yaml_key(l).map(|(i, ..)| i))
        .find(|&i| parent.is_none_or(|p| i > p))
        .unwrap_or(parent.map_or(0, |p| p + 2));
    let found = (from..to).find(|&i| yaml_key(&lines[i]).is_some_and(|(ind, k, _)| ind == child && k == key));
    (found, child)
}

fn yaml_scalar(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        v[1..v.len() - 1].replace("''", "'")
    } else if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].to_string()
    } else {
        v.split(" #").next().unwrap_or(v).trim().to_string()
    }
}

/// Значение по пути `a.b.c` (только скаляры).
fn yaml_get(text: &str, path: &[&str]) -> Option<String> {
    let lines: Vec<String> = text.lines().map(String::from).collect();
    let (mut from, mut to, mut parent) = (0, lines.len(), None);
    for (n, key) in path.iter().enumerate() {
        let (idx, child) = yaml_find(&lines, from, to, parent, key);
        let idx = idx?;
        if n + 1 == path.len() {
            return yaml_key(&lines[idx]).map(|(_, _, v)| yaml_scalar(v));
        }
        from = idx + 1;
        to = yaml_block_end(&lines, idx, child);
        parent = Some(child);
    }
    None
}

/// Ставит скаляр по пути `a.b.c` (значение — уже в синтаксисе YAML), создавая недостающие ключи.
/// Возвращает новый текст и старое значение.
fn yaml_set(text: &str, path: &[&str], value: &str) -> (String, Option<String>) {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let (mut from, mut to, mut parent) = (0, lines.len(), None);
    let mut old = None;
    for (n, key) in path.iter().enumerate() {
        let (idx, child) = yaml_find(&lines, from, to, parent, key);
        let pad = " ".repeat(child);
        let last = n + 1 == path.len();
        match idx {
            Some(idx) if last => {
                old = yaml_key(&lines[idx]).map(|(_, _, v)| yaml_scalar(v));
                lines[idx] = format!("{pad}{key}: {value}");
                break;
            }
            Some(idx) => {
                // `key: {}` или `key: ~` — заменяем на блок.
                if yaml_key(&lines[idx]).is_some_and(|(_, _, v)| !v.is_empty() && !v.starts_with('#')) {
                    lines[idx] = format!("{pad}{key}:");
                }
                from = idx + 1;
                to = yaml_block_end(&lines, idx, child);
                parent = Some(child);
            }
            None => {
                let mut pos = to;
                while pos > from && lines[pos - 1].trim().is_empty() {
                    pos -= 1;
                }
                let new: Vec<String> = path[n..]
                    .iter()
                    .enumerate()
                    .map(|(k, key)| {
                        let pad = " ".repeat(child + 2 * k);
                        if n + k + 1 == path.len() { format!("{pad}{key}: {value}") } else { format!("{pad}{key}:") }
                    })
                    .collect();
                lines.splice(pos..pos, new);
                break;
            }
        }
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') || text.is_empty() {
        out.push('\n');
    }
    (out, old)
}

fn yaml_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ───────────────────────────── файлы ─────────────────────────────

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(".{}.mctui-tmp", path.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| {
        tr!(format!("не удалось записать {}", path.display()), format!("cannot write {}", path.display()))
    })
}

/// `default-velocity.toml` из jar Velocity (есть во всех сборках 3.x/4.x).
pub fn default_config(jar: &Path) -> Option<String> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(jar).ok()?).ok()?;
    let mut f = z.by_name("default-velocity.toml").ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// velocity.toml для нового прокси: из стандартного, с `bind = "0.0.0.0:<port>"` и без примеров
/// серверов (`lobby`, `factions`… в `[servers]`, `try` и `[forced-hosts]` — иначе Velocity не стартует,
/// если их адреса недоступны, а forced-hosts ссылаются на несуществующие серверы).
pub fn initial_config(default: &str, port: u16) -> Result<String> {
    let mut doc = TomlDoc::parse(default);
    doc.set(None, "bind", &toml_str(&format!("0.0.0.0:{port}")));
    doc.set_servers(&[], &[]);
    doc.remove_where(Some("forced-hosts"), |_| true);
    let text = doc.render();
    text.parse::<toml::Table>().context("velocity.toml")?;
    Ok(text)
}

/// Меняет порт прокси (`bind` в velocity.toml), сохраняя адрес.
#[cfg(test)]
pub fn set_bind_port(proxy_dir: &Path, port: u16) -> Result<()> {
    let path = proxy_dir.join(CONFIG);
    let text = std::fs::read_to_string(&path)?;
    let mut doc = TomlDoc::parse(&text);
    let host = doc
        .get(None, "bind")
        .and_then(|v| v.as_str().and_then(|s| s.rsplit_once(':').map(|(h, _)| h.to_string())))
        .unwrap_or_else(|| "0.0.0.0".into());
    doc.set(None, "bind", &toml_str(&format!("{host}:{port}")));
    let out = doc.render();
    out.parse::<toml::Table>().context("velocity.toml")?;
    write_atomic(&path, &out)
}

/// Порт, на котором слушает прокси (`bind` в velocity.toml).
#[cfg(test)]
pub fn proxy_port(proxy_dir: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(proxy_dir.join(CONFIG)).ok()?;
    let bind = TomlDoc::parse(&text).get(None, "bind")?;
    bind.as_str()?.rsplit_once(':')?.1.parse().ok()
}

/// Секрет modern forwarding: из файла `forwarding-secret-file` (или старого ключа `forwarding-secret`),
/// иначе новый случайный — записывается в файл. Возвращает (секрет, создан ли новый).
fn ensure_secret(proxy_dir: &Path, doc: &mut TomlDoc, report: &mut LinkReport) -> Result<String> {
    if let Some(s) = doc.get(None, "forwarding-secret").and_then(|v| v.as_str().map(str::to_string))
        && !s.trim().is_empty()
    {
        return Ok(s.trim().to_string());
    }
    let file = match doc.get(None, "forwarding-secret-file").and_then(|v| v.as_str().map(str::to_string)) {
        Some(f) if !f.trim().is_empty() => f,
        _ => {
            doc.set(None, "forwarding-secret-file", &toml_str(DEFAULT_SECRET_FILE));
            DEFAULT_SECRET_FILE.to_string()
        }
    };
    let path = proxy_dir.join(&file);
    if let Ok(s) = std::fs::read_to_string(&path) {
        // Velocity склеивает строки файла без переводов строк.
        let s: String = s.lines().collect();
        if !s.trim().is_empty() {
            return Ok(s.trim().to_string());
        }
    }
    let secret = crate::daemon::random_token();
    write_atomic(&path, &secret)?;
    report.files.push(path);
    report
        .changes
        .push(tr!(format!("Создан секрет пересылки ({file})"), format!("Created the forwarding secret ({file})")));
    Ok(secret)
}

/// Тип сервера за прокси: по jar (как [`crate::core_update::detect`]) или по признакам в папке.
fn backend_core(dir: &Path) -> Option<Core> {
    if let Some(i) = crate::core_update::detect(dir) {
        return Some(i.core);
    }
    if dir.join(PAPER_GLOBAL).is_file() || dir.join("paper.yml").is_file() {
        Some(Core::Paper)
    } else if dir.join("mods").is_dir() || dir.join(".fabric").is_dir() {
        Some(Core::Fabric)
    } else {
        None
    }
}

fn set_offline(b: &Backend, report: &mut LinkReport) -> Result<()> {
    let path = b.dir.join("server.properties");
    let mut props = if path.is_file() { Properties::load(&path)? } else { Properties::parse("") };
    if !path.is_file() {
        props.set("server-port", &b.port.to_string());
    } else if let Some(p) = props.get("server-port")
        && p.trim() != b.port.to_string()
    {
        report.warnings.push(tr!(
            format!("{}: в server.properties порт {p}, а прокси подключается к {}", b.name, b.port),
            format!("{}: server.properties has port {p}, but the proxy connects to {}", b.name, b.port)
        ));
    }
    if props.get("online-mode") != Some("false") {
        props.set("online-mode", "false");
        props.save(&path)?;
        report.files.push(path);
        report.changes.push(tr!(
            format!("{}: online-mode=false (вход проверяет прокси)", b.name),
            format!("{}: online-mode=false (the proxy authenticates players)", b.name)
        ));
    }
    Ok(())
}

fn link_paper(b: &Backend, secret: &str, report: &mut LinkReport) -> Result<bool> {
    let path = b.dir.join(PAPER_GLOBAL);
    let Ok(mut text) = std::fs::read_to_string(&path) else {
        report.warnings.push(tr!(
            format!(
                "{}: нет {PAPER_GLOBAL} — запустите сервер один раз (он создаст файл) и повторите связывание",
                b.name
            ),
            format!("{}: no {PAPER_GLOBAL} — start the server once (it creates the file) and link again", b.name)
        ));
        return Ok(false);
    };
    let before = text.clone();
    for (key, value) in
        [("enabled", "true".to_string()), ("online-mode", "true".into()), ("secret", yaml_quote(secret))]
    {
        text = yaml_set(&text, &["proxies", "velocity", key], &value).0;
    }
    let ok = yaml_get(&text, &["proxies", "velocity", "enabled"]).as_deref() == Some("true")
        && yaml_get(&text, &["proxies", "velocity", "secret"]).as_deref() == Some(secret);
    if !ok {
        bail!(tr!(
            format!("{}: не удалось изменить {PAPER_GLOBAL}", b.name),
            format!("{}: could not update {PAPER_GLOBAL}", b.name)
        ));
    }
    if text != before {
        write_atomic(&path, &text)?;
        report.files.push(path);
        report.changes.push(tr!(
            format!("{}: включена поддержка Velocity в {PAPER_GLOBAL}", b.name),
            format!("{}: enabled Velocity support in {PAPER_GLOBAL}", b.name)
        ));
    }
    // BungeeCord-режим Spigot мешает modern forwarding.
    let spigot = b.dir.join("spigot.yml");
    if let Ok(s) = std::fs::read_to_string(&spigot)
        && yaml_get(&s, &["settings", "bungeecord"]).as_deref() == Some("true")
    {
        write_atomic(&spigot, &yaml_set(&s, &["settings", "bungeecord"], "false").0)?;
        report.files.push(spigot);
        report.changes.push(tr!(
            format!("{}: spigot.yml settings.bungeecord: false", b.name),
            format!("{}: spigot.yml settings.bungeecord: false", b.name)
        ));
    }
    Ok(true)
}

const FABRIC_PROXY_DEFAULT: &str = "hackOnlineMode = true\nhackEarlySend = false\nhackMessageChain = true\n\
disconnectMessage = \"This server requires you to connect with Velocity.\"\n";

fn link_fabric(b: &Backend, secret: &str, report: &mut LinkReport) -> Result<bool> {
    let mods = crate::plugins::scan(&b.dir);
    let has = |needle: &str| {
        mods.iter().any(|m| {
            m.enabled && (m.name.to_lowercase().contains(needle) || m.file_name.to_lowercase().contains(needle))
        })
    };
    let proxy_mod = has("fabricproxy");
    if !proxy_mod {
        report.warnings.push(tr!(
            format!(
                "{}: для Fabric нужен мод FabricProxy-Lite (Modrinth: fabricproxy-lite) и Fabric API в папке mods",
                b.name
            ),
            format!(
                "{}: Fabric needs the FabricProxy-Lite mod (Modrinth: fabricproxy-lite) and Fabric API in mods",
                b.name
            )
        ));
    } else if !has("fabric api") && !has("fabric-api") {
        report.warnings.push(tr!(
            format!("{}: FabricProxy-Lite требует Fabric API — добавьте его в mods", b.name),
            format!("{}: FabricProxy-Lite requires Fabric API — add it to mods", b.name)
        ));
    }
    let path = b.dir.join(FABRIC_PROXY_CONFIG);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) if b.dir.join("mods").is_dir() => FABRIC_PROXY_DEFAULT.to_string(),
        Err(_) => {
            report.warnings.push(tr!(
                format!("{}: нет папки mods — установите FabricProxy-Lite и повторите связывание", b.name),
                format!("{}: no mods folder — install FabricProxy-Lite and link again", b.name)
            ));
            return Ok(false);
        }
    };
    let mut doc = TomlDoc::parse(&text);
    let old = doc.get(None, "secret").and_then(|v| v.as_str().map(String::from));
    if old.as_deref() != Some(secret) || !path.is_file() {
        doc.set(None, "secret", &toml_str(secret));
        let out = doc.render();
        out.parse::<toml::Table>().context(FABRIC_PROXY_CONFIG)?;
        write_atomic(&path, &out)?;
        report.files.push(path);
        report.changes.push(tr!(
            format!("{}: секрет прокси записан в {FABRIC_PROXY_CONFIG}", b.name),
            format!("{}: proxy secret written to {FABRIC_PROXY_CONFIG}", b.name)
        ));
    }
    Ok(proxy_mod)
}

/// Связывает прокси в `proxy_dir` с серверами `backends`: см. описание модуля.
///
/// `[servers]` velocity.toml заменяется списком `backends` (адрес `127.0.0.1:<port>`), `try` — их порядок;
/// записи `[forced-hosts]`, ссылающиеся на удалённые серверы, убираются. Если velocity.toml нет, он берётся
/// из jar прокси (`default-velocity.toml`). Все серверы и прокси нужно перезапустить.
pub fn link(proxy_dir: &Path, backends: &[Backend]) -> Result<LinkReport> {
    let mut report = LinkReport::default();
    let mut seen = HashSet::new();
    for b in backends {
        if !valid_name(&b.name) {
            bail!(tr!(
                format!("имя сервера «{}» не подходит: только латиница, цифры, - и _", b.name),
                format!("server name “{}” is invalid: only latin letters, digits, - and _", b.name)
            ));
        }
        if !seen.insert(b.name.to_lowercase()) {
            bail!(tr!(format!("имя «{}» повторяется", b.name), format!("duplicate name “{}”", b.name)));
        }
        if b.port == 0 {
            bail!(tr!(format!("{}: порт не может быть 0", b.name), format!("{}: port cannot be 0", b.name)));
        }
        if b.dir == proxy_dir {
            bail!(tr!(
                format!("{}: это папка самого прокси", b.name),
                format!("{}: this is the proxy's own folder", b.name)
            ));
        }
    }

    let path = proxy_dir.join(CONFIG);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            let jar = crate::core_update::detect(proxy_dir).filter(|i| i.core == Core::Velocity).map(|i| i.jar);
            let default = jar.as_deref().and_then(default_config).with_context(|| {
                tr!(
                    format!("в {} нет velocity.toml — запустите прокси один раз", proxy_dir.display()),
                    format!("no velocity.toml in {} — start the proxy once", proxy_dir.display())
                )
            })?;
            initial_config(&default, 25565)?
        }
    };
    let mut doc = TomlDoc::parse(&text);
    if let Some(port) = doc.get(None, "bind").and_then(|v| v.as_str()?.rsplit_once(':')?.1.parse::<u16>().ok())
        && let Some(b) = backends.iter().find(|b| b.port == port)
    {
        bail!(tr!(
            format!("{}: порт {port} уже занят самим прокси — смените порт сервера или прокси", b.name),
            format!("{}: port {port} is the proxy's own port — change the server or proxy port", b.name)
        ));
    }

    let secret = ensure_secret(proxy_dir, &mut doc, &mut report)?;
    let mode = doc.get(None, "player-info-forwarding-mode").and_then(|v| v.as_str().map(str::to_lowercase));
    if mode.as_deref() != Some("modern") {
        doc.set(None, "player-info-forwarding-mode", &toml_str("modern"));
        report.changes.push(tr!(
            "velocity.toml: player-info-forwarding-mode = \"modern\"".to_string(),
            "velocity.toml: player-info-forwarding-mode = \"modern\"".to_string()
        ));
    }

    let servers: Vec<(String, String)> =
        backends.iter().map(|b| (b.name.clone(), format!("127.0.0.1:{}", b.port))).collect();
    let order: Vec<String> = backends.iter().map(|b| b.name.clone()).collect();
    let removed = doc.set_servers(&servers, &order);
    for (name, addr) in removed.iter().filter(|(n, a)| !servers.iter().any(|(sn, sa)| sn == n && sa == a)) {
        if !servers.iter().any(|(n, _)| n == name) {
            report.changes.push(tr!(
                format!("velocity.toml: убран сервер {name} ({addr})"),
                format!("velocity.toml: removed server {name} ({addr})")
            ));
        }
    }
    for (name, addr) in &servers {
        if !removed.iter().any(|(n, a)| n == name && a == addr) {
            report.changes.push(tr!(
                format!("velocity.toml: сервер {name} → {addr}"),
                format!("velocity.toml: server {name} → {addr}")
            ));
        }
    }
    let names: HashSet<&str> = order.iter().map(String::as_str).collect();
    let dropped = doc.remove_where(Some("forced-hosts"), |e| {
        let targets = e.parse_value().and_then(|v| v.as_array().cloned()).unwrap_or_default();
        targets.iter().any(|t| t.as_str().is_none_or(|s| !names.contains(s)))
    });
    for e in dropped {
        report.changes.push(tr!(
            format!("velocity.toml: убран forced-host {} (ссылался на удалённый сервер)", e.key),
            format!("velocity.toml: removed forced host {} (it pointed to a removed server)", e.key)
        ));
    }

    let out = doc.render();
    let parsed: toml::Table = out.parse().context("velocity.toml")?;
    let check = parsed.get("servers").and_then(|s| s.as_table()).is_some_and(|t| {
        servers.iter().all(|(n, a)| t.get(n).and_then(|v| v.as_str()) == Some(a.as_str()))
            && t.get("try").and_then(|v| v.as_array()).map(|a| a.len()) == Some(order.len())
    });
    if !check {
        bail!(tr!("не удалось обновить velocity.toml", "failed to update velocity.toml"));
    }
    if out != text || !path.is_file() {
        write_atomic(&path, &out)?;
        report.files.push(path);
    }

    for b in backends {
        let linked = match backend_core(&b.dir) {
            Some(Core::Paper | Core::Purpur) => {
                let ok = link_paper(b, &secret, &mut report)?;
                set_offline(b, &mut report)?;
                ok
            }
            Some(Core::Fabric) => {
                let ok = link_fabric(b, &secret, &mut report)?;
                set_offline(b, &mut report)?;
                ok
            }
            Some(Core::Vanilla) => {
                report.warnings.push(tr!(
                    format!(
                        "{}: ванильный сервер не поддерживает modern forwarding Velocity — используйте Paper, Purpur или Fabric",
                        b.name
                    ),
                    format!(
                        "{}: a vanilla server does not support Velocity modern forwarding — use Paper, Purpur or Fabric",
                        b.name
                    )
                ));
                false
            }
            Some(Core::Velocity) => bail!(tr!(
                format!("{}: это прокси Velocity, а не игровой сервер", b.name),
                format!("{}: this is a Velocity proxy, not a game server", b.name)
            )),
            None => {
                report.warnings.push(tr!(
                    format!("{}: не удалось определить ядро сервера — настройте его вручную", b.name),
                    format!("{}: could not detect the server core — configure it manually", b.name)
                ));
                false
            }
        };
        if linked {
            report.linked.push(b.name.clone());
        }
    }
    if !report.files.is_empty() {
        report.warnings.push(tr!(
            "Перезапустите прокси и серверы, чтобы изменения вступили в силу".to_string(),
            "Restart the proxy and the servers for the changes to take effect".to_string()
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("mctui-velocity-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Сокращённый default-velocity.toml из Velocity 4.3.1 (ноябрь 2026).
    const DEFAULT: &str = r#"# Config version. Do not change this
config-version = "2.9"

# What port should the proxy be bound to? By default, we'll bind to all addresses on port 25565.
bind = "0.0.0.0:25565"

# Should we authenticate players with Mojang? By default, this is on.
online-mode = true

# - "modern":      Forward player IPs and UUIDs as part of the login process using
#                  Velocity's native forwarding. Only applicable for Minecraft 1.13 or higher.
player-info-forwarding-mode = "NONE"

# If you are using modern or BungeeGuard IP forwarding, configure a file that contains a unique secret here.
# The file is expected to be UTF-8 encoded and not empty.
forwarding-secret-file = "forwarding.secret"

[ping-passthrough]
version = false

[servers]
# Configure your servers here. Each key represents the server's name, and the value
# represents the IP address of the server to connect to.
lobby = "127.0.0.1:30066"
factions = "127.0.0.1:30067"
minigames = "127.0.0.1:30068"

# In what order we should try servers when a player logs in or is kicked from a server.
try = [
    "lobby"
]

[forced-hosts]
# Configure your forced hosts here.
"lobby.example.com" = [
    "lobby"
]
"factions.example.com" = [
    "factions"
]

[advanced]
# How large a Minecraft packet has to be before we compress it.
compression-threshold = 256

[query]
enabled = false
port = 25565
"#;

    const PAPER_GLOBAL_YML: &str = "# This is the global configuration file for Paper.\n\
_version: 31\n\
anticheat:\n  obfuscation:\n    items:\n      all-models:\n        also-obfuscate: []\n        dont-obfuscate:\n        - minecraft:lodestone_tracker\n        sanitize-count: true\n\
messages:\n  no-permission: <red>I'm sorry, but you do not have permission to perform this command.\n    Please contact the server administrators if you believe that this is in error.\n\
proxies:\n  bungee-cord:\n    online-mode: true\n  proxy-protocol: false\n  velocity:\n    enabled: false\n    online-mode: true\n    secret: ''\n\
scoreboards:\n  save-empty-scoreboard-teams: true\n";

    #[test]
    fn names() {
        assert!(valid_name("lobby") && valid_name("survival_2") && valid_name("mini-games"));
        assert!(!valid_name("") && !valid_name("try") && !valid_name("my server") && !valid_name("мир"));
        assert_eq!(sanitize_name("Lobby 1"), "lobby-1");
        assert_eq!(sanitize_name("  My.Server  "), "my-server");
        assert_eq!(sanitize_name("Мир"), "server");
        assert_eq!(sanitize_name("try"), "server");
        assert_eq!(toml_str("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(toml_key("lobby.example.com"), "\"lobby.example.com\"");
    }

    #[test]
    fn toml_doc_entries_and_set() {
        let mut d = TomlDoc::parse(DEFAULT);
        assert_eq!(d.get(None, "bind").unwrap().as_str(), Some("0.0.0.0:25565"));
        assert_eq!(d.get(Some("query"), "port").unwrap().as_integer(), Some(25565));
        let try_list = d.get(Some("servers"), "try").unwrap();
        assert_eq!(try_list.as_array().unwrap().len(), 1);
        let fh = d.entries(d.section(Some("forced-hosts")).unwrap());
        assert_eq!(
            fh.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
            ["lobby.example.com", "factions.example.com"]
        );

        assert_eq!(d.set(None, "bind", "\"0.0.0.0:25577\"").as_deref(), Some("\"0.0.0.0:25565\""));
        assert_eq!(d.set(None, "new-key", "1"), None);
        let out = d.render();
        // Новый корневой ключ — после последнего корневого, до таблиц.
        let parsed: toml::Table = out.parse().unwrap();
        assert_eq!(parsed["new-key"].as_integer(), Some(1));
        assert!(out.contains("# What port should the proxy be bound to?"));
        assert!(out.contains("bind = \"0.0.0.0:25577\"\n"));
        assert!(out.find("new-key").unwrap() < out.find("[ping-passthrough]").unwrap());
        // Остальное не тронуто.
        assert_eq!(out.lines().count(), DEFAULT.lines().count() + 1);
    }

    #[test]
    fn initial_config_clears_examples() {
        let out = initial_config(DEFAULT, 25577).unwrap();
        let t: toml::Table = out.parse().unwrap();
        assert_eq!(t["bind"].as_str(), Some("0.0.0.0:25577"));
        let servers = t["servers"].as_table().unwrap();
        assert_eq!(servers.len(), 1, "{servers:?}");
        assert_eq!(servers["try"].as_array().unwrap().len(), 0);
        assert!(t["forced-hosts"].as_table().unwrap().is_empty());
        assert!(out.contains("# Configure your servers here."));
        assert!(out.contains("# In what order we should try servers"));
        assert_eq!(t["advanced"]["compression-threshold"].as_integer(), Some(256));
    }

    #[test]
    fn set_servers_keeps_comments_and_order() {
        let mut d = TomlDoc::parse(DEFAULT);
        let removed = d.set_servers(
            &[("lobby".into(), "127.0.0.1:25566".into()), ("survival".into(), "127.0.0.1:25567".into())],
            &["lobby".into(), "survival".into()],
        );
        assert_eq!(removed.len(), 3);
        let out = d.render();
        let t: toml::Table = out.parse().unwrap();
        let s = t["servers"].as_table().unwrap();
        assert_eq!(s["lobby"].as_str(), Some("127.0.0.1:25566"));
        assert_eq!(s["survival"].as_str(), Some("127.0.0.1:25567"));
        assert!(s.get("factions").is_none());
        let tries: Vec<&str> = s["try"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(tries, ["lobby", "survival"]);
        let comment = out.find("# Configure your servers here.").unwrap();
        let lobby = out.find("lobby = \"127.0.0.1:25566\"").unwrap();
        let order_comment = out.find("# In what order").unwrap();
        let try_pos = out.find("try = [").unwrap();
        assert!(comment < lobby && lobby < order_comment && order_comment < try_pos, "{out}");
        // Повторный вызов с тем же — без изменений.
        let again = {
            let mut d2 = TomlDoc::parse(&out);
            d2.set_servers(
                &[("lobby".into(), "127.0.0.1:25566".into()), ("survival".into(), "127.0.0.1:25567".into())],
                &["lobby".into(), "survival".into()],
            );
            d2.render()
        };
        assert_eq!(again, out);
        // Нет таблицы [servers] — создаётся.
        let mut d = TomlDoc::parse("bind = \"0.0.0.0:25565\"\n");
        d.set_servers(&[("a".into(), "127.0.0.1:1".into())], &["a".into()]);
        let t: toml::Table = d.render().parse().unwrap();
        assert_eq!(t["servers"]["a"].as_str(), Some("127.0.0.1:1"));
    }

    #[test]
    fn yaml_edit_preserves_rest() {
        let (out, old) = yaml_set(PAPER_GLOBAL_YML, &["proxies", "velocity", "enabled"], "true");
        assert_eq!(old.as_deref(), Some("false"));
        let (out, _) = yaml_set(&out, &["proxies", "velocity", "secret"], &yaml_quote("abc'd"));
        assert_eq!(yaml_get(&out, &["proxies", "velocity", "enabled"]).as_deref(), Some("true"));
        assert_eq!(yaml_get(&out, &["proxies", "velocity", "secret"]).as_deref(), Some("abc'd"));
        assert_eq!(yaml_get(&out, &["proxies", "bungee-cord", "online-mode"]).as_deref(), Some("true"));
        assert_eq!(out.lines().count(), PAPER_GLOBAL_YML.lines().count());
        let diff: Vec<(&str, &str)> =
            PAPER_GLOBAL_YML.lines().zip(out.lines()).filter(|(a, b)| a != b).collect::<Vec<_>>();
        assert_eq!(diff, [("    enabled: false", "    enabled: true"), ("    secret: ''", "    secret: 'abc''d'")]);
        // Списки «- x» на уровне ключа не ломают поиск.
        assert_eq!(
            yaml_get(PAPER_GLOBAL_YML, &["anticheat", "obfuscation", "items", "all-models", "sanitize-count"])
                .as_deref(),
            Some("true")
        );
        assert_eq!(
            yaml_get(PAPER_GLOBAL_YML, &["scoreboards", "save-empty-scoreboard-teams"]).as_deref(),
            Some("true")
        );

        // Недостающие ключи создаются в конце своего блока.
        let (out, old) = yaml_set(
            "_version: 31\nproxies:\n  proxy-protocol: false\nzzz: 1\n",
            &["proxies", "velocity", "enabled"],
            "true",
        );
        assert_eq!(old, None);
        assert_eq!(out, "_version: 31\nproxies:\n  proxy-protocol: false\n  velocity:\n    enabled: true\nzzz: 1\n");
        let (out, _) = yaml_set("a: 1\n", &["proxies", "velocity", "enabled"], "true");
        assert_eq!(out, "a: 1\nproxies:\n  velocity:\n    enabled: true\n");
        let (out, _) = yaml_set("proxies:\n  velocity: {}\n", &["proxies", "velocity", "enabled"], "true");
        assert_eq!(out, "proxies:\n  velocity:\n    enabled: true\n");
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn link_network() {
        let t = TempDir::new("link");
        let proxy = t.0.join("proxy");
        write(&proxy.join(CONFIG), &initial_config(DEFAULT, 25565).unwrap());

        // Paper, уже запускавшийся.
        let paper = t.0.join("lobby");
        write(&paper.join("paper-26.3-249.jar"), "");
        write(&paper.join(PAPER_GLOBAL), PAPER_GLOBAL_YML);
        write(&paper.join("server.properties"), "#Minecraft server properties\nonline-mode=true\nserver-port=25566\n");
        write(&paper.join("spigot.yml"), "settings:\n  bungeecord: true\n  debug: false\n");
        // Purpur, ни разу не запускавшийся.
        let purpur = t.0.join("survival");
        write(&purpur.join("purpur-26.3-2658.jar"), "");
        // Fabric с модами.
        let fabric = t.0.join("modded");
        write(&fabric.join("fabric-server-mc.1.21.11-loader.0.20.1-launcher.1.1.2.jar"), "");
        write(&fabric.join("mods/FabricProxy-Lite-2.10.1.jar"), "");
        write(&fabric.join("server.properties"), "server-port=25568\n");
        // Vanilla.
        let vanilla = t.0.join("vanilla");
        write(&vanilla.join("minecraft_server.1.21.11.jar"), "");

        let backends = vec![
            Backend { name: "lobby".into(), dir: paper.clone(), port: 25566 },
            Backend { name: "survival".into(), dir: purpur.clone(), port: 25567 },
            Backend { name: "modded".into(), dir: fabric.clone(), port: 25568 },
            Backend { name: "vanilla".into(), dir: vanilla.clone(), port: 25569 },
        ];
        let r = link(&proxy, &backends).unwrap();
        println!("{r:#?}");

        let secret = std::fs::read_to_string(proxy.join("forwarding.secret")).unwrap();
        assert_eq!(secret.len(), 48);
        let vt: toml::Table = std::fs::read_to_string(proxy.join(CONFIG)).unwrap().parse().unwrap();
        assert_eq!(vt["player-info-forwarding-mode"].as_str(), Some("modern"));
        assert_eq!(vt["servers"]["survival"].as_str(), Some("127.0.0.1:25567"));
        assert_eq!(vt["servers"]["try"].as_array().unwrap().len(), 4);

        let pg = std::fs::read_to_string(paper.join(PAPER_GLOBAL)).unwrap();
        assert_eq!(yaml_get(&pg, &["proxies", "velocity", "enabled"]).as_deref(), Some("true"));
        assert_eq!(yaml_get(&pg, &["proxies", "velocity", "online-mode"]).as_deref(), Some("true"));
        assert_eq!(yaml_get(&pg, &["proxies", "velocity", "secret"]).as_deref(), Some(secret.as_str()));
        let props = Properties::load(&paper.join("server.properties")).unwrap();
        assert_eq!(props.get("online-mode"), Some("false"));
        assert_eq!(props.get("server-port"), Some("25566"));
        let spigot = std::fs::read_to_string(paper.join("spigot.yml")).unwrap();
        assert_eq!(spigot, "settings:\n  bungeecord: false\n  debug: false\n");

        // Purpur без paper-global.yml: предупреждение, но server.properties готов.
        assert!(r.warnings.iter().any(|w| w.starts_with("survival: ") && w.contains("config/paper-global.yml")));
        let props = Properties::load(&purpur.join("server.properties")).unwrap();
        assert_eq!(props.get("online-mode"), Some("false"));
        assert_eq!(props.get("server-port"), Some("25567"));

        let fp: toml::Table = std::fs::read_to_string(fabric.join(FABRIC_PROXY_CONFIG)).unwrap().parse().unwrap();
        assert_eq!(fp["secret"].as_str(), Some(secret.as_str()));
        assert_eq!(fp["hackOnlineMode"].as_bool(), Some(true));
        assert!(r.warnings.iter().any(|w| w.starts_with("modded: ") && w.contains("Fabric API")));

        assert!(r.warnings.iter().any(|w| w.starts_with("vanilla: ")));
        assert!(!vanilla.join("server.properties").exists());
        assert_eq!(r.linked, ["lobby", "modded"]);

        // Повторное связывание: секрет тот же, файлы не меняются.
        let before = std::fs::read_to_string(proxy.join(CONFIG)).unwrap();
        let r2 = link(&proxy, &backends).unwrap();
        assert_eq!(std::fs::read_to_string(proxy.join("forwarding.secret")).unwrap(), secret);
        assert_eq!(std::fs::read_to_string(proxy.join(CONFIG)).unwrap(), before);
        assert!(!r2.files.contains(&proxy.join(CONFIG)));

        // Убрать сервер из сети.
        let r3 = link(&proxy, &backends[..1]).unwrap();
        assert!(r3.changes.iter().any(|c| c.contains("survival (127.0.0.1:25567)")));
        let vt: toml::Table = std::fs::read_to_string(proxy.join(CONFIG)).unwrap().parse().unwrap();
        assert!(vt["servers"].get("survival").is_none());
    }

    #[test]
    fn link_validates() {
        let t = TempDir::new("validate");
        let proxy = t.0.join("proxy");
        write(&proxy.join(CONFIG), &initial_config(DEFAULT, 25565).unwrap());
        let b = |name: &str, port| Backend { name: name.into(), dir: t.0.join(name), port };
        assert!(link(&proxy, &[b("bad name", 25566)]).is_err());
        assert!(link(&proxy, &[b("a", 25566), b("A", 25567)]).is_err());
        assert!(link(&proxy, &[b("a", 25565)]).is_err(), "port of the proxy itself");
        assert!(link(&t.0.join("nothing"), &[b("a", 25566)]).is_err(), "no velocity.toml and no jar");
        // Ничего не записано.
        assert!(!proxy.join("forwarding.secret").exists());
    }

    #[test]
    fn bind_port() {
        let t = TempDir::new("bind");
        write(&t.0.join(CONFIG), DEFAULT);
        assert_eq!(proxy_port(&t.0), Some(25565));
        set_bind_port(&t.0, 25600).unwrap();
        assert_eq!(proxy_port(&t.0), Some(25600));
    }
}
