//! Headless FH1 server: one map, up to 24 players. No world, no physics: each player already has the disc install.
//!
//! ```text
//! fh1-server [--config server.cfg] [--bind 0.0.0.0:7777] [--name ..] [--map colorado] [--max 24] [--password ..]
//!            [--motd ..] [--per-ip 2] [--registry-addr host:7700] [--public true|false]
//! fh1-server --registry [--bind 0.0.0.0:7700]          (the server list the in-game browser reads)
//! ```
//!
//! Config file: `key = value` lines, `#` comments. Keys: bind, name, map, max_players, password, motd, per_ip, ban
//! (an IP; repeatable), registry (host:port of the server list), public (true = heartbeat to the registry, so the
//! server shows in the browser). Command-line flags override the file. Console (stdin): `list`, `kick <id>`, `ban <id>`,
//! `quit`. Runs the same on Windows and Linux (std only).

use std::io::{BufRead, ErrorKind};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fh1_net::{decode, encode, Config, Handled, Heartbeat, Registry, Relay};

const USAGE: &str = "fh1-server [--config FILE] [--bind ADDR:PORT] [--name NAME] [--map MAP] [--max N] [--password PW] [--motd TEXT] [--per-ip N] [--registry-addr HOST:PORT] [--public true|false]\n       fh1-server --registry [--bind ADDR:PORT]";

struct Settings {
    bind: String,
    cfg: Config,
    /// host:port of the server list to heartbeat to.
    registry: Option<String>,
    public: bool,
    /// Run as the server list instead of a game server.
    registry_mode: bool,
}

fn main() {
    let set = settings();
    if set.registry_mode {
        return run_registry(&set.bind);
    }
    let bind = set.bind;
    let addr = fh1_net::resolve(&bind).unwrap_or_else(|e| die(&format!("{bind}: {e}")));
    let sock = UdpSocket::bind(addr).unwrap_or_else(|e| die(&format!("bind {addr}: {e} (port in use?)")));
    sock.set_read_timeout(Some(Duration::from_millis(100))).ok();
    let mut heartbeat = match (&set.registry, set.public) {
        (Some(r), true) => match fh1_net::resolve(r) {
            Ok(a) => {
                println!("listed on {r} ({a})");
                Some(Heartbeat::new(a))
            }
            Err(e) => {
                eprintln!("registry {r}: {e} (not listed; players can still connect directly)");
                None
            }
        },
        _ => None,
    };
    let mut relay = Relay::new(set.cfg);
    let c = relay.config();
    println!(
        "fh1-server {addr}  \"{}\"  map {}  {} players  {}  (protocol {})",
        c.name,
        c.map,
        c.max_players,
        if c.password.is_empty() { "open" } else { "password" },
        fh1_net::VERSION
    );
    let console = console();
    let mut buf = [0u8; 1500];
    let started = Instant::now();
    loop {
        let now = started.elapsed().as_millis() as u64;
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(hb) = heartbeat.as_mut().filter(|hb| hb.registry == from) {
                    if let Some(reply) = decode(&buf[..n]).and_then(|p| hb.handle(&p)) {
                        let _ = sock.send_to(&encode(&reply), from);
                    }
                    continue;
                }
                let h = relay.handle_datagram(from, &buf[..n], now);
                report(&h, Some(from));
                send_all(&sock, &h);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            // Windows: a client's port closed (ICMP unreachable) shows up here; nothing to do.
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
            Err(e) => eprintln!("recv: {e}"),
        }
        if let Some(hb) = heartbeat.as_mut() {
            if let Some(p) = hb.tick(now) {
                let _ = sock.send_to(&encode(&p), hb.registry);
            }
        }
        let h = relay.expire(now);
        if let Some((id, name)) = &h.left {
            println!("timeout #{id} {name}");
        }
        send_all(&sock, &h);
        while let Ok(line) = console.try_recv() {
            if command(&mut relay, &sock, &line) {
                return;
            }
        }
    }
}

fn report(h: &Handled, from: Option<SocketAddr>) {
    if let Some((id, name)) = &h.joined {
        println!("+ #{id} {name} ({})", from.map_or(String::new(), |a| a.to_string()));
    }
    if let Some((id, name)) = &h.left {
        println!("- #{id} {name}");
    }
}

fn send_all(sock: &UdpSocket, h: &Handled) {
    for (to, packet) in &h.packets {
        if let Err(e) = sock.send_to(&encode(packet), to) {
            if e.kind() != ErrorKind::WouldBlock {
                eprintln!("send {to}: {e}");
            }
        }
    }
}

/// Returns true to quit.
fn command(relay: &mut Relay, sock: &UdpSocket, line: &str) -> bool {
    let mut it = line.split_whitespace();
    match (it.next(), it.next().and_then(|v| v.parse::<u8>().ok())) {
        (Some("list"), _) => {
            let list = relay.list();
            println!("{} / {} players", list.len(), relay.config().max_players);
            for (id, name, addr) in list {
                println!("  #{id} {name} {addr}");
            }
        }
        (Some(cmd @ ("kick" | "ban")), Some(id)) => {
            let addr = relay.list().into_iter().find(|p| p.0 == id).map(|p| p.2);
            match relay.kick(id) {
                Some((name, h)) => {
                    send_all(sock, &h);
                    if cmd == "ban" {
                        if let Some(a) = addr {
                            relay.ban(a.ip());
                            println!("banned #{id} {name} ({}) until restart; add `ban = {}` to the config to keep it", a.ip(), a.ip());
                        }
                    } else {
                        println!("kicked #{id} {name}");
                    }
                }
                None => println!("no player #{id}"),
            }
        }
        (Some("quit" | "exit"), _) => return true,
        (Some(_), _) => println!("commands: list | kick <id> | ban <id> | quit"),
        (None, _) => {}
    }
    false
}

fn console() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("console".into()).spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// The server list: game servers heartbeat here, browsers ask for the list. No players, no console.
fn run_registry(bind: &str) {
    let addr = fh1_net::resolve(bind).unwrap_or_else(|e| die(&format!("{bind}: {e}")));
    let sock = UdpSocket::bind(addr).unwrap_or_else(|e| die(&format!("bind {addr}: {e} (port in use?)")));
    sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
    println!("fh1-server registry {addr}  (protocol {})", fh1_net::VERSION);
    let mut reg = Registry::new();
    let mut buf = [0u8; 1500];
    let started = Instant::now();
    loop {
        let now = started.elapsed().as_millis() as u64;
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(packet) = decode(&buf[..n]) {
                    let (replies, added) = reg.handle(from, packet, now);
                    if let Some(a) = added {
                        println!("+ server {a}");
                    }
                    for (to, p) in replies {
                        let _ = sock.send_to(&encode(&p), to);
                    }
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset) => {}
            Err(e) => eprintln!("recv: {e}"),
        }
        for a in reg.expire(now) {
            println!("- server {a} (no heartbeat)");
        }
    }
}

/// Settings from the config file and the command line (the command line wins).
fn settings() -> Settings {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let registry_mode = args.iter().any(|a| a == "--registry");
    let mut set = Settings {
        bind: if registry_mode { "0.0.0.0:7700".into() } else { "0.0.0.0:7777".into() },
        cfg: Config::default(),
        registry: None,
        public: true,
        registry_mode,
    };
    if let Some(i) = args.iter().position(|a| a == "--config") {
        let path = args.get(i + 1).unwrap_or_else(|| die("--config <file>"));
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { die(&format!("{path}:{}: expected key = value", n + 1)) };
            apply(&mut set, k.trim(), v.trim().trim_matches('"'), &format!("{path}:{}", n + 1));
        }
    }
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let key = match a.as_str() {
            "--config" => {
                it.next();
                continue;
            }
            "--registry" => continue,
            "--registry-addr" => "registry",
            "--public" => "public",
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--bind" => "bind",
            "--name" => "name",
            "--map" => "map",
            "--max" => "max_players",
            "--password" => "password",
            "--motd" => "motd",
            "--per-ip" => "per_ip",
            other => die(&format!("unknown argument {other}\n{USAGE}")),
        };
        let v = it.next().unwrap_or_else(|| die(&format!("{a} needs a value")));
        apply(&mut set, key, v, a);
    }
    set
}

fn apply(set: &mut Settings, key: &str, v: &str, at: &str) {
    let num = |v: &str| v.parse::<usize>().unwrap_or_else(|_| die(&format!("{at}: {key} must be a number")));
    let cfg = &mut set.cfg;
    match key {
        "bind" => set.bind = v.to_string(),
        "registry" => set.registry = Some(v.to_string()).filter(|s| !s.is_empty()),
        "public" => set.public = matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "on"),
        "name" => cfg.name = v.to_string(),
        "map" => cfg.map = v.to_string(),
        "max_players" => cfg.max_players = num(v),
        "password" => cfg.password = v.to_string(),
        "motd" => cfg.motd = v.to_string(),
        "per_ip" => cfg.per_ip = num(v),
        "ban" => cfg.banned.push(v.parse::<IpAddr>().unwrap_or_else(|_| die(&format!("{at}: ban needs an IP address")))),
        _ => die(&format!("{at}: unknown key {key}")),
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
