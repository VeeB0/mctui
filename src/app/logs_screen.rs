//! Экран логов: источники (живой вывод, чат, файлы), фильтр, поиск, прокрутка.

use super::*;

#[derive(Debug, Clone)]
pub enum LogSource {
    /// Вывод процесса из службы (или сессия RCON).
    Live,
    /// Только сообщения чата из живого вывода.
    Chat,
    File(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelFilter {
    All,
    WarnUp,
    ErrorOnly,
}

impl LevelFilter {
    pub fn label(self) -> &'static str {
        match self {
            LevelFilter::All => tr!("все", "all"),
            LevelFilter::WarnUp => "WARN+",
            LevelFilter::ErrorOnly => "ERROR",
        }
    }
    pub fn pass(self, l: Level) -> bool {
        match self {
            LevelFilter::All => true,
            LevelFilter::WarnUp => matches!(l, Level::Warn | Level::Error),
            LevelFilter::ErrorOnly => l == Level::Error,
        }
    }
}

pub struct LogsView {
    pub sources: Vec<(String, LogSource)>,
    pub source_sel: usize,
    pub active: usize,
    pub file_lines: Vec<LogLine>,
    pub file_error: Option<String>,
    /// Сколько строк от конца пролистано (0 — следим за концом).
    pub scroll: usize,
    pub hscroll: u16,
    pub filter: LevelFilter,
    pub search: Option<String>,
    pub search_input: Option<TextInput>,
    pub match_cursor: Option<usize>,
    pub focus_list: bool,
}

impl LogsView {
    pub(super) fn new() -> Self {
        Self {
            sources: vec![],
            source_sel: 0,
            active: 0,
            file_lines: vec![],
            file_error: None,
            scroll: 0,
            hscroll: 0,
            filter: LevelFilter::All,
            search: None,
            search_input: None,
            match_cursor: None,
            focus_list: false,
        }
    }

    pub fn active_source(&self) -> Option<&LogSource> {
        self.sources.get(self.active).map(|s| &s.1)
    }
}

impl App {
    pub fn toggle_logs(&mut self) {
        if self.screen == Screen::Logs {
            self.screen = Screen::Main;
            return;
        }
        if !self.has_server() {
            return;
        }
        self.screen = Screen::Logs;
        self.build_log_sources();
        let s = self.sel();
        // По умолчанию: живой вывод, если сервер работает (или это RCON), иначе latest.log.
        let prefer_live = s.running()
            || s.cfg.kind == Kind::Rcon && s.cfg.log_path.is_none()
            || s.log.len() > 0 && s.status == Status::Crashed;
        let pick = if prefer_live {
            self.logs.sources.iter().position(|(_, src)| matches!(src, LogSource::Live))
        } else {
            self.logs.sources.iter().position(|(_, src)| matches!(src, LogSource::File(_)))
        };
        self.open_log_source(pick.unwrap_or(0));
    }

    pub(super) fn build_log_sources(&mut self) {
        let s = self.sel();
        let mut v: Vec<(String, LogSource)> = vec![];
        match s.cfg.kind {
            Kind::Local => {
                let live = if s.running() {
                    tr!("● Живой вывод", "● Live output")
                } else {
                    tr!("Вывод последнего запуска", "Last run output")
                };
                v.push((live.into(), LogSource::Live));
                v.push((tr!("💬 Чат", "💬 Chat").into(), LogSource::Chat));
                if let Some(dir) = s.cfg.logs_dir() {
                    let latest = dir.join("latest.log");
                    if latest.is_file() {
                        v.push(("latest.log".into(), LogSource::File(latest)));
                    }
                    for c in crate::crash::list_crash_reports(s.cfg.dir()).into_iter().take(20) {
                        let label = format!("⚠ {} · {}", c.time.format("%d.%m %H:%M"), c.summary());
                        v.push((label, LogSource::File(c.path)));
                    }
                    let mut archives: Vec<PathBuf> = std::fs::read_dir(&dir)
                        .map(|rd| {
                            rd.flatten()
                                .map(|e| e.path())
                                .filter(|p| p.to_string_lossy().ends_with(".log.gz"))
                                .collect()
                        })
                        .unwrap_or_default();
                    archives.sort();
                    archives.reverse();
                    for a in archives.into_iter().take(100) {
                        let name = a.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        v.push((name, LogSource::File(a)));
                    }
                }
            }
            Kind::Rcon => {
                v.push((tr!("Сессия RCON", "RCON session").into(), LogSource::Live));
                if let Some(p) = &s.cfg.log_path {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    v.push((tr!("💬 Чат", "💬 Chat").into(), LogSource::Chat));
                    v.push((name, LogSource::File(p.clone())));
                }
            }
        }
        self.logs.sources = v;
    }

    pub(super) fn open_log_source(&mut self, i: usize) {
        let Some((_, src)) = self.logs.sources.get(i).cloned() else {
            return;
        };
        self.logs.active = i;
        self.logs.source_sel = i;
        self.logs.scroll = 0;
        self.logs.hscroll = 0;
        self.logs.match_cursor = None;
        self.logs.file_lines.clear();
        self.logs.file_error = None;
        if let LogSource::File(p) = src {
            match read_log_file(&p) {
                Ok(lines) => self.logs.file_lines = lines,
                Err(e) => self.logs.file_error = Some(format!("{}: {e}", p.display())),
            }
        }
    }

    /// Строки активного источника после фильтра.
    pub fn visible_log_lines(&self) -> Vec<&LogLine> {
        let f = self.logs.filter;
        match self.logs.active_source() {
            Some(LogSource::Live) => self.sel().log.iter().filter(|l| f.pass(l.level)).collect(),
            Some(LogSource::Chat) => {
                let s = self.sel();
                // У RCON-сервера живого вывода нет — чат берётся из его файла лога.
                if s.is_local() || s.log.iter().any(|l| crate::server::stats::is_chat(&l.text)) {
                    s.log.iter().filter(|l| crate::server::stats::is_chat(&l.text)).collect()
                } else {
                    vec![]
                }
            }
            Some(LogSource::File(_)) => self.logs.file_lines.iter().filter(|l| f.pass(l.level)).collect(),
            None => vec![],
        }
    }

    pub fn log_matches(&self, lines: &[&LogLine]) -> Vec<usize> {
        let Some(q) = self.logs.search.as_ref().filter(|q| !q.is_empty()) else {
            return vec![];
        };
        let q = q.to_lowercase();
        lines.iter().enumerate().filter(|(_, l)| l.text.to_lowercase().contains(&q)).map(|(i, _)| i).collect()
    }

    pub(super) fn jump_to_match(&mut self, d: isize) {
        let lines = self.visible_log_lines();
        let matches = self.log_matches(&lines);
        let total = lines.len();
        drop(lines);
        if matches.is_empty() {
            self.logs.match_cursor = None;
            if self.logs.search.is_some() {
                self.toast(tr!("совпадений нет", "no matches"), ToastKind::Info);
            }
            return;
        }
        let cur = match self.logs.match_cursor {
            None => matches.len() - 1,
            Some(c) => (c as isize + d).clamp(0, matches.len() as isize - 1) as usize,
        };
        self.logs.match_cursor = Some(cur);
        let h = self.logs_height.get().max(1) as usize;
        let line = matches[cur];
        // Совпадение — примерно в середине экрана.
        let end = (line + h / 2 + 1).min(total);
        self.logs.scroll = total - end.max(h.min(total));
    }

    pub(super) fn scroll_logs(&mut self, d: isize) {
        let total = self.visible_log_lines().len();
        let h = self.logs_height.get() as usize;
        let max = total.saturating_sub(h);
        self.logs.scroll = (self.logs.scroll as isize + d).clamp(0, max as isize) as usize;
    }

    pub(super) fn logs_key(&mut self, k: KeyEvent) {
        if let Some(mut inp) = self.logs.search_input.take() {
            match k.code {
                KeyCode::Enter => {
                    let q = inp.value.trim().to_string();
                    self.logs.search = (!q.is_empty()).then_some(q);
                    self.logs.match_cursor = None;
                    self.jump_to_match(0);
                }
                KeyCode::Esc => {}
                _ => {
                    inp.handle(&k);
                    self.logs.search_input = Some(inp);
                }
            }
            return;
        }
        let h = self.logs_height.get().max(1) as isize;
        let chat = matches!(self.logs.active_source(), Some(LogSource::Chat));
        match k.code {
            KeyCode::Esc => {
                if self.logs.search.is_some() {
                    self.logs.search = None;
                    self.logs.match_cursor = None;
                } else {
                    self.screen = Screen::Main;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => self.logs.focus_list = !self.logs.focus_list,
            KeyCode::Up if self.logs.focus_list => self.logs.source_sel = self.logs.source_sel.saturating_sub(1),
            KeyCode::Down if self.logs.focus_list => {
                self.logs.source_sel = (self.logs.source_sel + 1).min(self.logs.sources.len().saturating_sub(1))
            }
            KeyCode::Enter if self.logs.focus_list => {
                self.open_log_source(self.logs.source_sel);
                self.logs.focus_list = false;
            }
            KeyCode::Enter if chat => self.open_chat_input(),
            KeyCode::Up => self.scroll_logs(1),
            KeyCode::Down => self.scroll_logs(-1),
            KeyCode::PageUp => self.scroll_logs(h - 1),
            KeyCode::PageDown => self.scroll_logs(-(h - 1)),
            KeyCode::Home => self.scroll_logs(isize::MAX / 2),
            KeyCode::End => self.logs.scroll = 0,
            KeyCode::Left => self.logs.hscroll = self.logs.hscroll.saturating_sub(10),
            KeyCode::Right => self.logs.hscroll = self.logs.hscroll.saturating_add(10),
            KeyCode::Char(c) => match latin(c) {
                'l' | 'L' | 'q' => self.screen = Screen::Main,
                '?' | ',' => self.open_help(),
                '/' | '.' => self.logs.search_input = Some(TextInput::with(self.logs.search.as_deref().unwrap_or(""))),
                'n' => self.jump_to_match(-1),
                'N' => self.jump_to_match(1),
                'm' | 'i' => self.open_chat_input(),
                'F' => {
                    self.popup = Some(Popup::Edit {
                        title: tr!("Поиск по всем логам и отчётам о падениях", "Search all logs and crash reports")
                            .into(),
                        input: TextInput::with(self.logs.search.as_deref().unwrap_or("")),
                        target: EditTarget::LogSearch,
                    })
                }
                'c' => {
                    let i = self.logs.sources.iter().position(|(_, s)| matches!(s, LogSource::Chat));
                    if let Some(i) = i {
                        self.open_log_source(i);
                    }
                }
                'f' => {
                    self.logs.filter = match self.logs.filter {
                        LevelFilter::All => LevelFilter::WarnUp,
                        LevelFilter::WarnUp => LevelFilter::ErrorOnly,
                        LevelFilter::ErrorOnly => LevelFilter::All,
                    };
                    self.logs.scroll = 0;
                    self.logs.match_cursor = None;
                }
                'r' => {
                    self.build_log_sources();
                    let a = self.logs.active.min(self.logs.sources.len().saturating_sub(1));
                    self.open_log_source(a);
                }
                'g' => self.scroll_logs(isize::MAX / 2),
                'G' => self.logs.scroll = 0,
                _ => {}
            },
            _ => {}
        }
    }

    pub(super) fn open_chat_input(&mut self) {
        if !self.sel().accepts_commands() {
            return self.toast(tr!("сервер не запущен", "the server is not running"), ToastKind::Info);
        }
        self.popup = Some(Popup::Edit {
            title: tr!("Сообщение в чат (say)", "Chat message (say)").into(),
            input: TextInput::default(),
            target: EditTarget::Chat,
        });
    }
}

fn read_log_file(p: &std::path::Path) -> anyhow::Result<Vec<LogLine>> {
    use std::io::Read;
    const CAP: usize = 50_000;
    let mut bytes = Vec::new();
    let f = std::fs::File::open(p)?;
    if p.to_string_lossy().ends_with(".gz") {
        flate2::read::GzDecoder::new(f).take(256 << 20).read_to_end(&mut bytes)?;
    } else {
        f.take(256 << 20).read_to_end(&mut bytes)?;
    }
    Ok(crate::log::parse_text(&String::from_utf8_lossy(&bytes), CAP))
}
