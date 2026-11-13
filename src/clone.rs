//! Тестовая копия сервера: полная копия папки локального сервера в соседнюю папку `<dir>-test`
//! на свободных портах — чтобы сначала попробовать там обновление ядра или плагины.
//!
//! Два шага: [`plan`] — посчитать, куда и на каких портах будет копия и сколько она займёт
//! (ничего не меняет), [`run`] — скопировать. Код блокирующий: UI вызывает его из spawn_blocking.
//!
//! Не копируются: папка бэкапов (если лежит внутри сервера), `logs/`, `crash-reports/`, `cache/`,
//! служебные `.mctui-old/`, `.mctui-before-restore/`, `.mctui-before-reset/`, все `session.lock`
//! и недописанные бэкапы (`*.tar.gz.part`, `*.snap.partial`). Остальное копируется настоящими копиями
//! (не жёсткими ссылками — копия независима), с правами доступа и временем изменения файлов;
//! символьные ссылки копируются как ссылки.

use crate::config::{Kind, ServerConfig};
use crate::properties::Properties;
use crate::tr;
use crate::velocity;
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Сколько места оставить свободным сверх размера копии.
const RESERVE: u64 = 200 * 1024 * 1024;
/// Метка в конце motd копии.
const TEST_MARK: &str = " [test]";
/// Папки верхнего уровня, которые не копируются.
const SKIP_TOP: &[&str] = &["logs", "crash-reports", "cache"];
/// Служебные папки mctui — не копируются на любой глубине.
const SKIP_ANY: &[&str] = &[crate::plugins::OLD_DIR, ".mctui-before-restore", crate::world::RESET_STASH_DIR];
/// Порты Minecraft / Velocity по умолчанию.
const DEFAULT_PORT: u16 = 25565;
const DEFAULT_RCON_PORT: u16 = 25575;
const DEFAULT_PROXY_PORT: u16 = 25577;

/// Что и куда будет скопировано.
#[derive(Debug, Clone)]
pub struct ClonePlan {
    /// Папка исходного сервера (абсолютный путь).
    pub source_dir: PathBuf,
    /// Папка копии (соседняя `<dir>-test`, `<dir>-test-2`, …); пока не существует.
    pub dest: PathBuf,
    /// Имя копии в servers.toml.
    pub name: String,
    /// server-port копии (для Velocity — порт `bind`).
    pub port: u16,
    /// rcon.port копии, если в исходнике enable-rcon=true.
    pub rcon_port: Option<u16>,
    /// query.port копии, если в исходнике enable-query=true.
    pub query_port: Option<u16>,
    /// Сколько байт и файлов будет скопировано.
    pub bytes: u64,
    pub files: u64,
    /// Что не копируется (пути относительно папки сервера, у папок — `/` в конце).
    pub skipped: Vec<String>,
    /// Свободно на диске, где будет копия (None — не удалось узнать).
    pub free_space: Option<u64>,
}

/// Считает план копии `src`. `all` — все серверы из конфига (их порты и имена заняты).
pub fn plan(src: &ServerConfig, all: &[ServerConfig]) -> Result<ClonePlan> {
    plan_with(src, all, &port_free)
}

/// Копирует сервер по плану и возвращает конфиг копии (его ещё нужно добавить в servers.toml).
/// `progress(скопировано байт, всего байт)` вызывается после каждого файла.
/// Копия пишется в `<dest>.partial` и переименовывается в конце; при ошибке недоделанная папка удаляется.
pub fn run(plan: &ClonePlan, src: &ServerConfig, progress: &mut dyn FnMut(u64, u64)) -> Result<ServerConfig> {
    if src.kind != Kind::Local {
        bail!("{}", tr!("копировать можно только локальный сервер", "only a local server can be copied"));
    }
    let source = canonical(src.dir());
    if source != plan.source_dir {
        bail!(
            "{}",
            tr!(
                "план копии составлен для другой папки — пересчитайте его",
                "the copy plan was made for another folder — make it again"
            )
        );
    }
    if exists(&plan.dest) {
        bail!(
            "{}",
            tr!(
                format!("папка {} уже существует", plan.dest.display()),
                format!("{} already exists", plan.dest.display())
            )
        );
    }
    let partial = partial_path(&plan.dest);
    if exists(&partial) {
        bail!(
            "{}",
            tr!(
                format!("осталась недоделанная копия {} — удалите её", partial.display()),
                format!("an unfinished copy {} is left over — delete it", partial.display())
            )
        );
    }
    check_space(free_space(&plan.dest), plan.bytes)?;

    let ex = Excludes::new(src, &plan.source_dir);
    std::fs::create_dir(&partial).with_context(|| {
        tr!(format!("не создать {}", partial.display()), format!("cannot create {}", partial.display()))
    })?;
    let res = (|| -> Result<()> {
        let mut c = Copier { ex: &ex, done: 0, total: plan.bytes, progress };
        (c.progress)(0, c.total);
        c.dir(&plan.source_dir, &partial, Path::new(""))?;
        if let Ok(m) = std::fs::metadata(&plan.source_dir) {
            let _ = std::fs::set_permissions(&partial, m.permissions());
        }
        if partial.join(velocity::CONFIG).is_file() {
            velocity::set_bind_port(&partial, plan.port).context(velocity::CONFIG)?;
        } else {
            rewrite_properties(&partial, plan)?;
        }
        std::fs::rename(&partial, &plan.dest).with_context(|| {
            tr!(
                format!("не переименовать {} в {}", partial.display(), plan.dest.display()),
                format!("cannot rename {} to {}", partial.display(), plan.dest.display())
            )
        })?;
        let total = c.total.max(c.done);
        (c.progress)(total, total);
        Ok(())
    })();
    if let Err(e) = res {
        remove_partial(&partial);
        return Err(e);
    }
    Ok(copy_config(src, plan))
}

// ───────────────────────────── план ─────────────────────────────

fn plan_with(src: &ServerConfig, all: &[ServerConfig], free: &dyn Fn(u16, bool) -> bool) -> Result<ClonePlan> {
    if src.kind != Kind::Local {
        bail!("{}", tr!("копировать можно только локальный сервер", "only a local server can be copied"));
    }
    if !src.dir().is_dir() {
        bail!(
            "{}",
            tr!(
                format!("папка сервера не найдена: {}", src.dir().display()),
                format!("server folder not found: {}", src.dir().display())
            )
        );
    }
    let source_dir = canonical(src.dir());
    let dest = pick_dest(&source_dir, all)?;
    let name = unique_name(&src.name, all);

    // Порты: заняты все порты настроенных серверов (и самого исходника), плюс уже выбранные.
    let mut taken: HashSet<u16> = all.iter().flat_map(used_ports).collect();
    taken.extend(used_ports(src));
    let ports = Ports::read(&source_dir);
    let no_port = || anyhow::anyhow!("{}", tr!("не нашлось свободного порта", "no free port found"));
    let port = pick_port(ports.server, &taken, &|p| free(p, false)).ok_or_else(no_port)?;
    taken.insert(port);
    let mut rcon_port = None;
    let mut query_port = None;
    if !ports.proxy {
        if ports.rcon_enabled {
            let p = pick_port(ports.rcon, &taken, &|p| free(p, false)).ok_or_else(no_port)?;
            taken.insert(p);
            rcon_port = Some(p);
        }
        if ports.query_enabled {
            let p = pick_port(ports.query, &taken, &|p| free(p, true)).ok_or_else(no_port)?;
            taken.insert(p);
            query_port = Some(p);
        }
    }

    let ex = Excludes::new(src, &source_dir);
    let mut sz = Sizer::default();
    sz.dir(&ex, &source_dir, Path::new(""))?;
    sz.skipped.sort();
    Ok(ClonePlan {
        source_dir,
        free_space: free_space(&dest),
        dest,
        name,
        port,
        rcon_port,
        query_port,
        bytes: sz.bytes,
        files: sz.files,
        skipped: sz.skipped,
    })
}

/// Соседняя папка `<dir>-test` (`-test-2`, …), которой нет на диске и которую не занимает другой сервер.
fn pick_dest(dir: &Path, all: &[ServerConfig]) -> Result<PathBuf> {
    let (Some(parent), Some(base)) = (dir.parent(), dir.file_name()) else {
        bail!(
            "{}",
            tr!(
                format!("у папки {} нет соседней — копию не положить", dir.display()),
                format!("{} has no parent folder for the copy", dir.display())
            )
        );
    };
    let base = base.to_string_lossy();
    for n in 1..=99 {
        let d = parent.join(if n == 1 { format!("{base}-test") } else { format!("{base}-test-{n}") });
        let configured = all.iter().any(|s| s.kind == Kind::Local && canonical(s.dir()) == d);
        if !configured && !exists(&d) && !exists(&partial_path(&d)) {
            return Ok(d);
        }
    }
    bail!("{}", tr!("слишком много тестовых копий рядом с сервером", "too many test copies next to the server"))
}

/// `<имя> (тест)`, при совпадении — `<имя> (тест 2)`, …
fn unique_name(base: &str, all: &[ServerConfig]) -> String {
    let taken = |n: &str| all.iter().any(|s| s.name == n);
    let first = tr!(format!("{base} (тест)"), format!("{base} (test)"));
    if !taken(&first) {
        return first;
    }
    (2..).map(|n| tr!(format!("{base} (тест {n})"), format!("{base} (test {n})"))).find(|n| !taken(n)).unwrap_or(first)
}

/// Порты сервера, прочитанные из его файлов.
struct Ports {
    proxy: bool,
    server: u16,
    rcon_enabled: bool,
    rcon: u16,
    query_enabled: bool,
    query: u16,
}

impl Ports {
    fn read(dir: &Path) -> Self {
        if dir.join(velocity::CONFIG).is_file() {
            let port = velocity::proxy_port(dir).unwrap_or(DEFAULT_PROXY_PORT);
            return Ports { proxy: true, server: port, rcon_enabled: false, rcon: 0, query_enabled: false, query: 0 };
        }
        let props = Properties::load(&dir.join("server.properties")).unwrap_or_default();
        let port = |k: &str| props.get(k).and_then(|v| v.trim().parse::<u16>().ok());
        let on = |k: &str| props.get(k).is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
        let server = port("server-port").unwrap_or(DEFAULT_PORT);
        Ports {
            proxy: false,
            server,
            rcon_enabled: on("enable-rcon"),
            rcon: port("rcon.port").unwrap_or(DEFAULT_RCON_PORT),
            query_enabled: on("enable-query"),
            query: port("query.port").unwrap_or(server),
        }
    }
}

/// Порты, которые занимает (или займёт, если включить RCON / query) настроенный сервер.
fn used_ports(s: &ServerConfig) -> Vec<u16> {
    match s.kind {
        Kind::Rcon => {
            let local = matches!(s.host.as_str(), "127.0.0.1" | "localhost" | "::1" | "0.0.0.0");
            if local { vec![s.port] } else { vec![] }
        }
        Kind::Local => {
            let p = Ports::read(s.dir());
            if p.proxy { vec![p.server] } else { vec![p.server, p.rcon, p.query] }
        }
    }
}

/// Первый порт после `base`, не занятый настроенными серверами и свободный прямо сейчас.
fn pick_port(base: u16, taken: &HashSet<u16>, free: &dyn Fn(u16) -> bool) -> Option<u16> {
    (u32::from(base) + 1..=u32::from(u16::MAX))
        .filter_map(|p| u16::try_from(p).ok())
        .find(|p| !taken.contains(p) && free(*p))
}

/// Свободен ли порт сейчас: пробуем занять его (TCP — на 0.0.0.0 и 127.0.0.1, UDP — на 0.0.0.0).
fn port_free(port: u16, udp: bool) -> bool {
    if udp {
        return std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok();
    }
    // По очереди: первый слушатель должен закрыться до второй попытки.
    let any = std::net::TcpListener::bind(("0.0.0.0", port)).is_ok();
    any && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Свободное место на диске, где будет лежать `dest`.
fn free_space(dest: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list().ok()?;
    crate::core::disk_free(&disks, dest.parent()?)
}

fn check_space(free: Option<u64>, bytes: u64) -> Result<()> {
    let need = bytes.saturating_add(RESERVE);
    match free {
        Some(f) if f < need => bail!(
            "{}",
            tr!(
                format!("мало места на диске: нужно {} МиБ, свободно {} МиБ", need >> 20, f >> 20),
                format!("not enough disk space: need {} MiB, {} MiB free", need >> 20, f >> 20)
            )
        ),
        _ => Ok(()),
    }
}

// ───────────────────────────── что не копируется ─────────────────────────────

struct Excludes {
    /// Папка бэкапов относительно папки сервера (если внутри неё и не совпадает с ней).
    backup_rel: Option<PathBuf>,
    /// Бэкапы лежат прямо в папке сервера (`backup_dir = "."`): пропускаем их файлы по имени.
    backups_in_root: bool,
    prefix: String,
}

impl Excludes {
    fn new(src: &ServerConfig, source_dir: &Path) -> Self {
        let b = src.backup_dir();
        let rel = b
            .strip_prefix(src.dir())
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| canonical(&b).strip_prefix(source_dir).ok().map(Path::to_path_buf))
            .map(|r| r.components().filter(|c| !matches!(c, std::path::Component::CurDir)).collect::<PathBuf>());
        let backups_in_root = rel.as_ref().is_some_and(|r| r.as_os_str().is_empty());
        Excludes {
            backup_rel: rel.filter(|r| !r.as_os_str().is_empty()),
            backups_in_root,
            prefix: crate::backup::file_prefix(&src.name),
        }
    }

    /// Пропустить ли элемент `rel` (путь относительно папки сервера) с именем `name`.
    fn skip(&self, rel: &Path, name: &str) -> bool {
        let top = rel.components().count() == 1;
        (top && SKIP_TOP.contains(&name))
            || SKIP_ANY.contains(&name)
            || name == "session.lock"
            || name.ends_with(".tar.gz.part")
            || name.ends_with(".snap.partial")
            || self.backup_rel.as_deref() == Some(rel)
            || (top && self.backups_in_root && crate::backup::backup_kind(name, &self.prefix).is_some())
    }
}

/// Обход для плана: размер копируемого и список пропущенного.
#[derive(Default)]
struct Sizer {
    bytes: u64,
    files: u64,
    skipped: Vec<String>,
}

impl Sizer {
    fn dir(&mut self, ex: &Excludes, dir: &Path, rel: &Path) -> Result<()> {
        for (path, name, rel, meta) in entries(dir, rel)? {
            if ex.skip(&rel, &name) {
                let mut s = rel.to_string_lossy().replace('\\', "/");
                if meta.is_dir() {
                    s.push('/');
                }
                self.skipped.push(s);
            } else if meta.is_dir() {
                self.dir(ex, &path, &rel)?;
            } else if meta.is_file() || meta.file_type().is_symlink() {
                self.files += 1;
                self.bytes += if meta.is_file() { meta.len() } else { 0 };
            }
        }
        Ok(())
    }
}

/// Содержимое папки по алфавиту: (путь, имя, путь относительно сервера, метаданные без перехода по ссылкам).
fn entries(dir: &Path, rel: &Path) -> Result<Vec<(PathBuf, String, PathBuf, std::fs::Metadata)>> {
    let rd = std::fs::read_dir(dir)
        .with_context(|| tr!(format!("не читается {}", dir.display()), format!("cannot read {}", dir.display())))?;
    let mut v = vec![];
    for e in rd {
        let e = e.with_context(|| dir.display().to_string())?;
        let path = e.path();
        let meta = std::fs::symlink_metadata(&path).with_context(|| path.display().to_string())?;
        let name = e.file_name().to_string_lossy().into_owned();
        v.push((path, name, rel.join(e.file_name()), meta));
    }
    v.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(v)
}

// ───────────────────────────── копирование ─────────────────────────────

struct Copier<'a> {
    ex: &'a Excludes,
    done: u64,
    total: u64,
    progress: &'a mut dyn FnMut(u64, u64),
}

impl Copier<'_> {
    fn dir(&mut self, src: &Path, dst: &Path, rel: &Path) -> Result<()> {
        for (path, name, rel, meta) in entries(src, rel)? {
            if self.ex.skip(&rel, &name) {
                continue;
            }
            let to = dst.join(&name);
            let ft = meta.file_type();
            if ft.is_symlink() {
                copy_link(&path, &to)?;
            } else if ft.is_dir() {
                std::fs::create_dir(&to).with_context(|| to.display().to_string())?;
                self.dir(&path, &to, &rel)?;
                // Права папки — после содержимого (папка может быть только для чтения).
                let _ = std::fs::set_permissions(&to, meta.permissions());
            } else if ft.is_file() {
                // std::fs::copy переносит и права доступа (start.sh остаётся исполняемым).
                std::fs::copy(&path, &to).with_context(|| format!("{} → {}", path.display(), to.display()))?;
                if let Ok(t) = meta.modified() {
                    set_mtime(&to, t);
                }
                self.done += meta.len();
                (self.progress)(self.done, self.total.max(self.done));
            }
            // Сокеты, FIFO и прочее не копируются.
        }
        Ok(())
    }
}

fn set_mtime(p: &Path, t: std::time::SystemTime) {
    let f = std::fs::File::options().write(true).open(p).or_else(|_| std::fs::File::open(p));
    if let Ok(f) = f {
        let _ = f.set_modified(t);
    }
}

/// Копирует саму символьную ссылку (не то, на что она указывает).
fn copy_link(src: &Path, dst: &Path) -> Result<()> {
    let target = std::fs::read_link(src).with_context(|| src.display().to_string())?;
    #[cfg(unix)]
    let r = std::os::unix::fs::symlink(&target, dst);
    #[cfg(windows)]
    let r = if std::fs::metadata(src).is_ok_and(|m| m.is_dir()) {
        std::os::windows::fs::symlink_dir(&target, dst)
    } else {
        std::os::windows::fs::symlink_file(&target, dst)
    };
    r.with_context(|| {
        tr!(
            format!("не скопировать ссылку {} → {}", src.display(), target.display()),
            format!("cannot copy link {} → {}", src.display(), target.display())
        )
    })
}

/// Удаляет недоделанную копию (папки только для чтения сначала делаются доступными для записи).
fn remove_partial(p: &Path) {
    #[cfg(unix)]
    fn writable(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let Ok(m) = std::fs::symlink_metadata(p) else { return };
        if m.is_dir() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(m.permissions().mode() | 0o700));
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    writable(&e.path());
                }
            }
        }
    }
    #[cfg(unix)]
    writable(p);
    let _ = std::fs::remove_dir_all(p);
}

// ───────────────────────────── настройки копии ─────────────────────────────

/// server.properties копии: новые порты, новый пароль RCON, метка в motd. Порядок строк и комментарии сохраняются.
fn rewrite_properties(dir: &Path, plan: &ClonePlan) -> Result<()> {
    let path = dir.join("server.properties");
    // Нет файла — создаём с портом, иначе сервер при первом запуске займёт 25565, как исходник.
    let mut p = if path.is_file() {
        Properties::load(&path).with_context(|| path.display().to_string())?
    } else {
        Properties::default()
    };
    p.set("server-port", &plan.port.to_string());
    if let Some(r) = plan.rcon_port {
        p.set("rcon.port", &r.to_string());
        p.set("rcon.password", &crate::daemon::random_token()[..32]);
    }
    if let Some(q) = plan.query_port {
        p.set("query.port", &q.to_string());
    }
    if let Some(m) = p.get("motd").filter(|m| !m.trim().is_empty() && !m.ends_with(TEST_MARK)).map(str::to_owned) {
        p.set("motd", &format!("{m}{TEST_MARK}"));
    }
    p.save(&path).with_context(|| path.display().to_string())
}

/// Конфиг копии: как у исходника, но без автозапуска, расписаний и копий бэкапов в облако.
fn copy_config(src: &ServerConfig, plan: &ClonePlan) -> ServerConfig {
    let rebase = |s: &str| -> Option<String> {
        let p = Path::new(s);
        if !p.is_absolute() {
            return None;
        }
        let rel = p.strip_prefix(src.dir()).or_else(|_| p.strip_prefix(&plan.source_dir)).ok()?;
        Some(plan.dest.join(rel).display().to_string())
    };
    let mut c = src.clone();
    c.name = plan.name.clone();
    c.dir = Some(plan.dest.clone());
    // Относительная команда остаётся как есть (она относительно папки сервера), абсолютные пути внутрь
    // исходника переводятся в копию.
    c.command = src.command.iter().map(|a| rebase(a).unwrap_or_else(|| a.clone())).collect();
    // Абсолютная папка бэкапов вне сервера — общая с исходником; бэкапы копии держим у неё внутри.
    if Path::new(&src.backup_dir).is_absolute() {
        c.backup_dir = rebase(&src.backup_dir).unwrap_or_else(|| "backups".into());
    }
    c.autostart = false;
    c.backup_copy_to.clear();
    c.backup_every_hours = 0;
    c.restart_times.clear();
    c.backup_keep = 3;
    c
}

// ───────────────────────────── мелочи ─────────────────────────────

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mctui-clone-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, text: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn server(name: &str, dir: &Path) -> ServerConfig {
        ServerConfig::new_local(name, dir.to_path_buf(), vec!["./start.sh".into()], vec!["world".into()])
    }

    fn all_free(_: u16, _: bool) -> bool {
        true
    }

    fn no_progress() -> impl FnMut(u64, u64) {
        |_, _| {}
    }

    #[test]
    fn skips_service_dirs_and_copies_the_rest() {
        let root = tmp("excl");
        let s = root.join("srv");
        write(&s.join("server.properties"), "server-port=25565\n");
        write(&s.join("start.sh"), "#!/bin/sh\n");
        write(&s.join("world/level.dat"), "level");
        write(&s.join("world/session.lock"), "lock");
        write(&s.join("world_nether/session.lock"), "lock");
        write(&s.join("world/region/r.0.0.mca"), &"x".repeat(1000));
        write(&s.join("logs/latest.log"), "log");
        write(&s.join("crash-reports/c.txt"), "crash");
        write(&s.join("cache/mojang.jar"), "jar");
        write(&s.join(".mctui-old/purpur-1.jar"), "old");
        write(&s.join(".mctui-before-restore/20260101/world/level.dat"), "old");
        write(&s.join(".mctui-before-reset/x/level.dat"), "old");
        write(&s.join("plugins/.mctui-old/Foo.jar"), "old");
        write(&s.join("plugins/Foo.jar"), "foo");
        write(&s.join("plugins/Foo/logs/keep.log"), "nested logs are plugin data");
        write(&s.join("backups/srv-20260101-000000.tar.gz"), "backup");
        write(&s.join("half.tar.gz.part"), "part");
        fs::create_dir_all(s.join("srv-20260101-000000.snap.partial")).unwrap();
        let cfg = server("srv", &s);

        let plan = plan_with(&cfg, std::slice::from_ref(&cfg), &all_free).unwrap();
        assert_eq!(plan.dest.file_name().unwrap(), "srv-test");
        for want in [
            ".mctui-before-reset/",
            ".mctui-before-restore/",
            ".mctui-old/",
            "backups/",
            "cache/",
            "crash-reports/",
            "half.tar.gz.part",
            "logs/",
            "plugins/.mctui-old/",
            "srv-20260101-000000.snap.partial/",
            "world/session.lock",
            "world_nether/session.lock",
        ] {
            assert!(plan.skipped.iter().any(|x| x == want), "нет {want} в {:?}", plan.skipped);
        }
        // server.properties, start.sh, level.dat, r.0.0.mca, Foo.jar, keep.log
        assert_eq!(plan.files, 6);
        let expect_bytes = 18 + 10 + 5 + 1000 + 3 + "nested logs are plugin data".len() as u64;
        assert_eq!(plan.bytes, expect_bytes);

        let mut last = (0, 0);
        let copy = run(&plan, &cfg, &mut |d, t| last = (d, t)).unwrap();
        assert_eq!(last, (expect_bytes, expect_bytes));
        let d = &plan.dest;
        assert!(d.join("world/level.dat").is_file() && d.join("world/region/r.0.0.mca").is_file());
        assert!(d.join("plugins/Foo.jar").is_file() && d.join("plugins/Foo/logs/keep.log").is_file());
        assert!(d.join("world_nether").is_dir());
        for gone in [
            "logs",
            "crash-reports",
            "cache",
            ".mctui-old",
            ".mctui-before-restore",
            ".mctui-before-reset",
            "plugins/.mctui-old",
            "backups",
            "world/session.lock",
            "half.tar.gz.part",
            "srv-20260101-000000.snap.partial",
        ] {
            assert!(!d.join(gone).exists(), "{gone} скопирован");
        }
        assert!(!partial_path(d).exists());
        assert_eq!(copy.dir.as_deref(), Some(d.as_path()));
        // Копия независима: правка копии не меняет исходник.
        fs::write(d.join("world/level.dat"), "changed").unwrap();
        assert_eq!(fs::read_to_string(s.join("world/level.dat")).unwrap(), "level");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn backups_in_server_root_are_skipped_by_name() {
        let root = tmp("rootbk");
        let s = root.join("srv");
        write(&s.join("srv-20260101-000000.tar.gz"), "backup");
        write(&s.join("other.tar.gz"), "keep");
        let mut cfg = server("srv", &s);
        cfg.backup_dir = ".".into();
        let plan = plan_with(&cfg, &[], &all_free).unwrap();
        assert_eq!(plan.skipped, vec!["srv-20260101-000000.tar.gz"]);
        assert_eq!(plan.files, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ports_avoid_configured_and_busy_ones() {
        let root = tmp("ports");
        let a = root.join("a");
        write(
            &a.join("server.properties"),
            "server-port=25565\nenable-rcon=true\nrcon.port=25575\nenable-query=true\nquery.port=25565\n",
        );
        let b = root.join("b");
        write(&b.join("server.properties"), "server-port=25566\nrcon.port=25576\n");
        let proxy = root.join("proxy");
        write(&proxy.join(velocity::CONFIG), "bind = \"0.0.0.0:25567\"\n");
        let all = vec![server("A", &a), server("B", &b), server("P", &proxy)];
        let busy = |p: u16, _: bool| p != 25568;
        let plan = plan_with(&all[0], &all, &busy).unwrap();
        // 25566 — B, 25567 — прокси, 25568 — занят кем-то ещё.
        assert_eq!(plan.port, 25569);
        // 25576 — rcon.port у B.
        assert_eq!(plan.rcon_port, Some(25577));
        // От 25566: всё до 25569 занято или уже выбрано.
        assert_eq!(plan.query_port, Some(25570));

        // Прокси: порт bind + 1 (с пропуском занятых), RCON / query не трогаются.
        let pp = plan_with(&all[2], &all, &all_free).unwrap();
        assert_eq!(pp.port, 25568);
        assert_eq!((pp.rcon_port, pp.query_port), (None, None));

        // Без RCON / query — только server-port.
        let pb = plan_with(&all[1], &all, &all_free).unwrap();
        assert_eq!(pb.port, 25568);
        assert_eq!((pb.rcon_port, pb.query_port), (None, None));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn port_in_use_is_not_free() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(!port_free(port, false));
        assert_eq!(pick_port(u16::MAX, &HashSet::new(), &|_| true), None);
        assert_eq!(pick_port(100, &HashSet::from([101]), &|_| true), Some(102));
    }

    #[test]
    fn properties_rewrite_keeps_comments_and_order() {
        let root = tmp("props");
        let s = root.join("srv");
        let text = "#Minecraft server properties\n#Thu Nov 13 02:02:58 MSK 2026\nenable-query=false\n\
                    enable-rcon=true\nmotd=\\u00A7aМой \\u00A7lсервер\nquery.port=25565\nrcon.password=old\n\
                    rcon.port=25575\n# комментарий\nserver-port=25565\nview-distance=10\n";
        write(&s.join("server.properties"), text);
        let cfg = server("srv", &s);
        let plan = plan_with(&cfg, &[], &all_free).unwrap();
        run(&plan, &cfg, &mut no_progress()).unwrap();
        let out = fs::read_to_string(plan.dest.join("server.properties")).unwrap();
        let (a, b): (Vec<_>, Vec<_>) = (text.lines().collect(), out.lines().collect());
        assert_eq!(a.len(), b.len());
        let p = Properties::parse(&out);
        assert_eq!(p.get("server-port"), Some(plan.port.to_string().as_str()));
        assert_eq!(p.get("rcon.port"), Some(plan.rcon_port.unwrap().to_string().as_str()));
        assert_eq!(plan.query_port, None, "query выключен");
        assert_eq!(p.get("query.port"), Some("25565"));
        let pass = p.get("rcon.password").unwrap();
        assert!(pass != "old" && pass.len() == 32);
        assert_eq!(p.get("motd"), Some("\\u00A7aМой \\u00A7lсервер [test]"));
        // Остальные строки — байт в байт и на своих местах.
        for (x, y) in a.iter().zip(&b) {
            let key = x.split('=').next().unwrap();
            if !["server-port", "rcon.port", "rcon.password", "motd"].contains(&key) {
                assert_eq!(x, y);
            }
        }
        assert!(out.ends_with('\n'));
        // Исходник не тронут.
        assert_eq!(fs::read_to_string(s.join("server.properties")).unwrap(), text);

        // Копия копии: метка не дублируется.
        let cfg2 = server("srv-test", &plan.dest);
        let plan2 = plan_with(&cfg2, &[], &all_free).unwrap();
        run(&plan2, &cfg2, &mut no_progress()).unwrap();
        let p2 = Properties::load(&plan2.dest.join("server.properties")).unwrap();
        assert_eq!(p2.get("motd"), Some("\\u00A7aМой \\u00A7lсервер [test]"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_properties_get_a_port_and_velocity_gets_bind() {
        let root = tmp("noprops");
        let s = root.join("srv");
        write(&s.join("start.sh"), "");
        let cfg = server("srv", &s);
        let plan = plan_with(&cfg, &[], &all_free).unwrap();
        assert_eq!(plan.port, 25566);
        run(&plan, &cfg, &mut no_progress()).unwrap();
        assert_eq!(fs::read_to_string(plan.dest.join("server.properties")).unwrap(), "server-port=25566\n");

        let proxy = root.join("proxy");
        let vt = "# прокси\nbind = \"0.0.0.0:25577\"\n\n[servers]\nlobby = \"127.0.0.1:25565\"\ntry = [\"lobby\"]\n";
        write(&proxy.join(velocity::CONFIG), vt);
        let pc = server("proxy", &proxy);
        let pp = plan_with(&pc, &[], &all_free).unwrap();
        run(&pp, &pc, &mut no_progress()).unwrap();
        assert_eq!(velocity::proxy_port(&pp.dest), Some(25578));
        let out = fs::read_to_string(pp.dest.join(velocity::CONFIG)).unwrap();
        assert!(out.contains("lobby = \"127.0.0.1:25565\""));
        assert!(!pp.dest.join("server.properties").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_permissions_mtime_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("perm");
        let s = root.join("srv");
        write(&s.join("start.sh"), "#!/bin/sh\n");
        fs::set_permissions(s.join("start.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        write(&s.join("ro.txt"), "ro");
        fs::set_permissions(s.join("ro.txt"), fs::Permissions::from_mode(0o444)).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        fs::File::options().write(true).open(s.join("start.sh")).unwrap().set_modified(old).unwrap();
        std::os::unix::fs::symlink("start.sh", s.join("link.sh")).unwrap();
        let cfg = server("srv", &s);
        let plan = plan_with(&cfg, &[], &all_free).unwrap();
        run(&plan, &cfg, &mut no_progress()).unwrap();
        let d = &plan.dest;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d.join("start.sh")), 0o755);
        assert_eq!(mode(&d.join("ro.txt")), 0o444);
        assert_eq!(fs::metadata(d.join("start.sh")).unwrap().modified().unwrap(), old);
        assert!(fs::symlink_metadata(d.join("link.sh")).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(d.join("link.sh")).unwrap(), Path::new("start.sh"));
        // Настоящая копия, не жёсткая ссылка.
        assert_ne!(crate::backup::tests::ino(&s.join("start.sh")), crate::backup::tests::ino(&d.join("start.sh")));
        let _ = fs::set_permissions(d.join("ro.txt"), fs::Permissions::from_mode(0o644));
        let _ = fs::set_permissions(s.join("ro.txt"), fs::Permissions::from_mode(0o644));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn failed_copy_removes_partial() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("fail");
        let s = root.join("srv");
        write(&s.join("a/ok.txt"), "ok");
        fs::set_permissions(s.join("a"), fs::Permissions::from_mode(0o555)).unwrap();
        write(&s.join("z/secret.txt"), "no");
        fs::set_permissions(s.join("z/secret.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        let cfg = server("srv", &s);
        let plan = plan_with(&cfg, &[], &all_free).unwrap();
        if fs::read(s.join("z/secret.txt")).is_err() {
            // (под root файл читается — тогда проверять нечего)
            let err = run(&plan, &cfg, &mut no_progress()).unwrap_err();
            assert!(format!("{err:#}").contains("secret.txt"));
            assert!(!plan.dest.exists());
            assert!(!partial_path(&plan.dest).exists(), "недоделанная копия осталась");
        }
        let _ = fs::set_permissions(s.join("z/secret.txt"), fs::Permissions::from_mode(0o644));
        let _ = fs::set_permissions(s.join("a"), fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_existing_dest_and_picks_next_names() {
        let root = tmp("exists");
        let s = root.join("srv");
        write(&s.join("server.properties"), "server-port=25565\n");
        let cfg = server("srv", &s);
        let plan = plan_with(&cfg, std::slice::from_ref(&cfg), &all_free).unwrap();
        write(&plan.dest.join("mine.txt"), "user data");
        let err = run(&plan, &cfg, &mut no_progress()).unwrap_err();
        assert!(err.to_string().contains("srv-test"));
        assert_eq!(fs::read_to_string(plan.dest.join("mine.txt")).unwrap(), "user data");
        assert!(!partial_path(&plan.dest).exists());

        // Следующий план берёт -test-2 и другое имя, если первое уже занято.
        let taken = server(&plan.name, &root.join("elsewhere"));
        let plan2 = plan_with(&cfg, &[cfg.clone(), taken], &all_free).unwrap();
        assert_eq!(plan2.dest.file_name().unwrap(), "srv-test-2");
        assert_ne!(plan2.name, plan.name);
        assert!(plan2.name.starts_with("srv ("));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_config_is_safe_for_testing() {
        let root = tmp("cfg");
        let s = root.join("srv");
        write(&s.join("server.properties"), "server-port=25565\n");
        let mut cfg = server("srv", &s);
        let src_dir = canonical(&s);
        cfg.command = vec![src_dir.join("jdk/bin/java").display().to_string(), "-jar".into(), "paper.jar".into()];
        cfg.autostart = true;
        cfg.backup_copy_to = vec!["rclone:yadisk:mc".into()];
        cfg.backup_every_hours = 6;
        cfg.restart_times = vec!["05:00".into()];
        cfg.backup_keep = 20;
        cfg.auto_restart = false;
        cfg.backup_dir = "/Volumes/Big/mc-backups".into();
        let plan = plan_with(&cfg, &[], &all_free).unwrap();
        let c = copy_config(&cfg, &plan);
        assert_eq!(c.name, plan.name);
        assert_eq!(c.dir.as_deref(), Some(plan.dest.as_path()));
        assert_eq!(c.command[0], plan.dest.join("jdk/bin/java").display().to_string());
        assert_eq!(&c.command[1..], &["-jar", "paper.jar"]);
        assert!(!c.autostart && c.backup_copy_to.is_empty() && c.restart_times.is_empty());
        assert_eq!((c.backup_every_hours, c.backup_keep), (0, 3));
        assert!(!c.auto_restart, "остальное — как у исходника");
        assert_eq!(c.backup_dir, "backups");
        assert_eq!(c.backup_paths, cfg.backup_paths);

        cfg.backup_dir = src_dir.join("bk").display().to_string();
        cfg.command = vec!["./start.sh".into()];
        let c = copy_config(&cfg, &plan);
        assert_eq!(c.backup_dir, plan.dest.join("bk").display().to_string());
        assert_eq!(c.command, vec!["./start.sh"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn space_check() {
        assert!(check_space(None, u64::MAX).is_ok());
        assert!(check_space(Some(RESERVE + 10), 10).is_ok());
        assert!(check_space(Some(RESERVE + 9), 10).is_err());
    }
}
