//! The in-game server browser's network side: asks the registry for the list (page by page), adds direct / favourite
//! addresses, and INFO-pings every server for its name, map, players and round trip. Non-blocking; poll once a frame.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::proto::{decode, encode, resolve, Packet, ServerInfo};

/// An unanswered INFO is sent again this often, a few times.
const RETRY: Duration = Duration::from_millis(700);
const TRIES: u8 = 3;

#[derive(Clone, Debug)]
pub struct BrowserRow {
    pub addr: SocketAddr,
    /// None until the server answers (or if it never does: `tries` used up = unreachable).
    pub info: Option<ServerInfo>,
    pub ping_ms: Option<u32>,
    /// Added by hand (direct connect / favourite), not from the registry.
    pub direct: bool,
    nonce: u32,
    sent: Option<Instant>,
    tries: u8,
}

impl BrowserRow {
    /// Asked [`TRIES`] times without an answer.
    pub fn unreachable(&self) -> bool {
        self.info.is_none() && self.tries >= TRIES && self.sent.is_some_and(|t| t.elapsed() >= RETRY)
    }
}

pub struct ServerBrowser {
    sock: UdpSocket,
    registry: Option<SocketAddr>,
    rows: Vec<BrowserRow>,
    next_nonce: u32,
    /// Registry pages asked for and when.
    list_sent: Option<(u16, Instant, u8)>,
    pub registry_error: Option<String>,
}

impl ServerBrowser {
    /// `registry` = "host:port" of the server list (None = direct addresses only).
    pub fn new(registry: Option<&str>) -> io::Result<Self> {
        let sock = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))?;
        sock.set_nonblocking(true)?;
        let (registry, registry_error) = match registry.map(resolve) {
            Some(Ok(a)) => (Some(a), None),
            Some(Err(e)) => (None, Some(e.to_string())),
            None => (None, None),
        };
        Ok(Self { sock, registry, rows: Vec::new(), next_nonce: 1, list_sent: None, registry_error })
    }

    pub fn rows(&self) -> &[BrowserRow] {
        &self.rows
    }

    /// Adds a direct address ("host:port") and pings it.
    pub fn add_direct(&mut self, text: &str) -> io::Result<SocketAddr> {
        let addr = resolve(text)?;
        self.add(addr, true);
        Ok(addr)
    }

    fn add(&mut self, addr: SocketAddr, direct: bool) {
        if let Some(r) = self.rows.iter_mut().find(|r| r.addr == addr) {
            r.direct |= direct;
            return;
        }
        self.rows.push(BrowserRow { addr, info: None, ping_ms: None, direct, nonce: 0, sent: None, tries: 0 });
    }

    /// Asks the registry again and re-pings every row.
    pub fn refresh(&mut self) {
        if let Some(reg) = self.registry {
            let _ = self.sock.send_to(&encode(&Packet::ListReq { page: 0 }), reg);
            self.list_sent = Some((0, Instant::now(), 1));
        }
        for r in &mut self.rows {
            r.sent = None;
            r.tries = 0;
        }
    }

    /// Call every frame: reads answers, asks for the next registry page, (re)sends INFO requests.
    pub fn poll(&mut self) {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            match decode(&buf[..n]) {
                Some(Packet::List { page, pages, servers }) if Some(from) == self.registry => {
                    for mut a in servers {
                        // A game server on the registry's own box heartbeats from 127.0.0.1: it's at the registry's IP.
                        if a.ip().is_loopback() || a.ip().is_unspecified() {
                            a.set_ip(from.ip());
                        }
                        self.add(a, false);
                    }
                    if page + 1 < pages {
                        let _ = self.sock.send_to(&encode(&Packet::ListReq { page: page + 1 }), from);
                        self.list_sent = Some((page + 1, Instant::now(), 1));
                    } else {
                        self.list_sent = None;
                    }
                }
                Some(Packet::Info { nonce, info }) => {
                    if let Some(r) = self.rows.iter_mut().find(|r| r.addr == from && r.nonce == nonce) {
                        r.ping_ms = r.sent.map(|t| t.elapsed().as_millis() as u32);
                        r.info = Some(info);
                    }
                }
                _ => {}
            }
        }
        // A lost registry page: ask again a couple of times.
        if let (Some(reg), Some((page, at, tries))) = (self.registry, self.list_sent) {
            if at.elapsed() >= RETRY {
                if tries < TRIES {
                    let _ = self.sock.send_to(&encode(&Packet::ListReq { page }), reg);
                    self.list_sent = Some((page, Instant::now(), tries + 1));
                } else {
                    self.list_sent = None;
                    self.registry_error.get_or_insert_with(|| "server list didn't answer".into());
                }
            }
        }
        for r in &mut self.rows {
            let due = r.sent.is_none() || (r.info.is_none() && r.tries < TRIES && r.sent.is_some_and(|t| t.elapsed() >= RETRY));
            if due {
                r.nonce = self.next_nonce;
                self.next_nonce = self.next_nonce.wrapping_add(1).max(1);
                let _ = self.sock.send_to(&encode(&Packet::InfoReq { nonce: r.nonce }), r.addr);
                r.sent = Some(Instant::now());
                r.tries += 1;
            }
        }
    }
}
