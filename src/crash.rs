//! Отчёты о падениях (`crash-reports/crash-*.txt`) и поиск по всем логам сервера:
//! `logs/latest.log`, архивам `logs/*.log.gz` и отчётам о падениях.
//!
//! Поиск ограничен числом совпадений, файлов и прочитанных байт и может быть отменён флагом,
//! поэтому огромная папка логов не подвесит интерфейс (вызывать в `spawn_blocking`).

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Сколько строк читать из начала отчёта о падении.
const HEAD_LINES: usize = 100;
/// Длина строки в результатах поиска (длинные JSON-строки обрезаются).
const MAX_LINE_CHARS: usize = 2000;

#[derive(Clone, Debug)]
pub struct CrashReport {
    pub path: PathBuf,
    /// Время из имени файла (`crash-2026-11-13_04.27.43-server.txt`), иначе — время изменения.
    pub time: DateTime<Local>,
    /// `Description: ...`, например «Exception in server tick loop».
    pub description: Option<String>,
    /// Первая строка исключения, например `java.lang.NullPointerException: ...`.
    pub first_exception_line: Option<String>,
    /// Последний `Caused by: ...` в начале отчёта — обычно настоящая причина.
    pub root_cause: Option<String>,
}

impl CrashReport {
    #[cfg(test)]
    pub fn file_name(&self) -> String {
        self.path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }

    /// Короткая строка для списка.
    pub fn summary(&self) -> String {
        let what = self
            .root_cause
            .as_deref()
            .map(|c| c.trim_start_matches("Caused by:").trim())
            .or(self.first_exception_line.as_deref())
            .or(self.description.as_deref())
            .unwrap_or("?");
        format!("{} · {what}", self.time.format("%Y-%m-%d %H:%M:%S"))
    }
}

fn time_from_name(name: &str) -> Option<DateTime<Local>> {
    // crash-2026-11-13_04.27.43-server.txt (Paper/Fabric: -server, -client, -fml…)
    let s = name.strip_prefix("crash-")?;
    let stamp = s.get(..19)?;
    let naive = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d_%H.%M.%S").ok()?;
    Local.from_local_datetime(&naive).earliest()
}

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Разбор начала отчёта о падении.
pub fn parse_crash_head(text: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut description = None;
    let mut first_exc = None;
    let mut root = None;
    let mut after_desc = false;
    for line in text.lines().take(HEAD_LINES) {
        let t = line.trim();
        if let Some(d) = t.strip_prefix("Description:") {
            description = Some(d.trim().to_string());
            after_desc = true;
            continue;
        }
        if t.starts_with("A detailed walkthrough") || t.starts_with("-- ") {
            break;
        }
        if after_desc && first_exc.is_none() && !t.is_empty() && !t.starts_with("at ") && !t.starts_with("//") {
            first_exc = Some(t.to_string());
            continue;
        }
        if t.starts_with("Caused by:") {
            root = Some(t.to_string());
        }
    }
    (description, first_exc, root)
}

/// Отчёты о падениях из `<dir>/crash-reports/crash-*.txt`, новые первыми.
pub fn list_crash_reports(dir: &Path) -> Vec<CrashReport> {
    let Ok(rd) = std::fs::read_dir(dir.join("crash-reports")) else {
        return vec![];
    };
    let mut v: Vec<CrashReport> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy();
            n.starts_with("crash-") && n.ends_with(".txt") && p.is_file()
        })
        .map(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let time = time_from_name(&name)
                .or_else(|| mtime(&path).map(DateTime::<Local>::from))
                .unwrap_or_else(|| DateTime::<Local>::from(std::time::UNIX_EPOCH));
            let head = read_head(&path, HEAD_LINES).unwrap_or_default();
            let (description, first_exception_line, root_cause) = parse_crash_head(&head);
            CrashReport { path, time, description, first_exception_line, root_cause }
        })
        .collect();
    v.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.path.cmp(&a.path)));
    v
}

fn read_head(p: &Path, lines: usize) -> std::io::Result<String> {
    let f = std::fs::File::open(p)?;
    let mut r = BufReader::new(f.take(256 << 10));
    let mut out = String::new();
    let mut buf = Vec::new();
    for _ in 0..lines {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        out.push_str(&String::from_utf8_lossy(&buf));
    }
    Ok(out)
}

// ── Поиск ───────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogMatch {
    pub file: PathBuf,
    /// С единицы.
    pub line_no: usize,
    /// Строка без ANSI/§-кодов (обрезана до 2000 символов).
    pub line: String,
}

pub struct SearchOpts<'a> {
    pub case_insensitive: bool,
    /// Максимум совпадений всего.
    pub limit: usize,
    /// Максимум файлов (новые первыми).
    pub max_files: usize,
    /// Максимум прочитанных (распакованных) байт по всем файлам.
    pub max_bytes: u64,
    /// Отмена из другого потока.
    pub cancel: Option<&'a AtomicBool>,
}

impl Default for SearchOpts<'_> {
    fn default() -> Self {
        Self { case_insensitive: true, limit: 500, max_files: 200, max_bytes: 512 << 20, cancel: None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchResult {
    /// Файлы — от новых к старым; внутри файла — по порядку строк.
    pub matches: Vec<LogMatch>,
    pub files_scanned: usize,
    /// Упёрлись в лимит совпадений/файлов/байт: показаны не все.
    pub truncated: bool,
    pub cancelled: bool,
    /// Файлы, которые не удалось прочитать: (путь, ошибка).
    pub errors: Vec<(PathBuf, String)>,
}

/// Файлы для поиска в папке сервера: latest.log, затем архивы и отчёты о падениях — новые первыми.
#[cfg(test)]
pub fn search_files(dir: &Path) -> Vec<PathBuf> {
    search_files_in(&dir.join("logs"), &dir.join("crash-reports"))
}

/// То же для произвольных папок логов и отчётов (например, для RCON-сервера с `log_path`).
pub fn search_files_in(logs_dir: &Path, crash_dir: &Path) -> Vec<PathBuf> {
    let list = |d: &Path, pred: &dyn Fn(&str) -> bool| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && pred(&p.file_name().unwrap_or_default().to_string_lossy()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut rest = list(logs_dir, &|n| n.ends_with(".log.gz"));
    rest.extend(list(crash_dir, &|n| n.starts_with("crash-") && n.ends_with(".txt")));
    let mut keyed: Vec<(Option<std::time::SystemTime>, PathBuf)> = rest.into_iter().map(|p| (mtime(&p), p)).collect();
    keyed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut out = vec![];
    let latest = logs_dir.join("latest.log");
    if latest.is_file() {
        out.push(latest);
    }
    out.extend(keyed.into_iter().map(|(_, p)| p));
    out
}

/// Поиск подстроки по всем логам папки сервера. Короткая форма для типичного вызова.
#[cfg(test)]
pub fn search_logs(dir: &Path, query: &str, case_insensitive: bool, limit: usize) -> SearchResult {
    search_logs_with(&search_files(dir), query, &SearchOpts { case_insensitive, limit, ..Default::default() })
}

/// Поиск по списку файлов (`.gz` распаковываются на лету). Из каждого файла берутся последние
/// (самые свежие) совпадения в пределах оставшегося лимита.
pub fn search_logs_with(files: &[PathBuf], query: &str, o: &SearchOpts) -> SearchResult {
    let mut res = SearchResult::default();
    if query.is_empty() || o.limit == 0 {
        return res;
    }
    let needle = if o.case_insensitive { query.to_lowercase() } else { query.to_string() };
    let mut bytes_left = o.max_bytes;
    let cancelled = || o.cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    for (i, path) in files.iter().enumerate() {
        if res.matches.len() >= o.limit || i >= o.max_files || bytes_left == 0 {
            res.truncated = true;
            break;
        }
        if cancelled() {
            res.cancelled = true;
            break;
        }
        let remaining = o.limit - res.matches.len();
        match search_one(path, &needle, o.case_insensitive, remaining, &mut bytes_left, &cancelled) {
            Ok((found, dropped)) => {
                res.files_scanned += 1;
                res.truncated |= dropped;
                res.matches.extend(found);
            }
            Err(e) => res.errors.push((path.clone(), e.to_string())),
        }
        if cancelled() {
            res.cancelled = true;
            break;
        }
    }
    if bytes_left == 0 {
        res.truncated = true;
    }
    res
}

/// Совпадения в одном файле: (последние `keep` штук, были ли отброшены лишние).
fn search_one(
    path: &Path,
    needle: &str,
    ci: bool,
    keep: usize,
    bytes_left: &mut u64,
    cancelled: &dyn Fn() -> bool,
) -> std::io::Result<(Vec<LogMatch>, bool)> {
    let f = std::fs::File::open(path)?;
    let raw: Box<dyn Read> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(flate2::read::MultiGzDecoder::new(f))
    } else {
        Box::new(f)
    };
    let mut r = BufReader::new(raw.take(*bytes_left));
    let mut found: VecDeque<LogMatch> = VecDeque::new();
    let mut dropped = false;
    let mut buf = Vec::new();
    let mut line_no = 0usize;
    loop {
        buf.clear();
        let n = r.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        *bytes_left = bytes_left.saturating_sub(n as u64);
        line_no += 1;
        if line_no.is_multiple_of(4096) && cancelled() {
            break;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim_end_matches(['\n', '\r']);
        let clean = if text.contains(['\x1b', '§']) {
            std::borrow::Cow::Owned(crate::log::strip_codes(text))
        } else {
            std::borrow::Cow::Borrowed(text)
        };
        let hit = if ci { clean.to_lowercase().contains(needle) } else { clean.contains(needle) };
        if !hit {
            continue;
        }
        if found.len() == keep {
            found.pop_front();
            dropped = true;
        }
        let line = if clean.chars().count() > MAX_LINE_CHARS {
            clean.chars().take(MAX_LINE_CHARS).collect::<String>() + "…"
        } else {
            clean.into_owned()
        };
        found.push_back(LogMatch { file: path.to_path_buf(), line_no, line });
    }
    Ok((found.into(), dropped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CRASH: &str = "---- Minecraft Crash Report ----
// Why did you do that?

Time: 2026-11-13 04:27:43
Description: Exception in server tick loop

java.lang.IllegalStateException: Boom
\tat net.minecraft.server.MinecraftServer.tick(MinecraftServer.java:1)
\tat java.base/java.lang.Thread.run(Thread.java:1583)
Caused by: java.lang.NullPointerException: Cannot invoke \"foo()\"
\t... 2 more


A detailed walkthrough of the error, its code path and all known details is as follows:
---------------------------------------------------------------------------------------

-- Head --
Thread: Server thread
Caused by: should.not.be.Seen
";

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pechka-crash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("logs")).unwrap();
        std::fs::create_dir_all(d.join("crash-reports")).unwrap();
        d
    }

    fn gz(path: &Path, text: &str) {
        let f = std::fs::File::create(path).unwrap();
        let mut e = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        e.write_all(text.as_bytes()).unwrap();
        e.finish().unwrap();
    }

    fn set_mtime(p: &Path, secs_ago: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn parses_crash_head() {
        let (d, e, c) = parse_crash_head(CRASH);
        assert_eq!(d.as_deref(), Some("Exception in server tick loop"));
        assert_eq!(e.as_deref(), Some("java.lang.IllegalStateException: Boom"));
        assert_eq!(c.as_deref(), Some("Caused by: java.lang.NullPointerException: Cannot invoke \"foo()\""));
        assert_eq!(parse_crash_head("garbage\n"), (None, None, None));
    }

    #[test]
    fn lists_crash_reports_newest_first() {
        let d = tmp("list");
        let cr = d.join("crash-reports");
        std::fs::write(cr.join("crash-2026-11-13_04.27.43-server.txt"), CRASH).unwrap();
        std::fs::write(cr.join("crash-2025-01-02_10.00.00-server.txt"), "Description: Old\n\njava.lang.Error: x\n")
            .unwrap();
        std::fs::write(cr.join("notes.txt"), "ignored").unwrap();
        let v = list_crash_reports(&d);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].file_name(), "crash-2026-11-13_04.27.43-server.txt");
        assert_eq!(v[0].time.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-11-13 04:27:43");
        assert_eq!(v[1].description.as_deref(), Some("Old"));
        assert_eq!(v[1].first_exception_line.as_deref(), Some("java.lang.Error: x"));
        assert!(v[0].summary().contains("NullPointerException"));
        assert!(list_crash_reports(&d.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn searches_latest_archives_and_crashes() {
        let d = tmp("search");
        let logs = d.join("logs");
        std::fs::write(
            logs.join("latest.log"),
            "[10:00:00 INFO]: Steve joined the game\n\x1b[31m[10:00:01 ERROR]: \x1b[0mBoom here\n",
        )
        .unwrap();
        gz(&logs.join("2026-11-12-1.log.gz"), "[09:00:00 INFO]: boom old 1\nnothing\n[09:00:02 INFO]: BOOM old 2\n");
        gz(&logs.join("2026-11-01-1.log.gz"), "[08:00:00 INFO]: boom oldest\n");
        std::fs::write(logs.join("debug.txt"), "boom but not a log").unwrap();
        std::fs::write(d.join("crash-reports/crash-2026-11-13_04.27.43-server.txt"), CRASH).unwrap();
        set_mtime(&logs.join("2026-11-12-1.log.gz"), 3600);
        set_mtime(&logs.join("2026-11-01-1.log.gz"), 86_400 * 12);
        set_mtime(&d.join("crash-reports/crash-2026-11-13_04.27.43-server.txt"), 60);

        let files = search_files(&d);
        let names: Vec<String> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(
            names,
            ["latest.log", "crash-2026-11-13_04.27.43-server.txt", "2026-11-12-1.log.gz", "2026-11-01-1.log.gz"]
        );

        let r = search_logs(&d, "boom", true, 100);
        assert!(!r.truncated && !r.cancelled && r.errors.is_empty());
        let lines: Vec<&str> = r.matches.iter().map(|m| m.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "[10:00:01 ERROR]: Boom here", // ANSI убран
                "java.lang.IllegalStateException: Boom",
                "[09:00:00 INFO]: boom old 1",
                "[09:00:02 INFO]: BOOM old 2",
                "[08:00:00 INFO]: boom oldest",
            ]
        );
        assert_eq!(r.matches[0].line_no, 2);
        assert_eq!(r.matches[3].line_no, 3);
        assert_eq!(r.files_scanned, 4);

        // С учётом регистра.
        let r = search_logs(&d, "BOOM", false, 100);
        assert_eq!(r.matches.len(), 1);

        // Лимит: из первого файла с совпадениями берутся последние, дальше — стоп.
        let r = search_logs(&d, "old", true, 1);
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].line, "[09:00:02 INFO]: BOOM old 2");
        assert!(r.truncated);

        // Лимиты файлов и байт.
        let r = search_logs_with(&files, "boom", &SearchOpts { max_files: 1, ..Default::default() });
        assert_eq!((r.matches.len(), r.truncated), (1, true));
        let r = search_logs_with(&files, "boom", &SearchOpts { max_bytes: 40, ..Default::default() });
        assert!(r.truncated && r.matches.is_empty());

        // Отмена.
        let stop = AtomicBool::new(true);
        let r = search_logs_with(&files, "boom", &SearchOpts { cancel: Some(&stop), ..Default::default() });
        assert!(r.cancelled && r.matches.is_empty());

        assert!(search_logs(&d, "", true, 10).matches.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn broken_gz_is_reported_not_fatal() {
        let d = tmp("broken");
        std::fs::write(d.join("logs/2026-11-12-1.log.gz"), b"not gzip at all").unwrap();
        std::fs::write(d.join("logs/latest.log"), "needle\n").unwrap();
        let r = search_logs(&d, "needle", true, 10);
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.errors.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
