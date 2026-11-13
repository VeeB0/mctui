//! Параметры запуска Java локального сервера: память (`-Xms` / `-Xmx`), набор флагов JVM ([`Profile`]) и
//! проверка Java ([`advise`]).
//!
//! - [`read`] разбирает строку `java … -jar X.jar …` в скрипте запуска (start.sh / run.sh / start.bat…) или
//!   саму команду из настроек, если она запускает `java -jar` напрямую.
//! - [`advise`] советует объём памяти и набор флагов и предупреждает о проблемах (мало ОЗУ, старая или
//!   32-битная Java, `-Xms` ≠ `-Xmx` при флагах Aikar…). Без сети: нужная Java берётся из таблицы
//!   [`known_required_java`]; точное значение из API даёт [`fetch_required_java`] (сеть).
//! - [`preview`] / [`apply`] меняют только строку запуска: память, флаги старого набора на флаги нового;
//!   свои `-D…`, `-Xss…` и прочие флаги пользователя, jar и аргументы после него остаются. Остальные строки
//!   скрипта, окончания строк (`\r\n` / `\n`) и права файла сохраняются, старый скрипт копируется в
//!   `<dir>/.pechka-old/<скрипт>-ГГГГммдд-ЧЧММСС`.
//!
//! Разбор строки — общий с `core_update` ([`cu::find_in_script`], [`cu::tokens`]).
//! Сервер должен быть остановлен — новые параметры подействуют при следующем запуске.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::core_update::{self as cu, Tok};
use crate::create::{self, Core};
use crate::tr;

/// Выше этого `-Xmx` Aikar советует вариант флагов для большой кучи (`> 12G`).
pub const AIKAR_LARGE_HEAP_MB: u32 = 12 * 1024;
/// Меньше этого `-Xmx` не ставим: JVM с такой кучей сервер Minecraft не запустит.
pub const MIN_XMX_MB: u32 = 256;

/// Флаги Aikar, как в документации PaperMC (docs.papermc.io/paper/aikars-flags, проверено 2026-11).
const AIKAR: [&str; 20] = [
    "-XX:+UseG1GC",
    "-XX:+ParallelRefProcEnabled",
    "-XX:MaxGCPauseMillis=200",
    "-XX:+UnlockExperimentalVMOptions",
    "-XX:+DisableExplicitGC",
    "-XX:+AlwaysPreTouch",
    "-XX:G1NewSizePercent=30",
    "-XX:G1MaxNewSizePercent=40",
    "-XX:G1HeapRegionSize=8M",
    "-XX:G1ReservePercent=20",
    "-XX:G1HeapWastePercent=5",
    "-XX:G1MixedGCCountTarget=4",
    "-XX:InitiatingHeapOccupancyPercent=15",
    "-XX:G1MixedGCLiveThresholdPercent=90",
    "-XX:G1RSetUpdatingPauseTimePercent=5",
    "-XX:SurvivorRatio=32",
    "-XX:+PerfDisableSharedMem",
    "-XX:MaxTenuringThreshold=1",
    "-Dusing.aikars.flags=https://mcflags.emc.gs",
    "-Daikars.new.flags=true",
];

/// Замены для кучи больше 12 ГБ (mcflags.emc.gs: «If you have and use more than 12GB of memory»).
const AIKAR_LARGE: [(&str, &str); 5] = [
    ("-XX:G1NewSizePercent=30", "-XX:G1NewSizePercent=40"),
    ("-XX:G1MaxNewSizePercent=40", "-XX:G1MaxNewSizePercent=50"),
    ("-XX:G1HeapRegionSize=8M", "-XX:G1HeapRegionSize=16M"),
    ("-XX:G1ReservePercent=20", "-XX:G1ReservePercent=15"),
    ("-XX:InitiatingHeapOccupancyPercent=15", "-XX:InitiatingHeapOccupancyPercent=20"),
];

/// Флаги для Velocity из генератора скриптов PaperMC (docs.papermc.io/misc/tools/start-script-gen).
const VELOCITY: [&str; 6] = [
    "-XX:+UseG1GC",
    "-XX:G1HeapRegionSize=4M",
    "-XX:+UnlockExperimentalVMOptions",
    "-XX:+ParallelRefProcEnabled",
    "-XX:+AlwaysPreTouch",
    "-XX:MaxInlineLevel=15",
];

/// Простая настройка G1 для Fabric / Vanilla.
const BASIC: [&str; 4] =
    ["-XX:+UseG1GC", "-XX:+ParallelRefProcEnabled", "-XX:MaxGCPauseMillis=200", "-XX:+DisableExplicitGC"];

/// Набор флагов JVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Флаги Aikar (рекомендация PaperMC для Paper / Purpur), с вариантом для кучи > 12 ГБ.
    Aikar,
    /// Флаги PaperMC для прокси Velocity.
    Velocity,
    /// Простая настройка G1 (Fabric / Vanilla).
    Basic,
    /// Без флагов настройки: только память.
    None,
    /// Флаги `-XX:…`, не похожие ни на один набор. При смене только памяти не трогаются.
    Custom,
}

impl Profile {
    /// Что можно выбрать в интерфейсе ([`Profile::Custom`] только распознаётся).
    pub const CHOICES: [Profile; 4] = [Profile::Aikar, Profile::Velocity, Profile::Basic, Profile::None];

    pub fn label(self) -> &'static str {
        match self {
            Profile::Aikar => tr!("Флаги Aikar", "Aikar's flags"),
            Profile::Velocity => tr!("Флаги для Velocity", "Velocity flags"),
            Profile::Basic => tr!("Простой G1", "Basic G1"),
            Profile::None => tr!("Без флагов", "No flags"),
            Profile::Custom => tr!("Свои флаги", "Custom flags"),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Profile::Aikar => tr!(
                "Рекомендация PaperMC для Paper / Purpur: короткие паузы сборщика мусора",
                "PaperMC's recommendation for Paper / Purpur: short garbage collector pauses"
            ),
            Profile::Velocity => {
                tr!("Рекомендация PaperMC для прокси Velocity", "PaperMC's recommendation for the Velocity proxy")
            }
            Profile::Basic => tr!(
                "Сборщик G1 с умеренными паузами — для Fabric и Vanilla",
                "G1 collector with moderate pauses — for Fabric and Vanilla"
            ),
            Profile::None => {
                tr!("Только память, остальное — по умолчанию Java", "Memory only, Java defaults otherwise")
            }
            Profile::Custom => tr!(
                "Флаги не из известного набора — остаются как есть",
                "Flags that match no known set — kept as they are"
            ),
        }
    }

    /// Флаги набора для кучи `xmx_mb` (у Aikar выше [`AIKAR_LARGE_HEAP_MB`] — вариант для большой кучи).
    /// У [`Profile::None`] и [`Profile::Custom`] — пусто.
    pub fn flags(self, xmx_mb: u32) -> Vec<String> {
        let list: &[&str] = match self {
            Profile::Aikar => &AIKAR,
            Profile::Velocity => &VELOCITY,
            Profile::Basic => &BASIC,
            Profile::None | Profile::Custom => &[],
        };
        list.iter()
            .map(|f| {
                let large = self == Profile::Aikar && xmx_mb > AIKAR_LARGE_HEAP_MB;
                let swap = AIKAR_LARGE.iter().find(|(base, _)| base == f).filter(|_| large);
                swap.map_or(*f, |(_, l)| *l).to_string()
            })
            .collect()
    }

    /// Флаг принадлежит этому набору (у Aikar — любому из двух вариантов) и уйдёт при смене набора.
    /// У [`Profile::Custom`] это все флаги настройки (`-XX:…`, свойства Aikar), у [`Profile::None`] — ни один.
    fn owns(self, flag: &str) -> bool {
        match self {
            Profile::Aikar => AIKAR.contains(&flag) || AIKAR_LARGE.iter().any(|(_, l)| *l == flag),
            Profile::Velocity => VELOCITY.contains(&flag),
            Profile::Basic => BASIC.contains(&flag),
            Profile::None => false,
            Profile::Custom => is_tuning(flag),
        }
    }
}

/// Флаг настройки JVM: `-XX:…` или свойства-метки Aikar. Прочие `-D…`, `-X…`, `--add-opens…` — флаги пользователя.
fn is_tuning(flag: &str) -> bool {
    flag.starts_with("-XX:") || flag.starts_with("-Dusing.aikars.flags=") || flag.starts_with("-Daikars.new.flags=")
}

/// Набор по флагам (в любом порядке): все флаги набора есть, а прочие флаги настройки не взяты из другого
/// набора → этот набор (свои флаги пользователя вроде `-Dfile.encoding` не мешают). Нет флагов настройки →
/// [`Profile::None`], иначе (например, Aikar без одного флага) — [`Profile::Custom`].
pub fn detect_profile(flags: &[String]) -> Profile {
    if !flags.iter().any(|f| is_tuning(f)) {
        return Profile::None;
    }
    const KNOWN: [Profile; 3] = [Profile::Aikar, Profile::Velocity, Profile::Basic];
    let matches = |p: Profile, set: Vec<String>| {
        set.iter().all(|f| flags.contains(f))
            && flags.iter().filter(|f| is_tuning(f) && !p.owns(f)).all(|f| !KNOWN.iter().any(|q| q.owns(f)))
    };
    let aikar =
        matches(Profile::Aikar, Profile::Aikar.flags(u32::MAX)) || matches(Profile::Aikar, Profile::Aikar.flags(0));
    if aikar {
        Profile::Aikar
    } else if let Some(p) = [Profile::Velocity, Profile::Basic].into_iter().find(|&p| matches(p, p.flags(0))) {
        p
    } else {
        Profile::Custom
    }
}

/// Имя флага без значения: `-XX:+UseG1GC` / `-XX:-UseG1GC` → `-XX:UseG1GC`, `-XX:G1ReservePercent=20` →
/// `-XX:G1ReservePercent`, `-Dkey=v` → `-Dkey`.
fn flag_key(flag: &str) -> String {
    let key = flag.split_once('=').map_or(flag, |(k, _)| k);
    match key.strip_prefix("-XX:") {
        Some(rest) => format!("-XX:{}", rest.trim_start_matches(['+', '-'])),
        None => key.to_string(),
    }
}

// ───────────────────────────── разбор ─────────────────────────────

/// Как запускается Java.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Скрипт со строкой запуска; `None` — команда из настроек сама запускает `java -jar`.
    pub script: Option<PathBuf>,
    /// Номер строки запуска в скрипте (с 1).
    pub line_no: Option<usize>,
    /// Строка запуска как есть (для команды — аргументы через пробел).
    pub line: String,
    /// Java как в скрипте: `java`, `./jdk25/Contents/Home/bin/java`, `"$JAVA_HOME/bin/java"`…
    pub java: String,
    pub xms_mb: Option<u32>,
    pub xmx_mb: Option<u32>,
    /// Все параметры JVM между java и `-jar`, кроме `-Xms` / `-Xmx`, по порядку.
    pub flags: Vec<String>,
    /// Те из [`Launch::flags`], что не входят в найденный набор: они останутся при смене набора.
    pub extra_flags: Vec<String>,
    pub jar: String,
    /// Аргументы после jar (`--nogui`, `nogui`, `"$@"`, `%*`…).
    pub args: Vec<String>,
    /// Найденный набор флагов.
    pub profile: Profile,
}

/// Строка запуска с позициями — для изменения.
struct Parsed {
    launch: Launch,
    /// Текст скрипта и разметка строки (для команды — `None`).
    script: Option<ScriptLine>,
    /// Индексы java и `-jar` в словах строки (или в аргументах команды).
    java_idx: usize,
    jar_flag: usize,
}

struct ScriptLine {
    path: PathBuf,
    text: String,
    bat: bool,
    /// Номер строки с 0.
    line: usize,
    toks: Vec<Tok>,
}

/// Объём из значения `-Xmx` / `-Xms`: `4G`, `4096M`, `4g`, `4194304k`, байты без единицы → МиБ.
fn parse_size(v: &str) -> Option<u32> {
    let digits = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let n: u64 = v[..digits].parse().ok()?;
    let mb = match &v[digits..] {
        "" => n / (1024 * 1024),
        "k" | "K" => n / 1024,
        "m" | "M" => n,
        "g" | "G" => n * 1024,
        "t" | "T" => n * 1024 * 1024,
        _ => return None,
    };
    u32::try_from(mb).ok().filter(|&m| m > 0)
}

/// `-Xmx` + `4096` → `-Xmx4G`; не кратное гигабайту — в мегабайтах (`-Xmx4608M`).
fn size_flag(prefix: &str, mb: u32) -> String {
    if mb.is_multiple_of(1024) { format!("{prefix}{}G", mb / 1024) } else { format!("{prefix}{mb}M") }
}

fn has_variable(s: &str) -> bool {
    s.contains('$') || s.contains('%') || s.contains('`')
}

/// Память, флаги и проверка «можно ли менять» для слов между java и `-jar`.
struct Jvm {
    xms_mb: Option<u32>,
    xmx_mb: Option<u32>,
    flags: Vec<String>,
}

fn parse_jvm(words: &[&str], where_: &str) -> Result<Jvm> {
    let mut j = Jvm { xms_mb: None, xmx_mb: None, flags: Vec::new() };
    for &w in words {
        if w.starts_with('@') {
            bail!(tr!(
                format!("{where_}: параметры Java читаются из файла ({w}) — так pechka менять не умеет"),
                format!("{where_}: Java options are read from a file ({w}) — pechka can't edit that")
            ));
        }
        if has_variable(w) {
            bail!(tr!(
                format!("{where_}: в параметрах Java есть переменная ({w}) — память и флаги не изменить безопасно"),
                format!(
                    "{where_}: the Java options contain a variable ({w}) — memory and flags can't be changed safely"
                )
            ));
        }
        for (prefix, slot) in [("-Xmx", &mut j.xmx_mb), ("-Xms", &mut j.xms_mb)] {
            if let Some(v) = w.strip_prefix(prefix) {
                let mb = parse_size(v).with_context(|| {
                    tr!(
                        format!("{where_}: непонятный объём памяти {w}"),
                        format!("{where_}: unreadable memory size {w}")
                    )
                })?;
                *slot = Some(mb); // Как у Java: последнее значение побеждает.
            }
        }
        if !w.starts_with("-Xmx") && !w.starts_with("-Xms") {
            j.flags.push(w.to_string());
        }
    }
    Ok(j)
}

fn make_launch(
    script: Option<PathBuf>,
    line_no: Option<usize>,
    line: String,
    java: String,
    jvm: Jvm,
    jar: String,
    args: Vec<String>,
) -> Launch {
    let profile = detect_profile(&jvm.flags);
    let extra_flags = jvm.flags.iter().filter(|f| !profile.owns(f)).cloned().collect();
    Launch {
        script,
        line_no,
        line,
        java,
        xms_mb: jvm.xms_mb,
        xmx_mb: jvm.xmx_mb,
        flags: jvm.flags,
        extra_flags,
        jar,
        args,
        profile,
    }
}

fn script_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// Понятная ошибка для скрипта без подходящей строки `java … -jar`.
fn no_launch_line(path: &Path, text: &str, bat: bool) -> anyhow::Error {
    let name = script_name(path);
    let forge = text.lines().filter(|l| !cu::is_comment(l, bat)).any(|l| {
        let toks = cu::tokens(l, bat);
        toks.iter().any(|t| cu::is_java(&t.value)) && toks.iter().any(|t| t.value.starts_with('@'))
    });
    if forge {
        anyhow::anyhow!(tr!(
            format!(
                "{name}: Java запускается с @user_jvm_args.txt (Forge / NeoForge) — память задаётся в этом файле, pechka его пока не меняет"
            ),
            format!(
                "{name}: Java is started with @user_jvm_args.txt (Forge / NeoForge) — memory is set in that file, which pechka doesn't edit yet"
            )
        ))
    } else {
        anyhow::anyhow!(tr!(
            format!("{name}: не найдена строка запуска «java … -jar сервер.jar»"),
            format!("{name}: no “java … -jar server.jar” launch line found")
        ))
    }
}

fn parse_script(path: &Path) -> Result<Parsed> {
    let name = script_name(path);
    let text = std::fs::read_to_string(path)
        .with_context(|| tr!(format!("не удалось прочитать {name}"), format!("cannot read {name}")))?;
    let bat = cu::is_bat(path);
    let r = cu::find_in_script(&text, bat).ok_or_else(|| no_launch_line(path, &text, bat))?;
    let lines: Vec<&str> = text.lines().collect();
    let line = lines[r.line];
    let cont = if bat { '^' } else { '\\' };
    let continued = |l: &str| l.trim_end().ends_with(cont);
    if continued(line) || (r.line > 0 && continued(lines[r.line - 1])) {
        bail!(tr!(
            format!("{name}: строка запуска разбита на несколько строк ({cont} в конце) — так pechka менять не умеет"),
            format!("{name}: the launch line is split over several lines ({cont} at the end) — pechka can't edit that")
        ));
    }
    let Some(java_idx) = r.java_idx else {
        let var = r.toks[..r.jar_flag].iter().find(|t| has_variable(&t.value));
        bail!(match var {
            Some(t) => tr!(
                format!("{name}: Java задана переменной ({}) — эту строку pechka менять не умеет", t.value),
                format!("{name}: Java is set by a variable ({}) — pechka can't edit this line", t.value)
            ),
            None => tr!(
                format!("{name}: в строке запуска перед -jar не найдена java"),
                format!("{name}: no java before -jar on the launch line")
            ),
        });
    };
    let words: Vec<&str> = r.toks[java_idx + 1..r.jar_flag].iter().map(|t| t.value.as_str()).collect();
    let jvm = parse_jvm(&words, &name)?;
    let args = r.toks[r.jar_flag + 2..].iter().map(|t| t.value.clone()).collect();
    let launch = make_launch(
        Some(path.to_path_buf()),
        Some(r.line + 1),
        line.to_string(),
        r.toks[java_idx].value.clone(),
        jvm,
        r.jar.value.clone(),
        args,
    );
    let (jar_flag, toks) = (r.jar_flag, r.toks);
    Ok(Parsed {
        launch,
        script: Some(ScriptLine { path: path.to_path_buf(), text, bat, line: r.line, toks }),
        java_idx,
        jar_flag,
    })
}

fn parse_command(command: &[String], j: usize) -> Result<Parsed> {
    let what = tr!("команда запуска", "start command");
    command
        .get(j + 1)
        .with_context(|| tr!("в команде запуска нет jar после -jar", "no jar after -jar in the start command"))?;
    let java_idx = command[..j].iter().position(|a| cu::is_java(a)).with_context(|| {
        tr!("команда запуска не запускает java перед -jar", "the start command doesn't run java before -jar")
    })?;
    let words: Vec<&str> = command[java_idx + 1..j].iter().map(String::as_str).collect();
    let jvm = parse_jvm(&words, what)?;
    let launch = make_launch(
        None,
        None,
        command.join(" "),
        command[java_idx].clone(),
        jvm,
        command[j + 1].clone(),
        command[j + 2..].to_vec(),
    );
    Ok(Parsed { launch, script: None, java_idx, jar_flag: j })
}

fn parse(dir: &Path, command: &[String]) -> Result<Parsed> {
    if let Some(j) = command.iter().position(|a| a == "-jar") {
        return parse_command(command, j);
    }
    if let Some(s) = cu::command_script(command) {
        let path = dir.join(s);
        if !path.is_file() {
            bail!(tr!(format!("нет скрипта запуска {s}"), format!("start script {s} not found")));
        }
        return parse_script(&path);
    }
    let scripts = cu::script_candidates(dir, &[]);
    let mut first_err = None;
    for s in &scripts {
        match parse_script(s) {
            Ok(p) => return Ok(p),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err.unwrap_or_else(|| {
        anyhow::anyhow!(tr!(
            "не найден скрипт запуска (start.sh / start.bat) со строкой java … -jar",
            "no start script (start.sh / start.bat) with a java … -jar line found"
        ))
    }))
}

/// Как запускается Java сервера в `dir` с командой `command` из настроек: строка `java … -jar` в скрипте
/// (из команды или start.sh / run.sh / start.command / start.bat…) либо сама команда `java … -jar X.jar`.
/// Ошибка с понятным текстом, если строку нельзя безопасно менять (переменные, `@файл`, перенос строки).
pub fn read(dir: &Path, command: &[String]) -> Result<Launch> {
    parse(dir, command).map(|p| p.launch)
}

// ───────────────────────────── совет ─────────────────────────────

/// Что удалось узнать о Java из строки запуска.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaCheck {
    /// Запускается; версия и разрядность (если по выводу `-version` понятно).
    Found { major: Option<u32>, bits64: Option<bool> },
    /// Файла нет / `java` нет в PATH.
    NotFound,
    /// Есть, но `java -version` не отработала.
    Broken,
    /// Задана переменной (`$JAVA_HOME/bin/java`, `%~dp0jre\bin\java.exe`) — не проверить.
    Variable,
}

/// Путь к программе Java из строки запуска: `java` — из PATH, относительный — от папки сервера.
pub fn resolve_java(dir: &Path, java: &str) -> Option<PathBuf> {
    if has_variable(java) {
        return None;
    }
    if !java.contains(['/', '\\']) {
        let stem = java.strip_suffix(".exe").unwrap_or(java);
        return if stem.eq_ignore_ascii_case("java") { create::system_java() } else { create::which(stem) };
    }
    let p = Path::new(java);
    let p = if p.is_absolute() { p.to_path_buf() } else { dir.join(java.trim_start_matches("./")) };
    if !p.exists() && cfg!(windows) && p.extension().is_none() {
        let exe = p.with_extension("exe");
        if exe.exists() {
            return Some(exe);
        }
    }
    Some(p)
}

/// Разрядность по выводу `java -version`: «64-Bit Server VM» → 64; «Client VM» / «Server VM» без 64 → 32.
fn bits64_from(text: &str) -> Option<bool> {
    if text.contains("64-Bit") || text.contains("64-bit") {
        Some(true)
    } else if text.contains(" VM") {
        Some(false)
    } else {
        None
    }
}

/// Запускает `java -version` той Java, что в строке запуска. Блокирует (доли секунды).
pub fn check_java(dir: &Path, java: &str) -> JavaCheck {
    if has_variable(java) {
        return JavaCheck::Variable;
    }
    let Some(path) = resolve_java(dir, java) else { return JavaCheck::NotFound };
    if !path.is_file() {
        return JavaCheck::NotFound;
    }
    match create::java_version_text(&path) {
        Some(text) => JavaCheck::Found { major: create::parse_java_major(&text), bits64: bits64_from(&text) },
        None => JavaCheck::Broken,
    }
}

/// Минимальная Java без сети — по версии Minecraft (как `javaVersion` у Mojang) или Velocity.
/// `None`, если версия не распознана (снапшоты вида `25w45a`).
pub fn known_required_java(core: Option<Core>, version: &str) -> Option<u32> {
    let base = version.trim().split('-').next().unwrap_or_default();
    if base.is_empty() || !base.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let k = create::version_key(base);
    let at = |i: usize| k.get(i).copied().unwrap_or(0);
    if core == Some(Core::Velocity) {
        return Some(match (at(0), at(1)) {
            (4.., _) => 25,
            (3, 5..) => 21,
            (3, 3..) => 17,
            _ => 11,
        });
    }
    match (at(0), at(1), at(2)) {
        (26.., _, _) => Some(25),
        (1, 21.., _) | (1, 20, 5..) => Some(21),
        (1, 18..=20, _) => Some(17),
        (1, 17, _) => Some(16),
        (1, _, _) => Some(8),
        _ => None,
    }
}

/// Точная минимальная Java из API (Mojang, PaperMC Fill). Сеть, блокирует — из `spawn_blocking`.
pub fn fetch_required_java(core: Core, version: &str) -> Option<u32> {
    create::required_java(core, version).ok().map(|(j, _)| j)
}

/// Объём ОЗУ компьютера в МиБ (0, если не узнать).
pub fn total_ram_mb() -> u64 {
    sysinfo::System::new()
        .ok()
        .map(|mut s| {
            s.refresh_memory();
            s.total_memory() / (1024 * 1024)
        })
        .unwrap_or(0)
}

/// Рекомендуемый `-Xmx`: системе остаётся четверть ОЗУ (на машинах от 8 ГБ это не меньше 2 ГБ), вниз
/// до 512 МБ, в пределах 1–16 ГБ: больше 16 ГБ большинству серверов не помогает, а паузы сборщика растут.
/// Velocity — 512 МБ (прокси больше не нужно).
pub fn recommended_memory_mb(total_ram_mb: u64, core: Option<Core>) -> u32 {
    if core == Some(Core::Velocity) {
        return 512;
    }
    let total = if total_ram_mb == 0 { 8192 } else { total_ram_mb };
    let free = total * 3 / 4 / 512 * 512;
    u32::try_from(free.clamp(1024, 16 * 1024)).unwrap_or(16 * 1024)
}

/// Набор флагов, который советуем для ядра.
pub fn recommended_profile(core: Option<Core>) -> Profile {
    match core {
        Some(Core::Paper | Core::Purpur) => Profile::Aikar,
        Some(Core::Velocity) => Profile::Velocity,
        Some(Core::Fabric | Core::Vanilla) | None => Profile::Basic,
    }
}

/// Совет по параметрам запуска.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advice {
    pub total_ram_mb: u64,
    /// Рекомендуемый `-Xmx`, МиБ.
    pub recommended_mb: u32,
    /// Версия Java из строки запуска (если удалось запустить `java -version`).
    pub java_major: Option<u32>,
    pub java: JavaCheck,
    /// Нужная Java для ядра и версии (если известна).
    pub java_required: Option<u32>,
    /// Проблемы: сервер может не запуститься или работать плохо.
    pub warnings: Vec<String>,
    /// Советы: можно лучше, но и так работает.
    pub hints: Vec<String>,
    pub recommended_profile: Profile,
}

impl Advice {
    /// Изменение «как советуем»: рекомендуемая память и набор (`-Xms` = `-Xmx` для Aikar).
    pub fn suggested_change(&self) -> Change {
        Change {
            xmx_mb: Some(self.recommended_mb),
            xms_equal: self.recommended_profile == Profile::Aikar,
            profile: Some(self.recommended_profile),
        }
    }
}

fn gb(mb: u64) -> String {
    let g = mb as f64 / 1024.0;
    if g.fract() == 0.0 { format!("{g:.0}") } else { format!("{g:.1}") }
}

/// Совет для строки запуска `launch` (без сети; запускает `java -version`, блокирует на доли секунды).
/// `core` и `mc_version` — из `core_update::detect`, если известны.
#[cfg(all(test, unix))]
pub fn advise(dir: &Path, launch: &Launch, core: Option<Core>, mc_version: Option<&str>) -> Advice {
    let required = mc_version.and_then(|v| known_required_java(core, v));
    advise_with(launch, core, total_ram_mb(), check_java(dir, &launch.java), required)
}

/// [`advise`] с известными данными: ОЗУ, проверка Java и нужная Java (например, из [`fetch_required_java`]).
pub fn advise_with(
    launch: &Launch,
    core: Option<Core>,
    total_ram_mb: u64,
    java: JavaCheck,
    java_required: Option<u32>,
) -> Advice {
    let mut warnings = Vec::new();
    let mut hints = Vec::new();
    let recommended_mb = recommended_memory_mb(total_ram_mb, core);
    let recommended_profile = recommended_profile(core);
    let proxy = core == Some(Core::Velocity);
    let j = &launch.java;

    // Java
    let mut java_major = None;
    match &java {
        JavaCheck::NotFound => warnings.push(tr!(
            format!("Java не найдена: {j} — сервер не запустится"),
            format!("Java not found: {j} — the server won't start")
        )),
        JavaCheck::Broken => warnings.push(tr!(
            format!("{j} не запускается (java -version завершилась с ошибкой)"),
            format!("{j} does not run (java -version failed)")
        )),
        JavaCheck::Variable => hints.push(tr!(
            format!("Java задана переменной ({j}) — версию не проверить"),
            format!("Java is set by a variable ({j}) — its version can't be checked")
        )),
        JavaCheck::Found { major, bits64 } => {
            java_major = *major;
            if *bits64 == Some(false) {
                warnings.push(tr!(
                    "32-битная Java: больше ~1,5 ГБ памяти ей не дать — поставьте 64-битную".to_string(),
                    "32-bit Java: it can't use more than ~1.5 GB — install a 64-bit Java".to_string()
                ));
            }
            if let (Some(m), Some(r)) = (*major, java_required)
                && m < r
            {
                warnings.push(tr!(
                    format!("Java {m} слишком старая: для этой версии нужна Java {r} или новее"),
                    format!("Java {m} is too old: this version needs Java {r} or newer")
                ));
            }
        }
    }

    // Память
    match launch.xmx_mb {
        Some(xmx) => {
            if total_ram_mb > 0 && u64::from(xmx) + 1024 > total_ram_mb {
                warnings.push(tr!(
                    format!(
                        "-Xmx {} ГБ при {} ГБ ОЗУ: системе и самой Java не хватит памяти — советуем {} ГБ",
                        gb(xmx.into()),
                        gb(total_ram_mb),
                        gb(recommended_mb.into())
                    ),
                    format!(
                        "-Xmx {} GB with {} GB of RAM leaves too little for the system and Java itself — {} GB is advised",
                        gb(xmx.into()),
                        gb(total_ram_mb),
                        gb(recommended_mb.into())
                    )
                ));
            }
            if !proxy && xmx < 1024 {
                warnings.push(tr!(
                    format!("-Xmx {xmx} МБ — мало для сервера Minecraft, нужно хотя бы 1 ГБ"),
                    format!("-Xmx {xmx} MB is too little for a Minecraft server, at least 1 GB is needed")
                ));
            }
            if !proxy && xmx > 16 * 1024 {
                hints.push(tr!(
                    "Больше 16 ГБ памяти серверу обычно не помогает, а паузы сборщика мусора растут".to_string(),
                    "More than 16 GB rarely helps a server, and garbage collector pauses grow".to_string()
                ));
            }
            if proxy && xmx > 2048 {
                hints.push(tr!(
                    "Прокси Velocity обычно хватает 512 МБ – 1 ГБ".to_string(),
                    "The Velocity proxy usually needs only 512 MB – 1 GB".to_string()
                ));
            }
            if launch.profile == Profile::Aikar
                && xmx > AIKAR_LARGE_HEAP_MB
                && !launch.flags.iter().any(|f| f == "-XX:G1HeapRegionSize=16M")
            {
                hints.push(tr!(
                    "Для кучи больше 12 ГБ у Aikar другой вариант флагов — примените набор заново".to_string(),
                    "Aikar has a variant of the flags for heaps above 12 GB — apply the profile again".to_string()
                ));
            }
        }
        None => hints.push(tr!(
            format!(
                "-Xmx не задан: Java возьмёт четверть ОЗУ — лучше указать явно (советуем {} ГБ)",
                gb(recommended_mb.into())
            ),
            format!(
                "-Xmx is not set: Java takes a quarter of RAM — better set it explicitly ({} GB advised)",
                gb(recommended_mb.into())
            )
        )),
    }
    if launch.profile == Profile::Aikar && launch.xms_mb != launch.xmx_mb {
        warnings.push(tr!(
            "С флагами Aikar -Xms должен быть равен -Xmx".to_string(),
            "With Aikar's flags -Xms should equal -Xmx".to_string()
        ));
    }
    if matches!(core, Some(Core::Paper | Core::Purpur)) && launch.profile != Profile::Aikar {
        hints.push(tr!(
            "Для Paper / Purpur PaperMC советует флаги Aikar".to_string(),
            "PaperMC recommends Aikar's flags for Paper / Purpur".to_string()
        ));
    }

    Advice { total_ram_mb, recommended_mb, java_major, java, java_required, warnings, hints, recommended_profile }
}

// ───────────────────────────── изменение ─────────────────────────────

/// Что поменять.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Change {
    /// Новый `-Xmx`, МиБ (`None` — оставить).
    pub xmx_mb: Option<u32>,
    /// Поставить `-Xms` равным `-Xmx`. Иначе `-Xms` остаётся (но не больше `-Xmx`).
    pub xms_equal: bool,
    /// Новый набор флагов (`None` или [`Profile::Custom`] — флаги не трогать; у Aikar вариант для большой
    /// кучи всё равно подстраивается под новый `-Xmx`).
    pub profile: Option<Profile>,
}

/// Результат [`preview`] / [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Изменённый скрипт (для команды из настроек — `None`).
    pub script: Option<PathBuf>,
    /// Строка запуска до и после (для команды — аргументы через пробел).
    pub before: String,
    pub after: String,
    /// Новая команда для настроек сервера, если Java запускается командой, а не скриптом.
    pub new_command: Option<Vec<String>>,
    /// Копия старого скрипта (`<dir>/.pechka-old/start.sh-20261113-054500`); у [`preview`] — `None`.
    pub backup: Option<PathBuf>,
    /// Строка запуска после изменения.
    pub launch: Launch,
}

impl Applied {
    pub fn changed(&self) -> bool {
        self.before != self.after
    }
}

/// Новые память и флаги.
struct Plan {
    xms_mb: Option<u32>,
    xmx_mb: Option<u32>,
    /// `None` — флаги как были; иначе (набор, оставшиеся флаги пользователя — индексы в [`Launch::flags`]).
    flags: Option<(Vec<String>, Vec<usize>)>,
}

fn plan(launch: &Launch, change: &Change) -> Result<Plan> {
    if let Some(x) = change.xmx_mb
        && x < MIN_XMX_MB
    {
        bail!(tr!(
            format!("слишком мало памяти: {x} МБ (нужно хотя бы {MIN_XMX_MB} МБ)"),
            format!("too little memory: {x} MB (at least {MIN_XMX_MB} MB is needed)")
        ));
    }
    let xmx = change.xmx_mb.or(launch.xmx_mb);
    let xms = if change.xms_equal && xmx.is_some() {
        xmx
    } else {
        match (launch.xms_mb, xmx) {
            (Some(s), Some(x)) => Some(s.min(x)),
            (s, _) => s,
        }
    };
    let target = change.profile.filter(|p| *p != Profile::Custom).or_else(|| {
        // Aikar остаётся Aikar, но вариант для большой кучи следует за -Xmx.
        (launch.profile == Profile::Aikar).then_some(Profile::Aikar)
    });
    let flags = target.and_then(|p| {
        let new = p.flags(xmx.unwrap_or(0));
        // Флаги пользователя остаются, кроме тех, что задаёт новый набор (его значение главнее).
        let keys: Vec<String> = new.iter().map(|f| flag_key(f)).collect();
        let kept: Vec<usize> = (0..launch.flags.len())
            .filter(|&i| !launch.profile.owns(&launch.flags[i]) && !keys.contains(&flag_key(&launch.flags[i])))
            .collect();
        let mut all: Vec<&String> = new.iter().chain(kept.iter().map(|&i| &launch.flags[i])).collect();
        let mut old: Vec<&String> = launch.flags.iter().collect();
        all.sort();
        old.sort();
        // Тот же набор в другом порядке — не переставляем.
        (all != old).then_some((new, kept))
    });
    Ok(Plan { xms_mb: xms, xmx_mb: xmx, flags })
}

fn mem_flags(plan: &Plan) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(s) = plan.xms_mb {
        v.push(size_flag("-Xms", s));
    }
    if let Some(x) = plan.xmx_mb {
        v.push(size_flag("-Xmx", x));
    }
    v
}

/// Новая строка скрипта. Только память — правка на месте (пробелы и порядок остальных слов не меняются);
/// новые флаги — слова между java и `-jar` собираются заново: память, флаги набора, флаги пользователя
/// (в исходной записи, с кавычками).
fn rewrite_line(line: &str, toks: &[Tok], java_idx: usize, jar_flag: usize, bat: bool, plan: &Plan) -> String {
    let quote = |s: &str| if bat { create::bat_quote(s) } else { create::sh_quote(s) };
    let seg = &toks[java_idx + 1..jar_flag];
    let java_end = toks[java_idx].end;
    let mut out = line.to_string();
    if let Some((new, kept)) = &plan.flags {
        // Слова пользователя (не память) — в исходной записи.
        let user: Vec<&Tok> =
            seg.iter().filter(|t| !t.value.starts_with("-Xmx") && !t.value.starts_with("-Xms")).collect();
        let mut words = mem_flags(plan);
        words.extend(new.iter().map(|f| quote(f)));
        words.extend(kept.iter().map(|&i| line[user[i].start..user[i].end].to_string()));
        let mut middle = String::from(" ");
        for w in &words {
            middle.push_str(w);
            middle.push(' ');
        }
        out.replace_range(java_end..toks[jar_flag].start, &middle);
        return out;
    }
    // Правки (начало, конец, текст) справа налево.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let xmx_toks: Vec<&Tok> = seg.iter().filter(|t| t.value.starts_with("-Xmx")).collect();
    let xms_toks: Vec<&Tok> = seg.iter().filter(|t| t.value.starts_with("-Xms")).collect();
    if let Some(x) = plan.xmx_mb {
        edits.extend(xmx_toks.iter().map(|t| (t.start, t.end, size_flag("-Xmx", x))));
    }
    if let Some(s) = plan.xms_mb {
        edits.extend(xms_toks.iter().map(|t| (t.start, t.end, size_flag("-Xms", s))));
    }
    match (plan.xms_mb.filter(|_| xms_toks.is_empty()), plan.xmx_mb.filter(|_| xmx_toks.is_empty())) {
        (Some(s), Some(x)) => {
            edits.push((java_end, java_end, format!(" {} {}", size_flag("-Xms", s), size_flag("-Xmx", x))))
        }
        (Some(s), None) => {
            let at = xmx_toks.first().map_or(java_end, |t| t.start);
            let text = if at == java_end {
                format!(" {}", size_flag("-Xms", s))
            } else {
                format!("{} ", size_flag("-Xms", s))
            };
            edits.push((at, at, text));
        }
        (None, Some(x)) => {
            let at = xms_toks.last().map_or(java_end, |t| t.end);
            edits.push((at, at, format!(" {}", size_flag("-Xmx", x))));
        }
        (None, None) => {}
    }
    edits.sort_by_key(|e| std::cmp::Reverse((e.0, e.1)));
    for (s, e, text) in edits {
        out.replace_range(s..e, &text);
    }
    out
}

/// Новая команда `java … -jar X.jar …` (для серверов без скрипта).
fn rewrite_command(command: &[String], java_idx: usize, jar_flag: usize, plan: &Plan) -> Vec<String> {
    let seg = &command[java_idx + 1..jar_flag];
    let mut out: Vec<String> = command[..=java_idx].to_vec();
    if let Some((new, kept)) = &plan.flags {
        let user: Vec<&String> = seg.iter().filter(|a| !a.starts_with("-Xmx") && !a.starts_with("-Xms")).collect();
        out.extend(mem_flags(plan));
        out.extend(new.iter().cloned());
        out.extend(kept.iter().map(|&i| user[i].clone()));
    } else {
        let mut mid: Vec<String> = seg
            .iter()
            .map(|a| match (a.starts_with("-Xmx"), a.starts_with("-Xms")) {
                (true, _) => plan.xmx_mb.map_or_else(|| a.clone(), |x| size_flag("-Xmx", x)),
                (_, true) => plan.xms_mb.map_or_else(|| a.clone(), |s| size_flag("-Xms", s)),
                _ => a.clone(),
            })
            .collect();
        if let Some(x) = plan.xmx_mb.filter(|_| !seg.iter().any(|a| a.starts_with("-Xmx"))) {
            let at = mid.iter().rposition(|a| a.starts_with("-Xms")).map_or(0, |i| i + 1);
            mid.insert(at, size_flag("-Xmx", x));
        }
        if let Some(s) = plan.xms_mb.filter(|_| !seg.iter().any(|a| a.starts_with("-Xms"))) {
            let at = mid.iter().position(|a| a.starts_with("-Xmx")).unwrap_or(0);
            mid.insert(at, size_flag("-Xms", s));
        }
        out.extend(mid);
    }
    out.extend(command[jar_flag..].iter().cloned());
    out
}

/// Проверка, что новая строка читается так, как задумано (иначе — ошибка, файл не трогаем).
fn verify(old: &Launch, new: &Launch, plan: &Plan) -> Result<()> {
    let mut want: Vec<&String> = match &plan.flags {
        Some((set, kept)) => set.iter().chain(kept.iter().map(|&i| &old.flags[i])).collect(),
        None => old.flags.iter().collect(),
    };
    let mut got: Vec<&String> = new.flags.iter().collect();
    want.sort();
    got.sort();
    let ok = new.xmx_mb == plan.xmx_mb
        && new.xms_mb == plan.xms_mb
        && want == got
        && new.jar == old.jar
        && new.args == old.args
        && new.java == old.java;
    if ok {
        Ok(())
    } else {
        bail!(tr!(
            format!("не удалось собрать строку запуска: {}", new.line),
            format!("failed to build the launch line: {}", new.line)
        ))
    }
}

/// Результат и новый текст скрипта (если он меняется).
fn prepare(dir: &Path, command: &[String], change: &Change) -> Result<(Applied, Option<String>)> {
    let p = parse(dir, command)?;
    let plan = plan(&p.launch, change)?;
    let before = p.launch.line.clone();
    match &p.script {
        Some(s) => {
            let new_text = cu::replace_line(&s.text, s.line, |l| {
                *l = rewrite_line(l, &s.toks, p.java_idx, p.jar_flag, s.bat, &plan)
            });
            let after = new_text.lines().nth(s.line).unwrap_or_default().to_string();
            let script = Some(s.path.clone());
            if after == before {
                let a = Applied { script, before, after, new_command: None, backup: None, launch: p.launch };
                return Ok((a, None));
            }
            // Разбираем новую строку тем же кодом, что и при чтении.
            let r = cu::find_in_script(&new_text, s.bat).filter(|r| r.line == s.line && r.java_idx.is_some());
            let r = r.with_context(|| {
                tr!(
                    format!("строка запуска сломалась при изменении: {after}"),
                    format!("the launch line broke: {after}")
                )
            })?;
            let java_idx = r.java_idx.unwrap_or_default();
            let words: Vec<&str> = r.toks[java_idx + 1..r.jar_flag].iter().map(|t| t.value.as_str()).collect();
            let jvm = parse_jvm(&words, &script_name(&s.path))?;
            let args = r.toks[r.jar_flag + 2..].iter().map(|t| t.value.clone()).collect();
            let java = r.toks[java_idx].value.clone();
            let launch = make_launch(script.clone(), Some(s.line + 1), after.clone(), java, jvm, r.jar.value, args);
            verify(&p.launch, &launch, &plan)?;
            Ok((Applied { script, before, after, new_command: None, backup: None, launch }, Some(new_text)))
        }
        None => {
            let new = rewrite_command(command, p.java_idx, p.jar_flag, &plan);
            let launch = parse_command(&new, new.len() - (command.len() - p.jar_flag))?.launch;
            verify(&p.launch, &launch, &plan)?;
            let after = new.join(" ");
            let new_command = (new != command).then_some(new);
            Ok((Applied { script: None, before, after, new_command, backup: None, launch }, None))
        }
    }
}

/// Что получится после изменения — без записи на диск.
pub fn preview(dir: &Path, command: &[String], change: &Change) -> Result<Applied> {
    prepare(dir, command, change).map(|(a, _)| a)
}

/// Копия скрипта в `<dir>/.pechka-old/<скрипт>-ГГГГммдд-ЧЧММСС` (права копируются вместе с файлом).
fn backup_script(dir: &Path, script: &Path) -> Result<PathBuf> {
    let old_dir = dir.join(crate::plugins::OLD_DIR);
    std::fs::create_dir_all(&old_dir)?;
    let base = format!("{}-{}", script_name(script), chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let path = std::iter::once(old_dir.join(&base))
        .chain((1..).map(|i| old_dir.join(format!("{base}-{i}"))))
        .find(|p| !p.exists())
        .expect("free name");
    std::fs::copy(script, &path).with_context(|| {
        tr!(
            format!("не удалось сохранить копию {} в {}", script_name(script), crate::plugins::OLD_DIR),
            format!("failed to save a copy of {} to {}", script_name(script), crate::plugins::OLD_DIR)
        )
    })?;
    Ok(path)
}

/// Меняет память и флаги в строке запуска (сервер должен быть остановлен). Скрипт: сначала копия в
/// `.pechka-old/`, затем запись через временный файл (при ошибке старый скрипт остаётся на месте).
/// Команда из настроек: файлы не трогаются, новая команда — в [`Applied::new_command`], её сохраняет
/// вызывающая сторона. Если ничего не меняется — ничего не пишется.
pub fn apply(dir: &Path, command: &[String], change: &Change) -> Result<Applied> {
    let (mut a, new_text) = prepare(dir, command, change)?;
    if let (Some(script), Some(text)) = (&a.script, new_text) {
        a.backup = Some(backup_script(dir, script)?);
        cu::write_script(script, &text).with_context(|| {
            tr!(format!("не удалось записать {}", script_name(script)), format!("cannot write {}", script_name(script)))
        })?;
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("pechka-jvm-{tag}-{}", std::process::id()));
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

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Как у живого сервера пользователя (~/ServerMine/start.sh).
    const USER_SH: &str = "#!/bin/bash\ncd \"$(dirname \"$0\")\"\nexec ./jdk25/Contents/Home/bin/java -Xms2G -Xmx4G -jar purpur-26.3-2658.jar --nogui\n";

    /// Строка из docs.papermc.io/paper/aikars-flags.
    const PAPER_DOCS: &str = "java -Xms10G -Xmx10G -XX:+UseG1GC -XX:+ParallelRefProcEnabled -XX:MaxGCPauseMillis=200 -XX:+UnlockExperimentalVMOptions -XX:+DisableExplicitGC -XX:+AlwaysPreTouch -XX:G1NewSizePercent=30 -XX:G1MaxNewSizePercent=40 -XX:G1HeapRegionSize=8M -XX:G1ReservePercent=20 -XX:G1HeapWastePercent=5 -XX:G1MixedGCCountTarget=4 -XX:InitiatingHeapOccupancyPercent=15 -XX:G1MixedGCLiveThresholdPercent=90 -XX:G1RSetUpdatingPauseTimePercent=5 -XX:SurvivorRatio=32 -XX:+PerfDisableSharedMem -XX:MaxTenuringThreshold=1 -Dusing.aikars.flags=https://mcflags.emc.gs -Daikars.new.flags=true -jar paper.jar --nogui";

    fn sh(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4G"), Some(4096));
        assert_eq!(parse_size("4g"), Some(4096));
        assert_eq!(parse_size("4096M"), Some(4096));
        assert_eq!(parse_size("4096m"), Some(4096));
        assert_eq!(parse_size("4194304k"), Some(4096));
        assert_eq!(parse_size("4294967296"), Some(4096));
        assert_eq!(parse_size("1T"), Some(1024 * 1024));
        assert_eq!(parse_size("4.5G"), None);
        assert_eq!(parse_size("G"), None);
        assert_eq!(parse_size("0M"), None);
        assert_eq!(size_flag("-Xmx", 4096), "-Xmx4G");
        assert_eq!(size_flag("-Xms", 4608), "-Xms4608M");
    }

    #[test]
    fn aikar_flags_match_paper_docs() {
        let docs: Vec<String> =
            PAPER_DOCS.split(' ').filter(|w| w.starts_with("-XX:") || w.starts_with("-D")).map(String::from).collect();
        let mut ours = Profile::Aikar.flags(10 * 1024);
        let mut theirs = docs.clone();
        ours.sort();
        theirs.sort();
        assert_eq!(ours, theirs);
        // Порядок как в документации.
        assert_eq!(Profile::Aikar.flags(10 * 1024), docs);

        // Ровно 12G — базовый вариант, больше — для большой кучи.
        assert_eq!(Profile::Aikar.flags(AIKAR_LARGE_HEAP_MB), docs);
        let large = Profile::Aikar.flags(AIKAR_LARGE_HEAP_MB + 1);
        assert_eq!(large.len(), 20);
        for f in [
            "-XX:G1NewSizePercent=40",
            "-XX:G1MaxNewSizePercent=50",
            "-XX:G1HeapRegionSize=16M",
            "-XX:G1ReservePercent=15",
            "-XX:InitiatingHeapOccupancyPercent=20",
        ] {
            assert!(large.contains(&f.to_string()), "{f}");
        }
        assert!(!large.contains(&"-XX:G1HeapRegionSize=8M".to_string()));
        assert_eq!(large.iter().filter(|f| docs.contains(f)).count(), 15);
        assert!(Profile::None.flags(4096).is_empty() && Profile::Custom.flags(4096).is_empty());
        assert_eq!(Profile::Velocity.flags(512).len(), 6);
        for p in [Profile::Aikar, Profile::Velocity, Profile::Basic, Profile::None, Profile::Custom] {
            assert!(!p.label().is_empty() && !p.description().is_empty());
        }
    }

    #[test]
    fn profile_detection_any_order() {
        let mut base = Profile::Aikar.flags(4096);
        assert_eq!(detect_profile(&base), Profile::Aikar);
        base.reverse();
        assert_eq!(detect_profile(&base), Profile::Aikar);
        base.rotate_left(7);
        base.insert(3, "-Dfile.encoding=UTF-8".into());
        assert_eq!(detect_profile(&base), Profile::Aikar, "extra -D does not break detection");
        let mut large = Profile::Aikar.flags(16 * 1024);
        large.rotate_right(5);
        assert_eq!(detect_profile(&large), Profile::Aikar);
        // Без одного флага — уже не Aikar.
        let mut partial = Profile::Aikar.flags(4096);
        partial.retain(|f| f != "-XX:G1NewSizePercent=30");
        assert_eq!(detect_profile(&partial), Profile::Custom);
        // Basic входит в Aikar, но сам по себе — Basic.
        let mut basic = Profile::Basic.flags(4096);
        basic.reverse();
        assert_eq!(detect_profile(&basic), Profile::Basic);
        assert_eq!(detect_profile(&Profile::Velocity.flags(512)), Profile::Velocity);
        assert_eq!(detect_profile(&strs(&["-XX:+UseZGC", "-XX:+ZGenerational"])), Profile::Custom);
        assert_eq!(detect_profile(&strs(&["-Dfile.encoding=UTF-8", "-Xss4M"])), Profile::None);
        assert_eq!(detect_profile(&[]), Profile::None);
        // Basic плюс флаг, взятый из Aikar, — не Basic.
        let mut mixed = Profile::Basic.flags(0);
        mixed.push("-XX:G1NewSizePercent=30".into());
        assert_eq!(detect_profile(&mixed), Profile::Custom);
        // Свой -XX сверх набора — набор остаётся.
        let mut thp = Profile::Aikar.flags(4096);
        thp.push("-XX:+UseTransparentHugePages".into());
        assert_eq!(detect_profile(&thp), Profile::Aikar);

        assert_eq!(flag_key("-XX:+UseG1GC"), "-XX:UseG1GC");
        assert_eq!(flag_key("-XX:-UseG1GC"), "-XX:UseG1GC");
        assert_eq!(flag_key("-XX:G1ReservePercent=20"), "-XX:G1ReservePercent");
        assert_eq!(flag_key("-Dusing.aikars.flags=https://mcflags.emc.gs"), "-Dusing.aikars.flags");
        assert_eq!(flag_key("-Xss4M"), "-Xss4M");
    }

    #[test]
    fn profile_switch_drops_conflicting_user_flags() {
        let t = TempDir::new("conflict");
        let d = &t.0;
        // Basic + свой THP и -D → Aikar: свои флаги остаются после флагов набора.
        let line = format!(
            "java -Xmx4G {} -XX:+UseTransparentHugePages -Dx=1 -jar server.jar",
            Profile::Basic.flags(0).join(" ")
        );
        sh(d, "start.sh", &format!("#!/bin/sh\n{line}\n"));
        let cmd = strs(&["./start.sh"]);
        assert_eq!(read(d, &cmd).unwrap().profile, Profile::Basic);
        let a = apply(d, &cmd, &Change { xmx_mb: None, xms_equal: true, profile: Some(Profile::Aikar) }).unwrap();
        assert_eq!(
            a.after,
            format!(
                "java -Xms4G -Xmx4G {} -XX:+UseTransparentHugePages -Dx=1 -jar server.jar",
                Profile::Aikar.flags(4096).join(" ")
            )
        );
        assert_eq!(a.launch.profile, Profile::Aikar);
        assert_eq!(a.launch.extra_flags, ["-XX:+UseTransparentHugePages", "-Dx=1"]);
        // Свой -XX:MaxGCPauseMillis=50 при Velocity → Basic: значение набора главнее.
        let line =
            format!("java -Xmx1G {} -XX:MaxGCPauseMillis=50 -jar server.jar", Profile::Velocity.flags(0).join(" "));
        sh(d, "start.sh", &format!("#!/bin/sh\n{line}\n"));
        let l = read(d, &cmd).unwrap();
        assert_eq!((l.profile, l.extra_flags.clone()), (Profile::Velocity, strs(&["-XX:MaxGCPauseMillis=50"])));
        let a = apply(d, &cmd, &Change { xmx_mb: None, xms_equal: false, profile: Some(Profile::Basic) }).unwrap();
        assert_eq!(a.after, format!("java -Xmx1G {} -jar server.jar", Profile::Basic.flags(0).join(" ")));
    }

    #[test]
    fn read_user_script() {
        let t = TempDir::new("user");
        sh(&t.0, "start.sh", USER_SH);
        let l = read(&t.0, &strs(&["./start.sh"])).unwrap();
        assert_eq!(l.script, Some(t.0.join("start.sh")));
        assert_eq!(l.line_no, Some(3));
        assert_eq!(l.line, "exec ./jdk25/Contents/Home/bin/java -Xms2G -Xmx4G -jar purpur-26.3-2658.jar --nogui");
        assert_eq!(l.java, "./jdk25/Contents/Home/bin/java");
        assert_eq!((l.xms_mb, l.xmx_mb), (Some(2048), Some(4096)));
        assert!(l.flags.is_empty() && l.extra_flags.is_empty());
        assert_eq!(l.jar, "purpur-26.3-2658.jar");
        assert_eq!(l.args, ["--nogui"]);
        assert_eq!(l.profile, Profile::None);
        // Без команды — стандартное имя.
        assert_eq!(read(&t.0, &[]).unwrap(), l);
    }

    #[test]
    fn real_script_if_present() {
        // Живой сервер пользователя — только чтение.
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        if d.join("start.sh").is_file() && d.join("purpur-26.3-2658.jar").is_file() {
            let l = read(&d, &strs(&["./start.sh"])).unwrap();
            assert!(l.xmx_mb.is_some(), "{l:?}");
            assert!(l.jar.ends_with(".jar"));
        }
    }

    #[test]
    fn read_bat_and_aikar_script() {
        let t = TempDir::new("bat");
        let bat = "@echo off\r\nrem Generated\r\ncd /d \"%~dp0\"\r\n\"C:\\Program Files\\Java\\bin\\java.exe\" -Xms1G -Xmx3G -Dfile.encoding=UTF-8 -jar \"paper-1.21.4-232.jar\" nogui %*\r\npause\r\n";
        sh(&t.0, "start.bat", bat);
        let l = read(&t.0, &strs(&["cmd", "/C", "start.bat"])).unwrap();
        assert_eq!(l.line_no, Some(4));
        assert_eq!(l.java, "C:\\Program Files\\Java\\bin\\java.exe");
        assert_eq!((l.xms_mb, l.xmx_mb), (Some(1024), Some(3072)));
        assert_eq!(l.flags, ["-Dfile.encoding=UTF-8"]);
        assert_eq!(l.jar, "paper-1.21.4-232.jar");
        assert_eq!(l.args, ["nogui", "%*"]);
        assert_eq!(l.profile, Profile::None);

        // Флаги Aikar в перемешанном порядке и свой -D.
        let mut flags = Profile::Aikar.flags(16 * 1024);
        flags.reverse();
        let line = format!("java -Xmx16G -Dmy.prop=1 {} -Xms16G -jar paper.jar --nogui", flags.join(" "));
        sh(&t.0, "start.sh", &format!("#!/bin/sh\n{line}\n"));
        let l = read(&t.0, &strs(&["sh", "start.sh"])).unwrap();
        assert_eq!(l.profile, Profile::Aikar);
        assert_eq!(l.extra_flags, ["-Dmy.prop=1"]);
        assert_eq!((l.xms_mb, l.xmx_mb), (Some(16384), Some(16384)));
    }

    #[test]
    fn refuses_what_it_cannot_edit() {
        let t = TempDir::new("refuse");
        let d = &t.0;
        let cmd = strs(&["./start.sh"]);
        let err = |text: &str| {
            sh(d, "start.sh", text);
            format!("{:#}", read(d, &cmd).unwrap_err())
        };
        assert!(err("#!/bin/sh\n$JAVA -Xmx4G -jar server.jar\n").contains("$JAVA"));
        assert!(
            err("#!/bin/sh\njava @user_jvm_args.txt @libraries/net/forge/unix_args.txt \"$@\"\n")
                .contains("user_jvm_args")
        );
        assert!(err("#!/bin/sh\njava -Xmx$MEM -jar server.jar\n").contains("$MEM"));
        assert!(err("#!/bin/sh\nexec java $JAVA_OPTS -jar server.jar\n").contains("$JAVA_OPTS"));
        assert!(err("#!/bin/sh\njava -Xms4G -Xmx4G \\\n  -XX:+UseG1GC \\\n  -jar server.jar\n").contains('\\'));
        assert!(err("#!/bin/sh\necho hi\n").contains("-jar"));
        assert!(err("#!/bin/sh\njava -Xmx4.5G -jar server.jar\n").contains("-Xmx4.5G"));
        assert!(read(d, &strs(&["./missing.sh"])).is_err());
        assert!(read(d, &strs(&["./server", "-jar", "x.jar"])).is_err(), "no java before -jar");
        // Java из переменной, но параметры обычные — читать и менять можно (java не трогаем).
        sh(d, "start.sh", "#!/bin/sh\n\"$JAVA_HOME/bin/java\" -Xmx2G -jar server.jar\n");
        let l = read(d, &cmd).unwrap();
        assert_eq!(l.java, "$JAVA_HOME/bin/java");
        assert_eq!(check_java(d, &l.java), JavaCheck::Variable);
        let empty = TempDir::new("refuse-empty");
        assert!(read(&empty.0, &[]).is_err());
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn apply_round_trip_keeps_other_lines() {
        let t = TempDir::new("apply");
        let d = &t.0;
        let script = "#!/bin/bash\n# комментарий: java -Xmx1G -jar old.jar\ncd \"$(dirname \"$0\")\"\nexec ./jdk25/Contents/Home/bin/java -Xms2G -Xmx4G -jar purpur-26.3-2658.jar --nogui\necho stopped\n";
        let path = sh(d, "start.sh", script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        }
        let cmd = strs(&["./start.sh"]);

        // Предпросмотр ничего не пишет.
        let change = Change { xmx_mb: Some(8192), xms_equal: true, profile: Some(Profile::Aikar) };
        let p = preview(d, &cmd, &change).unwrap();
        assert!(p.changed() && p.backup.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), script);
        assert!(!d.join(".pechka-old").exists());

        let a = apply(d, &cmd, &change).unwrap();
        assert_eq!(a.after, p.after);
        let expected_line = format!(
            "exec ./jdk25/Contents/Home/bin/java -Xms8G -Xmx8G {} -jar purpur-26.3-2658.jar --nogui",
            Profile::Aikar.flags(8192).join(" ")
        );
        assert_eq!(a.after, expected_line);
        let text = std::fs::read_to_string(&path).unwrap();
        let (old_lines, new_lines): (Vec<&str>, Vec<&str>) = (script.lines().collect(), text.lines().collect());
        assert_eq!(old_lines.len(), new_lines.len());
        for (i, (o, n)) in old_lines.iter().zip(&new_lines).enumerate() {
            if i == 3 {
                assert_eq!(*n, expected_line);
            } else {
                assert_eq!(o, n, "line {i}");
            }
        }
        assert!(text.ends_with("\necho stopped\n"));
        // Копия старого скрипта.
        let backup = a.backup.clone().unwrap();
        assert!(backup.starts_with(d.join(".pechka-old")));
        assert!(backup.file_name().unwrap().to_string_lossy().starts_with("start.sh-"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), script);
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o750);

        let l = read(d, &cmd).unwrap();
        assert_eq!(l, a.launch);
        assert_eq!((l.profile, l.xms_mb, l.xmx_mb), (Profile::Aikar, Some(8192), Some(8192)));

        // Повторно то же — без изменений и без новой копии.
        let again = apply(d, &cmd, &change).unwrap();
        assert!(!again.changed() && again.backup.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        // 16G без смены набора: Aikar сам переходит на вариант для большой кучи.
        let a = apply(d, &cmd, &Change { xmx_mb: Some(16384), xms_equal: true, profile: None }).unwrap();
        assert!(a.after.contains("-XX:G1HeapRegionSize=16M") && !a.after.contains("-XX:G1HeapRegionSize=8M"));
        assert_eq!(a.launch.profile, Profile::Aikar);
        assert_ne!(a.backup, Some(backup), "second backup gets its own name");

        // Обратно на «без флагов»: остаётся только память.
        let a = apply(d, &cmd, &Change { xmx_mb: Some(4096), xms_equal: false, profile: Some(Profile::None) }).unwrap();
        assert_eq!(a.after, "exec ./jdk25/Contents/Home/bin/java -Xms4G -Xmx4G -jar purpur-26.3-2658.jar --nogui");
        assert_eq!(a.launch.profile, Profile::None);
    }

    #[test]
    fn memory_only_edits_in_place() {
        let t = TempDir::new("inplace");
        let d = &t.0;
        let cmd = strs(&["./start.sh"]);
        // Свои флаги (Custom) и странные пробелы — меняются только слова памяти.
        let line =
            "nohup java  -Xss4M\t-Xmx2048M -XX:+UseZGC -XX:+ZGenerational '-Dmotd=a b' -Xms1G -jar server.jar nogui";
        sh(d, "start.sh", &format!("#!/bin/sh\n{line}\n"));
        let l = read(d, &cmd).unwrap();
        assert_eq!(l.profile, Profile::Custom);
        assert_eq!(l.flags, ["-Xss4M", "-XX:+UseZGC", "-XX:+ZGenerational", "-Dmotd=a b"]);
        assert_eq!(l.extra_flags, ["-Xss4M", "-Dmotd=a b"]);
        let a = apply(d, &cmd, &Change { xmx_mb: Some(6144), xms_equal: false, profile: None }).unwrap();
        assert_eq!(a.after, line.replace("-Xmx2048M", "-Xmx6G"));
        let a = apply(d, &cmd, &Change { xmx_mb: None, xms_equal: true, profile: Some(Profile::Custom) }).unwrap();
        assert_eq!(a.after, line.replace("-Xmx2048M", "-Xmx6G").replace("-Xms1G", "-Xms6G"));

        // -Xms больше нового -Xmx — уменьшается, иначе Java не запустится.
        let a = apply(d, &cmd, &Change { xmx_mb: Some(3072), xms_equal: false, profile: None }).unwrap();
        assert_eq!((a.launch.xms_mb, a.launch.xmx_mb), (Some(3072), Some(3072)));

        // Смена набора: -XX свои уходят (Custom), -Xss и -D со своими кавычками остаются.
        let a = apply(d, &cmd, &Change { xmx_mb: None, xms_equal: true, profile: Some(Profile::Basic) }).unwrap();
        assert_eq!(
            a.after,
            format!(
                "nohup java -Xms3G -Xmx3G {} -Xss4M '-Dmotd=a b' -jar server.jar nogui",
                Profile::Basic.flags(0).join(" ")
            )
        );
        assert_eq!(a.launch.extra_flags, ["-Xss4M", "-Dmotd=a b"]);

        // Нет памяти вовсе — вставляется после java.
        sh(d, "start.sh", "#!/bin/sh\nexec java -jar server.jar\n");
        let a = apply(d, &cmd, &Change { xmx_mb: Some(2048), xms_equal: true, profile: None }).unwrap();
        assert_eq!(a.after, "exec java -Xms2G -Xmx2G -jar server.jar");
        sh(d, "start.sh", "#!/bin/sh\nexec java -Xms1G -jar server.jar\n");
        let a = apply(d, &cmd, &Change { xmx_mb: Some(2048), xms_equal: false, profile: None }).unwrap();
        assert_eq!(a.after, "exec java -Xms1G -Xmx2G -jar server.jar");
        sh(d, "start.sh", "#!/bin/sh\nexec java -Xmx1G -jar server.jar\n");
        let a = apply(d, &cmd, &Change { xmx_mb: None, xms_equal: true, profile: None }).unwrap();
        assert_eq!(a.after, "exec java -Xms1G -Xmx1G -jar server.jar");
        assert!(apply(d, &cmd, &Change { xmx_mb: Some(100), ..Default::default() }).is_err());
    }

    #[test]
    fn bat_crlf_preserved() {
        let t = TempDir::new("crlf");
        let d = &t.0;
        let bat = "@echo off\r\nrem Generated by pechka\r\ncd /d \"%~dp0\"\r\njava -Xms1G -Xmx2G -jar paper-1.21.4-232.jar nogui %*\r\npause\r\n";
        sh(d, "start.bat", bat);
        let cmd = strs(&["cmd", "/C", "start.bat"]);
        let a = apply(d, &cmd, &Change { xmx_mb: Some(4096), xms_equal: true, profile: Some(Profile::Aikar) }).unwrap();
        let text = std::fs::read_to_string(d.join("start.bat")).unwrap();
        assert_eq!(text.matches("\r\n").count(), 5);
        assert_eq!(text.matches('\n').count(), 5, "no bare LF");
        assert!(
            text.starts_with(
                "@echo off\r\nrem Generated by pechka\r\ncd /d \"%~dp0\"\r\njava -Xms4G -Xmx4G -XX:+UseG1GC"
            )
        );
        assert!(text.ends_with(" -jar paper-1.21.4-232.jar nogui %*\r\npause\r\n"));
        // В cmd значение с «/» — в кавычках.
        assert!(a.after.contains("\"-Dusing.aikars.flags=https://mcflags.emc.gs\""));
        let l = read(d, &cmd).unwrap();
        assert_eq!(l.profile, Profile::Aikar);
        assert_eq!(l.args, ["nogui", "%*"]);

        // Скрипт pechka (create.rs) на Unix тоже читается и меняется.
        let s = create::start_script("jre/a b/bin/java", 2048, "paper-1.21.4-232.jar", "--nogui", false);
        sh(d, "start.sh", &s);
        let a =
            apply(d, &strs(&["./start.sh"]), &Change { xmx_mb: Some(3072), xms_equal: true, profile: None }).unwrap();
        assert_eq!(a.after, "exec 'jre/a b/bin/java' -Xms3G -Xmx3G -jar paper-1.21.4-232.jar --nogui \"$@\"");
    }

    #[test]
    fn command_based_server() {
        let t = TempDir::new("cmd");
        let cmd = strs(&["java", "-Xms1G", "-Xmx2G", "-Dfoo=bar", "-jar", "purpur-26.3-2658.jar", "--nogui"]);
        let l = read(&t.0, &cmd).unwrap();
        assert_eq!((l.script.as_ref(), l.line_no), (None, None));
        assert_eq!((l.xms_mb, l.xmx_mb, l.profile), (Some(1024), Some(2048), Profile::None));
        assert_eq!(l.flags, ["-Dfoo=bar"]);

        let a = apply(&t.0, &cmd, &Change { xmx_mb: Some(4096), xms_equal: false, profile: None }).unwrap();
        assert_eq!(
            a.new_command.unwrap(),
            strs(&["java", "-Xms1G", "-Xmx4G", "-Dfoo=bar", "-jar", "purpur-26.3-2658.jar", "--nogui"])
        );
        assert!(a.script.is_none() && a.backup.is_none());
        assert!(std::fs::read_dir(&t.0).unwrap().next().is_none(), "nothing written");

        let a =
            apply(&t.0, &cmd, &Change { xmx_mb: Some(6144), xms_equal: true, profile: Some(Profile::Aikar) }).unwrap();
        let new = a.new_command.unwrap();
        assert_eq!(&new[..3], ["java", "-Xms6G", "-Xmx6G"]);
        assert_eq!(&new[new.len() - 4..], ["-Dfoo=bar", "-jar", "purpur-26.3-2658.jar", "--nogui"]);
        assert_eq!(read(&t.0, &new).unwrap().profile, Profile::Aikar);

        let none = apply(&t.0, &cmd, &Change::default()).unwrap();
        assert!(!none.changed() && none.new_command.is_none());

        let bare = strs(&["java", "-jar", "server.jar"]);
        let a = apply(&t.0, &bare, &Change { xmx_mb: Some(2048), xms_equal: true, profile: None }).unwrap();
        assert_eq!(a.new_command.unwrap(), strs(&["java", "-Xms2G", "-Xmx2G", "-jar", "server.jar"]));
    }

    #[test]
    fn recommendation_math() {
        assert_eq!(recommended_memory_mb(8 * 1024, Some(Core::Paper)), 6 * 1024);
        assert_eq!(recommended_memory_mb(16 * 1024, Some(Core::Purpur)), 12 * 1024);
        assert_eq!(recommended_memory_mb(32 * 1024, Some(Core::Fabric)), 16 * 1024, "capped at 16 GB");
        assert_eq!(recommended_memory_mb(64 * 1024, None), 16 * 1024);
        assert_eq!(recommended_memory_mb(32 * 1024, Some(Core::Velocity)), 512);
        // Реальный объём чуть меньше «круглого» и маленькие машины.
        assert_eq!(recommended_memory_mb(16_000, Some(Core::Paper)), 11_776);
        assert_eq!(recommended_memory_mb(4 * 1024, Some(Core::Paper)), 3 * 1024);
        assert_eq!(recommended_memory_mb(2 * 1024, Some(Core::Vanilla)), 1536);
        assert_eq!(recommended_memory_mb(1024, Some(Core::Vanilla)), 1024, "at least 1 GB");
        assert_eq!(recommended_memory_mb(0, None), 6 * 1024, "unknown RAM → as for 8 GB");
        for total in [8 * 1024, 16 * 1024, 32 * 1024] {
            assert!(total - u64::from(recommended_memory_mb(total, None)) >= 2048);
        }
        assert_eq!(recommended_profile(Some(Core::Paper)), Profile::Aikar);
        assert_eq!(recommended_profile(Some(Core::Purpur)), Profile::Aikar);
        assert_eq!(recommended_profile(Some(Core::Fabric)), Profile::Basic);
        assert_eq!(recommended_profile(Some(Core::Velocity)), Profile::Velocity);
    }

    #[test]
    fn required_java_table() {
        let paper = Some(Core::Paper);
        assert_eq!(known_required_java(paper, "26.3"), Some(25));
        assert_eq!(known_required_java(Some(Core::Purpur), "26.1.2"), Some(25));
        assert_eq!(known_required_java(paper, "1.21.11"), Some(21));
        assert_eq!(known_required_java(paper, "1.20.6"), Some(21));
        assert_eq!(known_required_java(paper, "1.20.4"), Some(17));
        assert_eq!(known_required_java(Some(Core::Fabric), "1.18.2"), Some(17));
        assert_eq!(known_required_java(Some(Core::Vanilla), "1.17.1"), Some(16));
        assert_eq!(known_required_java(paper, "1.16.5"), Some(8));
        assert_eq!(known_required_java(None, "1.21.11-pre5"), Some(21));
        assert_eq!(known_required_java(None, "25w45a"), None);
        assert_eq!(known_required_java(None, ""), None);
        let v = Some(Core::Velocity);
        assert_eq!(known_required_java(v, "4.3.1"), Some(25));
        assert_eq!(known_required_java(v, "3.5.1"), Some(21));
        assert_eq!(known_required_java(v, "3.4.0-SNAPSHOT"), Some(17));
    }

    fn launch_of(line: &str) -> Launch {
        let words: Vec<String> = line.split(' ').map(String::from).collect();
        read(Path::new("."), &words).unwrap()
    }

    #[test]
    fn warnings() {
        let java = |major, bits64| JavaCheck::Found { major: Some(major), bits64: Some(bits64) };
        let paper = Some(Core::Paper);

        // Всё хорошо: Aikar, -Xms = -Xmx, Java 25 для 26.3.
        let good =
            launch_of(&format!("java -Xms6G -Xmx6G {} -jar paper.jar --nogui", Profile::Aikar.flags(6144).join(" ")));
        let a = advise_with(&good, paper, 8192, java(25, true), Some(25));
        assert!(a.warnings.is_empty() && a.hints.is_empty(), "{a:?}");
        assert_eq!((a.recommended_mb, a.java_major, a.recommended_profile), (6144, Some(25), Profile::Aikar));
        assert_eq!(a.suggested_change(), Change { xmx_mb: Some(6144), xms_equal: true, profile: Some(Profile::Aikar) });

        // -Xmx больше, чем ОЗУ − 1 ГБ.
        let big = launch_of("java -Xms2G -Xmx8G -jar paper.jar");
        let a = advise_with(&big, Some(Core::Fabric), 8192, java(21, true), Some(21));
        assert_eq!(a.warnings.len(), 1, "{a:?}");
        assert!(a.warnings[0].contains("-Xmx"));
        assert!(advise_with(&big, Some(Core::Fabric), 9216, java(21, true), Some(21)).warnings.is_empty());

        // Aikar с -Xms ≠ -Xmx.
        let uneven = launch_of(&format!("java -Xms2G -Xmx6G {} -jar paper.jar", Profile::Aikar.flags(6144).join(" ")));
        let a = advise_with(&uneven, paper, 16384, java(25, true), Some(25));
        assert_eq!(a.warnings.len(), 1, "{a:?}");
        assert!(a.warnings[0].contains("-Xms"));

        // Старая Java, 32 бита.
        let user = launch_of("java -Xms2G -Xmx4G -jar purpur-26.3-2658.jar --nogui");
        let a = advise_with(&user, Some(Core::Purpur), 16384, java(21, false), Some(25));
        assert_eq!(a.warnings.len(), 2, "{a:?}");
        assert!(a.warnings.iter().any(|w| w.contains("32")));
        assert!(a.warnings.iter().any(|w| w.contains("21") && w.contains("25")));
        // Paper / Purpur без Aikar — совет, не предупреждение.
        assert!(a.hints.iter().any(|h| h.contains("Aikar")));
        assert!(advise_with(&user, Some(Core::Vanilla), 16384, java(25, true), Some(25)).hints.is_empty());

        // Нет Java / не запускается / переменная.
        let a = advise_with(&user, paper, 16384, JavaCheck::NotFound, Some(25));
        assert!(a.warnings.iter().any(|w| w.contains("java")) && a.java_major.is_none());
        assert_eq!(advise_with(&user, paper, 16384, JavaCheck::Broken, Some(25)).warnings.len(), 1);
        let a = advise_with(&user, Some(Core::Vanilla), 16384, JavaCheck::Variable, Some(25));
        assert!(a.warnings.is_empty() && a.hints.len() == 1);

        // Нет -Xmx, мало памяти, слишком много памяти, большая куча с базовым Aikar.
        let a = advise_with(&launch_of("java -jar server.jar"), Some(Core::Vanilla), 16384, java(25, true), None);
        assert!(a.warnings.is_empty() && a.hints.iter().any(|h| h.contains("-Xmx")));
        let a =
            advise_with(&launch_of("java -Xmx512M -jar server.jar"), Some(Core::Vanilla), 16384, java(25, true), None);
        assert_eq!(a.warnings.len(), 1);
        let a = advise_with(
            &launch_of("java -Xmx512M -jar velocity.jar"),
            Some(Core::Velocity),
            16384,
            java(25, true),
            None,
        );
        assert!(a.warnings.is_empty() && a.hints.is_empty(), "512M is fine for a proxy: {a:?}");
        let huge = format!("java -Xms20G -Xmx20G {} -jar paper.jar", Profile::Aikar.flags(4096).join(" "));
        let a = advise_with(&launch_of(&huge), paper, 65536, java(25, true), Some(25));
        assert!(a.warnings.is_empty());
        assert_eq!(a.hints.len(), 2, "{a:?}");
    }

    #[test]
    fn java_resolution_and_probe() {
        let t = TempDir::new("java");
        let d = &t.0;
        assert_eq!(resolve_java(d, "./jdk25/Contents/Home/bin/java"), Some(d.join("jdk25/Contents/Home/bin/java")));
        assert_eq!(resolve_java(d, "jre/bin/java"), Some(d.join("jre/bin/java")));
        assert_eq!(resolve_java(d, "$JAVA"), None);
        assert_eq!(check_java(d, "./nope/bin/java"), JavaCheck::NotFound);
        assert_eq!(bits64_from("OpenJDK 64-Bit Server VM Temurin-25+36 (build 25+36-LTS, mixed mode)"), Some(true));
        assert_eq!(bits64_from("Java HotSpot(TM) Client VM (build 25.391-b13, mixed mode)"), Some(false));
        assert_eq!(bits64_from("something"), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(d.join("jdk/bin")).unwrap();
            let fake = d.join("jdk/bin/java");
            std::fs::write(
                &fake,
                "#!/bin/sh\necho 'openjdk version \"21.0.5\" 2024-10-15' >&2\necho 'OpenJDK 64-Bit Server VM (build 21.0.5+11-LTS, mixed mode)' >&2\n",
            )
            .unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(check_java(d, "./jdk/bin/java"), JavaCheck::Found { major: Some(21), bits64: Some(true) });
            std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
            assert_eq!(check_java(d, "./jdk/bin/java"), JavaCheck::Broken);

            sh(d, "start.sh", "#!/bin/sh\nexec ./jdk/bin/java -Xmx2G -jar purpur-26.3-2658.jar\n");
            let l = read(d, &[]).unwrap();
            let a = advise(d, &l, Some(Core::Purpur), Some("26.3"));
            assert_eq!(a.java_required, Some(25));
            assert!(a.warnings.iter().any(|w| w.contains("jdk/bin/java")), "{a:?}");
        }
    }
}
