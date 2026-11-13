//! «Как друзьям зайти на сервер»: адреса этой машины в локальной сети, внешний IP, проверка,
//! открыт ли порт из интернета, и подсказки по настройке.
//!
//! Только чтение и советы: роутер, UPnP и брандмауэр не трогаются.
//! Сеть — через системный `curl` (как в `update.rs`), с жёсткими таймаутами.

use crate::tr;
use anyhow::{Context, Result, bail};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::process::{Command, Stdio};

/// Сервисы определения внешнего IP (plain text), по очереди.
const PUBLIC_IP_URLS: &[&str] = &["https://api.ipify.org", "https://ifconfig.co/ip"];
/// Проверка порта: `GET /port/<n>` → `{"ip": "...", "port": n, "reachable": bool}`.
const PORT_CHECK_URL: &str = "https://ifconfig.co/port/";

fn curl() -> Command {
    let mut c = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    // -4: друзьям нужен IPv4; без --retry, чтобы не висеть.
    c.args(["-4", "-fsS", "--connect-timeout", "5", "--max-time", "15"]);
    c.arg("-A").arg(format!("mctui/{}", crate::update::VERSION));
    c.stdin(Stdio::null());
    c
}

fn curl_get(url: &str, accept_json: bool) -> Result<String> {
    let mut c = curl();
    if accept_json {
        c.args(["-H", "Accept: application/json"]);
    }
    let out = c.arg(url).output().context(tr!("не удалось запустить curl", "failed to run curl"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(tr!(format!("{url} недоступен ({e})"), format!("{url} is unreachable ({e})")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── Адреса в локальной сети ─────────────────────────────────────────────────────────────────

/// 100.64.0.0/10 — CGNAT провайдера (и Tailscale).
pub fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xC0) == 64
}

/// Адрес из обычной домашней/офисной сети (10/8, 172.16/12, 192.168/16).
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(_) => false,
    }
}

/// Виртуальные интерфейсы (Docker, VPN, VM, мосты), адреса которых друзьям не нужны.
fn is_virtual_iface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "lo",
        "docker",
        "br-",
        "veth",
        "virbr",
        "vmnet",
        "vboxnet",
        "tun",
        "tap",
        "utun",
        "wg",
        "tailscale",
        "zt",
        "bridge",
        "awdl",
        "llw",
        "anpi",
        "gif",
        "stf",
        "ipsec",
        "ppp",
        "cni",
        "flannel",
        "kube",
        "lxc",
        "lxd",
    ];
    const WORDS: &[&str] = &[
        "vethernet",
        "virtualbox",
        "vmware",
        "hyper-v",
        "loopback",
        "wsl",
        "tailscale",
        "zerotier",
        "wireguard",
        "vpn",
    ];
    PREFIXES.iter().any(|p| n.starts_with(p)) || WORDS.iter().any(|w| n.contains(w))
}

/// Разбор `ifconfig` (macOS/BSD, старый Linux `inet addr:`) и `ip -o -4 addr` (Linux):
/// пары (интерфейс, IPv4).
pub fn parse_unix_addrs(text: &str) -> Vec<(String, Ipv4Addr)> {
    let mut out = vec![];
    let mut iface = String::new();
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.is_empty() {
            continue;
        }
        // `ip -o addr`: "2: eth0    inet 192.168.1.10/24 brd ... scope global eth0"
        let ip_o = toks.len() > 2 && toks[0].ends_with(':') && toks[0][..toks[0].len() - 1].parse::<u32>().is_ok();
        if ip_o {
            iface = toks[1].trim_end_matches(':').split('@').next().unwrap_or("").to_string();
        } else if !line.starts_with(char::is_whitespace) {
            // ifconfig: "en0: flags=..." или Linux "eth0      Link encap:..."
            iface = toks[0].trim_end_matches(':').to_string();
        }
        let Some(pos) = toks.iter().position(|t| *t == "inet") else {
            continue;
        };
        let Some(addr) = toks.get(pos + 1) else {
            continue;
        };
        let addr = addr.trim_start_matches("addr:");
        let addr = addr.split('/').next().unwrap_or(addr);
        if let Ok(ip) = addr.parse::<Ipv4Addr>() {
            out.push((iface.clone(), ip));
        }
    }
    out
}

/// Разбор `ipconfig` (Windows, любой язык): строки с «IPv4» внутри блока адаптера.
pub fn parse_ipconfig(text: &str) -> Vec<(String, Ipv4Addr)> {
    let mut out = vec![];
    let mut adapter = String::new();
    for line in text.lines() {
        let t = line.trim_end();
        if t.is_empty() {
            continue;
        }
        if !t.starts_with(char::is_whitespace) {
            adapter = t.trim_end_matches(':').trim().to_string();
            continue;
        }
        if !t.contains("IPv4") {
            continue;
        }
        let Some((_, val)) = t.rsplit_once(" : ").or_else(|| t.rsplit_once(": ")) else {
            continue;
        };
        // "192.168.1.5(Preferred)" / "192.168.1.5(Основной)"
        let val: String = val.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if let Ok(ip) = val.parse::<Ipv4Addr>() {
            out.push((adapter.clone(), ip));
        }
    }
    out
}

fn run_text(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Адрес, через который идёт трафик в интернет (по таблице маршрутов). Пакеты не отправляются:
/// `connect` у UDP только выбирает маршрут.
pub fn primary_address() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// Отбор и порядок: только адреса локальной сети на реальных интерфейсах, основной — первым.
pub fn select_lan(pairs: &[(String, Ipv4Addr)], primary: Option<IpAddr>) -> Vec<IpAddr> {
    let mut v: Vec<IpAddr> = vec![];
    if let Some(p) = primary.filter(|p| is_lan(*p)) {
        v.push(p);
    }
    let rank = |ip: &Ipv4Addr| match ip.octets() {
        [192, 168, ..] => 0,
        [10, ..] => 1,
        _ => 2,
    };
    let mut rest: Vec<Ipv4Addr> =
        pairs.iter().filter(|(name, ip)| !is_virtual_iface(name) && ip.is_private()).map(|(_, ip)| *ip).collect();
    rest.sort_by_key(rank);
    for ip in rest {
        if !v.contains(&IpAddr::V4(ip)) {
            v.push(IpAddr::V4(ip));
        }
    }
    v
}

/// IPv4-адреса этой машины в локальной сети (без loopback, Docker, VPN), основной — первым.
pub fn lan_addresses() -> Vec<IpAddr> {
    let pairs = if cfg!(windows) {
        run_text("ipconfig", &[]).map(|t| parse_ipconfig(&t)).unwrap_or_default()
    } else {
        run_text("ip", &["-o", "-4", "addr", "show"])
            .or_else(|| run_text("ifconfig", &[]))
            .or_else(|| run_text("/sbin/ifconfig", &[]))
            .map(|t| parse_unix_addrs(&t))
            .unwrap_or_default()
    };
    select_lan(&pairs, primary_address())
}

// ── Интернет ────────────────────────────────────────────────────────────────────────────────

/// Внешний IPv4 (как его видит интернет). Ошибка — нет интернета или сервисы недоступны.
pub fn public_ip() -> Result<IpAddr> {
    let mut last = None;
    for url in PUBLIC_IP_URLS {
        match curl_get(url, false) {
            Ok(body) => match body.trim().parse::<IpAddr>() {
                Ok(ip) => return Ok(ip),
                Err(_) => {
                    last = Some(anyhow::anyhow!(tr!(
                        format!("{url}: непонятный ответ"),
                        format!("{url}: unexpected response")
                    )))
                }
            },
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no public IP service")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortCheck {
    /// Внешний IP, который проверялся.
    pub ip: IpAddr,
    pub port: u16,
    /// Удалось ли подключиться к `ip:port` из интернета.
    pub reachable: bool,
}

pub fn parse_port_check(body: &str) -> Result<PortCheck> {
    let v: serde_json::Value = serde_json::from_str(body.trim())
        .with_context(|| tr!("сервис проверки порта ответил не JSON", "the port check service did not return JSON"))?;
    let ip = v.get("ip").and_then(|x| x.as_str()).and_then(|s| s.parse().ok());
    let port = v.get("port").and_then(|x| x.as_u64()).and_then(|p| u16::try_from(p).ok());
    let reachable = v.get("reachable").and_then(|x| x.as_bool());
    match (ip, port, reachable) {
        (Some(ip), Some(port), Some(reachable)) => Ok(PortCheck { ip, port, reachable }),
        _ => bail!(tr!("неожиданный ответ сервиса проверки порта", "unexpected port check response")),
    }
}

/// Открыт ли TCP-порт на внешнем IP из интернета (через ifconfig.co). Имеет смысл, только пока
/// сервер запущен. Не дольше ~15 секунд.
pub fn check_port(port: u16) -> Result<PortCheck> {
    let body = curl_get(&format!("{PORT_CHECK_URL}{port}"), true)?;
    parse_port_check(&body)
}

// ── Подсказки ───────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Windows,
    Linux,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

/// Всё, что известно для подсказок. Сетевые поля заполняет [`Facts::probe`] (блокирующе —
/// вызывать в `spawn_blocking`), серверные — [`Facts::apply_properties`].
#[derive(Clone, Debug)]
pub struct Facts {
    pub port: u16,
    pub lan: Vec<IpAddr>,
    pub primary: Option<IpAddr>,
    /// None — ещё не проверяли.
    pub public: Option<Result<IpAddr, String>>,
    pub check: Option<Result<PortCheck, String>>,
    pub running: bool,
    /// Значение `server-ip`.
    pub server_ip: String,
    pub online_mode: bool,
    pub white_list: bool,
    pub os: Os,
}

impl Default for Facts {
    fn default() -> Self {
        Self {
            port: 25565,
            lan: vec![],
            primary: None,
            public: None,
            check: None,
            running: false,
            server_ip: String::new(),
            online_mode: true,
            white_list: false,
            os: Os::current(),
        }
    }
}

impl Facts {
    pub fn apply_properties(&mut self, p: &crate::properties::Properties) {
        let b = |k: &str, def: bool| p.get(k).map(|v| v.trim().eq_ignore_ascii_case("true")).unwrap_or(def);
        self.port = p.get("server-port").and_then(|v| v.trim().parse().ok()).unwrap_or(25565);
        self.server_ip = p.get("server-ip").unwrap_or("").trim().to_string();
        self.online_mode = b("online-mode", true);
        self.white_list = b("white-list", false);
    }

    /// Сетевые проверки. `with_port_check` — только если сервер запущен (иначе порт закрыт всегда).
    pub fn probe(&mut self, with_port_check: bool) {
        self.primary = primary_address();
        self.lan = lan_addresses();
        self.public = Some(public_ip().map_err(|e| e.to_string()));
        if with_port_check {
            self.check = Some(check_port(self.port).map_err(|e| e.to_string()));
        }
    }

    /// Трафик в интернет идёт не через локальную сеть (VPN/прокси-туннель): внешний IP и проверка
    /// порта тогда относятся к VPN, а не к домашнему подключению.
    pub fn vpn_suspected(&self) -> bool {
        matches!(self.primary, Some(p) if !is_lan(p))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintLevel {
    /// Заголовок раздела.
    Header,
    Ok,
    Info,
    Warn,
    Bad,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    pub level: HintLevel,
    pub text: String,
}

fn h(level: HintLevel, text: String) -> Hint {
    Hint { level, text }
}

fn router_guess(lan: Option<&IpAddr>) -> Option<String> {
    match lan {
        Some(IpAddr::V4(v4)) => {
            let o = v4.octets();
            Some(format!("http://{}.{}.{}.1", o[0], o[1], o[2]))
        }
        _ => None,
    }
}

/// Подсказки «как зайти друзьям» по собранным фактам.
pub fn hints(f: &Facts) -> Vec<Hint> {
    use HintLevel::*;
    let mut v = vec![];
    let port = f.port;
    let lan = f.lan.first();
    let lan_txt = lan.map(|ip| ip.to_string()).unwrap_or_else(|| tr!("<IP этого компьютера>", "<this PC's IP>").into());

    // server-ip
    if !f.server_ip.is_empty() {
        let bound_elsewhere = f.server_ip.parse::<IpAddr>().map(|ip| !f.lan.contains(&ip)).unwrap_or(true);
        let lvl = if bound_elsewhere { Bad } else { Warn };
        v.push(h(
            lvl,
            tr!(
                format!("server-ip={} — сервер слушает только этот адрес. Оставьте server-ip пустым.", f.server_ip),
                format!("server-ip={} — the server listens on this address only. Leave server-ip empty.", f.server_ip)
            ),
        ));
    }

    // Та же сеть
    v.push(h(Header, tr!("В той же сети (Wi-Fi / роутер)", "On the same network (Wi-Fi / router)").into()));
    if f.lan.is_empty() {
        v.push(h(
            Warn,
            tr!(
                "Не найден адрес в локальной сети — компьютер не подключён к роутеру?",
                "No local network address found — is this computer connected to a router?"
            )
            .into(),
        ));
    } else {
        v.push(h(
            Ok,
            tr!(
                format!("Сетевая игра → Добавить сервер → {lan_txt}:{port}"),
                format!("Multiplayer → Add Server → {lan_txt}:{port}")
            ),
        ));
        if f.lan.len() > 1 {
            let others: Vec<String> = f.lan[1..].iter().map(|ip| format!("{ip}:{port}")).collect();
            v.push(h(
                Info,
                tr!(
                    format!("Другие адреса этой машины: {}", others.join(", ")),
                    format!("Other addresses of this machine: {}", others.join(", "))
                ),
            ));
        }
        v.push(h(Info, tr!("На этом же компьютере: localhost", "On this computer: localhost").into()));
    }
    v.push(h(Info, firewall_hint(f.os, port)));

    // Интернет
    v.push(h(Header, tr!("Через интернет", "Over the internet").into()));
    if f.vpn_suspected() {
        v.push(h(
            Warn,
            tr!(
                "Включён VPN/прокси: внешний IP и проверка порта относятся к VPN. Для проверки выключите его.",
                "A VPN/proxy is active: the public IP and port check refer to the VPN. Turn it off to check."
            )
            .into(),
        ));
    }
    let public = match &f.public {
        Some(Result::Ok(ip)) => Some(*ip),
        Some(Err(e)) => {
            v.push(h(
                Warn,
                tr!(
                    format!("Не удалось узнать внешний IP (нет интернета или сервис недоступен): {e}"),
                    format!("Could not find the public IP (no internet or service unavailable): {e}")
                ),
            ));
            None
        }
        None => None,
    };
    let mut need_tunnel = matches!(f.public, Some(Err(_)));
    match (&f.check, public) {
        (Some(Result::Ok(c)), _) if c.reachable => {
            v.push(h(
                Ok,
                tr!(
                    format!("Порт {port} открыт из интернета. Друзьям: {}:{port}", c.ip),
                    format!("Port {port} is reachable from the internet. Friends use {}:{port}", c.ip)
                ),
            ));
        }
        (Some(Result::Ok(c)), _) => {
            if f.running {
                v.push(h(
                    Bad,
                    tr!(
                        format!("Порт {port} на {} закрыт: из интернета к серверу не подключиться.", c.ip),
                        format!("Port {port} on {} is closed: the server cannot be reached from the internet.", c.ip)
                    ),
                ));
                need_tunnel = true;
            } else {
                v.push(h(
                    Info,
                    tr!(
                        "Сервер остановлен — проверка порта имеет смысл только при запущенном сервере.",
                        "The server is stopped — the port check only works while it is running."
                    )
                    .into(),
                ));
            }
            forwarding_hints(&mut v, port, lan, &lan_txt, Some(c.ip));
        }
        (Some(Err(e)), _) => {
            v.push(h(Warn, tr!(format!("Проверка порта не удалась: {e}"), format!("Port check failed: {e}"))));
            forwarding_hints(&mut v, port, lan, &lan_txt, public);
        }
        (None, _) => {
            if let Some(ip) = public {
                v.push(h(
                    Info,
                    tr!(
                        format!("Внешний адрес: {ip}:{port} — заработает после проброса порта."),
                        format!("Public address: {ip}:{port} — works once the port is forwarded.")
                    ),
                ));
            }
            forwarding_hints(&mut v, port, lan, &lan_txt, public);
        }
    }
    if let Some(IpAddr::V4(p)) = public
        && (p.is_private() || is_cgnat(p))
    {
        need_tunnel = true;
    }

    // Туннель
    v.push(h(Header, tr!("Если порт открыть нельзя", "If the port cannot be opened").into()));
    v.push(h(
        if need_tunnel { Warn } else { Info },
        tr!(
            "Если WAN-IP в роутере не совпадает с внешним или начинается на 100.64–100.127 — это CGNAT, проброс не поможет.",
            "If the router's WAN IP differs from the public IP or is in 100.64–100.127, it is CGNAT: forwarding won't help."
        )
        .into(),
    ));
    v.push(h(
        if need_tunnel { Warn } else { Info },
        tr!(
            format!(
                "Бесплатный туннель playit.gg: установите агент, создайте туннель Minecraft Java на 127.0.0.1:{port} и дайте друзьям его адрес."
            ),
            format!(
                "Free tunnel playit.gg: install the agent, add a Minecraft Java tunnel to 127.0.0.1:{port}, share the address it gives."
            )
        ),
    ));

    // Безопасность
    if !f.online_mode {
        v.push(h(Header, tr!("Безопасность", "Security").into()));
        v.push(h(
            Bad,
            tr!(
                "online-mode=false: войти может кто угодно под любым ником, в том числе под вашим (оператора).",
                "online-mode=false: anyone can join under any name, including yours (an operator's)."
            )
            .into(),
        ));
        v.push(h(
            Warn,
            if f.white_list {
                tr!(
                    "Белый список включён, но без проверки лицензии ник можно подделать — не открывайте сервер всем.",
                    "The whitelist is on, but without account checks names can be faked — don't share the address widely."
                )
            } else {
                tr!(
                    "Включите белый список (white-list=true) или online-mode=true, прежде чем открывать порт.",
                    "Turn on the whitelist (white-list=true) or online-mode=true before opening the port."
                )
            }
            .into(),
        ));
    }
    v
}

fn forwarding_hints(v: &mut Vec<Hint>, port: u16, lan: Option<&IpAddr>, lan_txt: &str, public: Option<IpAddr>) {
    let router = router_guess(lan).map(|r| tr!(format!(" (часто {r})"), format!(" (often {r})"))).unwrap_or_default();
    v.push(h(
        HintLevel::Info,
        tr!(
            format!("Проброс порта в роутере{router}: TCP {port} → {lan_txt}:{port}."),
            format!("Port forwarding on the router{router}: TCP {port} → {lan_txt}:{port}.")
        ),
    ));
    if let Some(ip) = public {
        v.push(h(
            HintLevel::Info,
            tr!(format!("После этого друзьям: {ip}:{port}"), format!("Then friends connect to {ip}:{port}")),
        ));
    }
}

fn firewall_hint(os: Os, port: u16) -> String {
    match os {
        Os::Mac => tr!(
            "Брандмауэр macOS: Настройки → Сеть → Брандмауэр → Параметры — разрешите входящие для java.",
            "macOS firewall: System Settings → Network → Firewall → Options — allow incoming for java."
        )
        .into(),
        Os::Windows => tr!(
            "Брандмауэр Windows: разрешите java (OpenJDK Platform binary) в «Разрешение приложений» Защитника.",
            "Windows: allow java (OpenJDK Platform binary) in Defender Firewall → Allow an app through firewall."
        )
        .into(),
        Os::Linux => tr!(
            format!("Если включён брандмауэр: например, sudo ufw allow {port}/tcp"),
            format!("If a firewall is enabled: e.g. sudo ufw allow {port}/tcp")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_IFCONFIG: &str = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384
\tinet 127.0.0.1 netmask 0xff000000
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether aa:bb:cc:dd:ee:ff
\tinet6 fe80::1%en0 prefixlen 64 secured scopeid 0xb
\tinet 192.168.3.3 netmask 0xffffff00 broadcast 192.168.3.255
bridge100: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tinet 192.168.64.1 netmask 0xffffff00 broadcast 192.168.64.255
utun244: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 8500
\tinet 198.18.0.2 --> 198.18.0.1 netmask 0xffff0000
utun5: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280
\tinet 100.101.102.103 --> 100.101.102.103 netmask 0xffffffff
";

    const LINUX_IP_O: &str = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: enp3s0    inet 10.0.0.15/24 brd 10.0.0.255 scope global dynamic noprefixroute enp3s0\\       valid_lft 85000sec
3: wlan0    inet 192.168.1.42/24 brd 192.168.1.255 scope global dynamic wlan0\\       valid_lft 3600sec
4: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\\       valid_lft forever
5: tailscale0    inet 100.64.1.2/32 scope global tailscale0\\       valid_lft forever
";

    const LINUX_OLD_IFCONFIG: &str = "eth0      Link encap:Ethernet  HWaddr 00:11:22:33:44:55
          inet addr:192.168.0.20  Bcast:192.168.0.255  Mask:255.255.255.0
lo        Link encap:Local Loopback
          inet addr:127.0.0.1  Mask:255.0.0.0
";

    const WIN_IPCONFIG_EN: &str = "
Windows IP Configuration


Ethernet adapter vEthernet (WSL):

   Connection-specific DNS Suffix  . :
   Link-local IPv6 Address . . . . . : fe80::1%20
   IPv4 Address. . . . . . . . . . . : 172.20.112.1
   Subnet Mask . . . . . . . . . . . : 255.255.240.0

Wireless LAN adapter Wi-Fi:

   Connection-specific DNS Suffix  . : home
   IPv4 Address. . . . . . . . . . . : 192.168.1.77(Preferred)
   Subnet Mask . . . . . . . . . . . : 255.255.255.0
   Default Gateway . . . . . . . . . : 192.168.1.1
";

    const WIN_IPCONFIG_RU: &str = "
Настройка протокола IP для Windows

Адаптер Ethernet Ethernet:

   DNS-суффикс подключения . . . . . :
   IPv4-адрес. . . . . . . . . . . . : 10.1.2.3
   Маска подсети . . . . . . . . . . : 255.255.255.0
";

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_macos_ifconfig() {
        let p = parse_unix_addrs(MAC_IFCONFIG);
        assert!(p.contains(&("en0".into(), "192.168.3.3".parse().unwrap())));
        assert!(p.contains(&("utun244".into(), "198.18.0.2".parse().unwrap())));
        assert_eq!(p.len(), 5);
        // VPN-туннели, мост и loopback отброшены; основной адрес — VPN (не LAN), поэтому не первым.
        assert_eq!(select_lan(&p, Some(ip("198.18.0.2"))), vec![ip("192.168.3.3")]);
    }

    #[test]
    fn parses_linux_ip_and_old_ifconfig() {
        let p = parse_unix_addrs(LINUX_IP_O);
        assert_eq!(p.len(), 5);
        assert_eq!(p[1], ("enp3s0".into(), "10.0.0.15".parse().unwrap()));
        // 192.168 раньше 10.x; docker и tailscale отброшены; основной (10.0.0.15) — первым.
        assert_eq!(select_lan(&p, None), vec![ip("192.168.1.42"), ip("10.0.0.15")]);
        assert_eq!(select_lan(&p, Some(ip("10.0.0.15"))), vec![ip("10.0.0.15"), ip("192.168.1.42")]);

        let p = parse_unix_addrs(LINUX_OLD_IFCONFIG);
        assert_eq!(select_lan(&p, None), vec![ip("192.168.0.20")]);
    }

    #[test]
    fn parses_ipconfig_any_language() {
        let p = parse_ipconfig(WIN_IPCONFIG_EN);
        assert_eq!(p.len(), 2);
        assert_eq!(p[1], ("Wireless LAN adapter Wi-Fi".into(), "192.168.1.77".parse().unwrap()));
        assert_eq!(select_lan(&p, None), vec![ip("192.168.1.77")]); // WSL отброшен
        let p = parse_ipconfig(WIN_IPCONFIG_RU);
        assert_eq!(select_lan(&p, None), vec![ip("10.1.2.3")]);
    }

    #[test]
    fn ranges() {
        assert!(is_cgnat("100.64.0.1".parse().unwrap()));
        assert!(is_cgnat("100.127.255.255".parse().unwrap()));
        assert!(!is_cgnat("100.128.0.1".parse().unwrap()));
        assert!(is_lan(ip("172.16.5.5")));
        assert!(!is_lan(ip("198.18.0.2")));
    }

    #[test]
    fn port_check_json() {
        let c = parse_port_check("{\n  \"ip\": \"89.1.2.3\",\n  \"port\": 25565,\n  \"reachable\": false\n}").unwrap();
        assert_eq!(c, PortCheck { ip: ip("89.1.2.3"), port: 25565, reachable: false });
        assert!(parse_port_check("<html>rate limited</html>").is_err());
        assert!(parse_port_check("{\"ip\":\"1.2.3.4\"}").is_err());
    }

    fn has(v: &[Hint], level: HintLevel, needle: &str) -> bool {
        v.iter().any(|x| x.level == level && x.text.contains(needle))
    }

    #[test]
    fn hints_selection() {
        // Порт открыт: зелёная строка с внешним адресом, туннель — не настойчиво.
        let mut f = Facts {
            port: 25570,
            lan: vec![ip("192.168.1.5"), ip("10.0.0.2")],
            primary: Some(ip("192.168.1.5")),
            public: Some(Result::Ok(ip("89.1.2.3"))),
            check: Some(Result::Ok(PortCheck { ip: ip("89.1.2.3"), port: 25570, reachable: true })),
            running: true,
            os: Os::Linux,
            ..Default::default()
        };
        let v = hints(&f);
        assert!(has(&v, HintLevel::Ok, "192.168.1.5:25570"));
        assert!(has(&v, HintLevel::Info, "10.0.0.2:25570"));
        assert!(has(&v, HintLevel::Ok, "89.1.2.3:25570"));
        assert!(has(&v, HintLevel::Info, "ufw allow 25570/tcp"));
        assert!(has(&v, HintLevel::Info, "playit.gg"));
        assert!(!v.iter().any(|x| x.level == HintLevel::Bad));

        // Порт закрыт при запущенном сервере: проброс с адресом роутера и настойчивый туннель.
        f.check = Some(Result::Ok(PortCheck { ip: ip("89.1.2.3"), port: 25570, reachable: false }));
        let v = hints(&f);
        assert!(has(&v, HintLevel::Bad, "89.1.2.3"));
        assert!(has(&v, HintLevel::Info, "http://192.168.1.1"));
        assert!(has(&v, HintLevel::Info, "TCP 25570 → 192.168.1.5:25570"));
        assert!(has(&v, HintLevel::Warn, "playit.gg"));

        // Сервер остановлен: «закрыт» не считается ошибкой.
        f.running = false;
        let v = hints(&f);
        assert!(!v.iter().any(|x| x.level == HintLevel::Bad));

        // Внешний IP не определился, VPN, server-ip, offline-режим.
        f.public = Some(Err("timeout".into()));
        f.check = None;
        f.primary = Some(ip("198.18.0.2"));
        f.server_ip = "192.168.9.9".into();
        f.online_mode = false;
        f.os = Os::Mac;
        let v = hints(&f);
        assert!(has(&v, HintLevel::Warn, "timeout"));
        assert!(has(&v, HintLevel::Warn, "VPN"));
        assert!(has(&v, HintLevel::Bad, "server-ip=192.168.9.9"));
        assert!(has(&v, HintLevel::Bad, "online-mode=false"));
        assert!(has(&v, HintLevel::Warn, "white-list=true"));
        assert!(has(&v, HintLevel::Warn, "playit.gg"));
        assert!(v.iter().any(|x| x.text.contains("java")));

        // Нет сети совсем.
        let v = hints(&Facts { lan: vec![], ..Default::default() });
        assert!(v.iter().any(|x| x.level == HintLevel::Warn));
    }

    #[test]
    fn facts_from_properties() {
        let p =
            crate::properties::Properties::parse("server-port=25599\nserver-ip=\nonline-mode=false\nwhite-list=true\n");
        let mut f = Facts::default();
        f.apply_properties(&p);
        assert_eq!((f.port, f.server_ip.as_str(), f.online_mode, f.white_list), (25599, "", false, true));
    }

    #[test]
    fn lan_addresses_do_not_panic() {
        // Живой вызов без сети: только локальные команды.
        let v = lan_addresses();
        assert!(v.iter().all(|ip| is_lan(*ip)));
    }

    #[test]
    #[ignore = "network: cargo test -- --ignored live_public_ip_and_port"]
    fn live_public_ip_and_port() {
        let ip = public_ip().unwrap();
        println!("public ip: {ip}, primary: {:?}, lan: {:?}", primary_address(), lan_addresses());
        let c = check_port(25565).unwrap();
        println!("port check: {c:?}");
        assert_eq!(c.port, 25565);
    }
}
