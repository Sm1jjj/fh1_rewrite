//! Non-blocking UDP client. The game polls it once a frame and ticks it with its car's motion and identity.
//!
//! Join: HELLO -> CHALLENGE -> HELLO(cookie, password proof) -> WELCOME (token). Then STATE at 20 Hz, PLAYER when the
//! car / paint changes and every few seconds, PING every second. Nothing heard for [`LOST_AFTER`] = [`Event::Lost`], and
//! the client starts joining again by itself.

use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::proto::{self, decode, encode, password_proof, Packet, PlayerInfo, RejectReason, ServerInfo, Snapshot};

const HELLO_EVERY: Duration = Duration::from_secs(1);
const STATE_EVERY: Duration = Duration::from_millis(50);
const PING_EVERY: Duration = Duration::from_secs(1);
const PLAYER_EVERY: Duration = Duration::from_secs(3);
/// A full / per-IP-capped server is asked again this often.
const RETRY_FULL: Duration = Duration::from_secs(5);
/// Nothing from the server for this long = connection lost.
pub const LOST_AFTER: Duration = Duration::from_secs(6);

#[derive(Debug)]
pub struct Client {
    sock: UdpSocket,
    server: SocketAddr,
    name: String,
    password: String,
    map: String,
    started: Instant,
    id: Option<u8>,
    token: u64,
    cookie: u64,
    seq: u32,
    last_hello: Option<Instant>,
    last_state: Option<Instant>,
    last_ping: Option<Instant>,
    last_player: Option<(Instant, PlayerInfo)>,
    last_heard: Option<Instant>,
    /// Joining paused until then (full server), or for good (wrong version / map / password / banned).
    paused_until: Option<Instant>,
    stopped: bool,
    rtt_ms: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Welcome { id: u8, max_players: u8, server_name: String, motd: String, map: String },
    Full,
    /// The server refused the join; the client stops trying (until [`Client::retry`]) except for `TooMany`.
    Rejected { reason: RejectReason, server_version: u8, map: String },
    State(Snapshot),
    Player(PlayerInfo),
    Leave { id: u8 },
    /// Nothing heard for [`LOST_AFTER`]; the client is joining again.
    Lost,
}

impl Client {
    /// `map` = the map this client is on (the server only takes its own map). `password` empty = none.
    pub fn connect(server: &str, name: &str, password: &str, map: &str) -> io::Result<Self> {
        let server = proto::resolve(server)?;
        let local = if server.is_ipv4() { SocketAddr::from(([0, 0, 0, 0], 0)) } else { SocketAddr::from(([0u16; 8], 0)) };
        let sock = UdpSocket::bind(local)?;
        sock.set_nonblocking(true)?;
        let mut name = proto::clean(name, proto::NAME_LEN);
        if name.trim().is_empty() {
            name = "Driver".into();
        }
        Ok(Self {
            sock,
            server,
            name,
            password: password.to_owned(),
            map: map.to_owned(),
            started: Instant::now(),
            id: None,
            token: 0,
            cookie: 0,
            seq: 1,
            last_hello: None,
            last_state: None,
            last_ping: None,
            last_player: None,
            last_heard: None,
            paused_until: None,
            stopped: false,
            rtt_ms: None,
        })
    }

    pub fn id(&self) -> Option<u8> {
        self.id
    }

    pub fn server(&self) -> SocketAddr {
        self.server
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Round trip of the last PING, ms.
    pub fn rtt_ms(&self) -> Option<u32> {
        self.rtt_ms
    }

    /// This client's clock, the `sent_ms` of its snapshots.
    pub fn clock_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    /// The map changed (the server will reject or welcome on the next HELLO).
    pub fn set_map(&mut self, map: &str) {
        if self.map != map {
            self.map = map.to_owned();
            self.forget();
            self.retry();
        }
    }

    /// Start joining again after a [`Event::Rejected`].
    pub fn retry(&mut self) {
        self.stopped = false;
        self.paused_until = None;
    }

    /// Tell the server we're going (best effort).
    pub fn leave(&mut self) {
        if self.token != 0 {
            let _ = self.sock.send_to(&encode(&Packet::Leave { id: 0, token: self.token }), self.server);
        }
        self.forget();
        self.stopped = true;
    }

    fn forget(&mut self) {
        self.id = None;
        self.token = 0;
        self.cookie = 0;
        self.last_player = None;
    }

    fn hello(&mut self, car: &str) {
        let proof = if self.password.is_empty() || self.cookie == 0 { 0 } else { password_proof(&self.password, self.cookie) };
        let pkt = Packet::Hello { name: self.name.clone(), map: self.map.clone(), car: car.to_owned(), cookie: self.cookie, proof };
        let _ = self.sock.send_to(&encode(&pkt), self.server);
        self.last_hello = Some(Instant::now());
    }

    /// Datagrams waiting from the server. Anything else is ignored.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if from != self.server {
                        continue;
                    }
                    let Some(packet) = decode(&buf[..n]) else { continue };
                    self.last_heard = Some(Instant::now());
                    match packet {
                        Packet::Challenge { cookie } => {
                            if self.id.is_none() {
                                self.cookie = cookie;
                                let car = self.last_player.as_ref().map(|p| p.1.car.clone()).unwrap_or_default();
                                self.hello(&car);
                            }
                        }
                        Packet::Welcome { id, max_players, token, map, server_name, motd, .. } => {
                            let fresh = self.token != token;
                            self.id = Some(id);
                            self.token = token;
                            if fresh {
                                // A new slot: everyone must hear who we are again.
                                self.last_player = None;
                                out.push(Event::Welcome { id, max_players, server_name, motd, map });
                            }
                        }
                        Packet::Full => {
                            self.paused_until = Some(Instant::now() + RETRY_FULL);
                            out.push(Event::Full);
                        }
                        Packet::Reject { reason, version, map } => {
                            if reason == RejectReason::TooMany {
                                self.paused_until = Some(Instant::now() + RETRY_FULL);
                            } else {
                                self.stopped = true;
                            }
                            self.forget();
                            out.push(Event::Rejected { reason, server_version: version, map });
                        }
                        Packet::State { snap, .. } if Some(snap.id) != self.id => out.push(Event::State(snap)),
                        Packet::Player { info, .. } if Some(info.id) != self.id => out.push(Event::Player(info)),
                        Packet::Leave { id, .. } if Some(id) != self.id => out.push(Event::Leave { id }),
                        Packet::Pong { client_ms, .. } => self.rtt_ms = Some(self.clock_ms().wrapping_sub(client_ms)),
                        _ => {}
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => break,
                // Windows reports an ICMP "port unreachable" as ConnectionReset on the next receive: no server there yet.
                Err(_) => break,
            }
        }
        if self.id.is_some() && self.last_heard.is_some_and(|t| t.elapsed() >= LOST_AFTER) {
            self.forget();
            self.last_heard = None;
            out.push(Event::Lost);
        }
        out
    }

    /// Call every frame: joins, then sends `state` (20 Hz; None = no car right now) and `me` (on change / every 3 s), and
    /// keeps the slot alive.
    pub fn tick(&mut self, state: Option<&Snapshot>, me: &PlayerInfo) {
        let now = Instant::now();
        if self.stopped || self.paused_until.is_some_and(|t| now < t) {
            return;
        }
        let Some(_) = self.id else {
            if self.last_hello.is_none_or(|t| now.duration_since(t) >= HELLO_EVERY) {
                // A cookie older than the hello interval may have expired: ask for a fresh one each round.
                if self.last_hello.is_some_and(|t| now.duration_since(t) >= HELLO_EVERY * 10) {
                    self.cookie = 0;
                }
                self.hello(&me.car);
            }
            return;
        };
        if self.last_ping.is_none_or(|t| now.duration_since(t) >= PING_EVERY) {
            let _ = self.sock.send_to(&encode(&Packet::Ping { token: self.token, client_ms: self.clock_ms() }), self.server);
            self.last_ping = Some(now);
            if self.last_heard.is_none() {
                self.last_heard = Some(now);
            }
        }
        let player_due = match &self.last_player {
            None => true,
            Some((t, p)) => p != me || now.duration_since(*t) >= PLAYER_EVERY,
        };
        if player_due {
            let _ = self.sock.send_to(&encode(&Packet::Player { token: self.token, info: me.clone() }), self.server);
            self.last_player = Some((now, me.clone()));
        }
        let Some(state) = state else { return };
        if self.last_state.is_some_and(|t| now.duration_since(t) < STATE_EVERY) {
            return;
        }
        let mut snap = state.clone();
        snap.seq = self.seq;
        snap.sent_ms = self.clock_ms();
        self.seq = self.seq.wrapping_add(1);
        if snap.sane() {
            let _ = self.sock.send_to(&encode(&Packet::State { token: self.token, snap }), self.server);
        }
        self.last_state = Some(now);
    }
}

/// Asks one server for its INFO (non-blocking): send with [`InfoQuery::send`], read with [`InfoQuery::poll`].
#[derive(Debug)]
pub struct InfoQuery {
    sock: UdpSocket,
    sent: Vec<(SocketAddr, u32, Instant)>,
}

impl InfoQuery {
    pub fn new() -> io::Result<Self> {
        let sock = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))?;
        sock.set_nonblocking(true)?;
        Ok(Self { sock, sent: Vec::new() })
    }

    pub fn send(&mut self, server: SocketAddr, nonce: u32) {
        let _ = self.sock.send_to(&encode(&Packet::InfoReq { nonce }), server);
        self.sent.push((server, nonce, Instant::now()));
    }

    /// Answers so far: (server, info, ping ms).
    pub fn poll(&mut self) -> Vec<(SocketAddr, ServerInfo, u32)> {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            if let Some(Packet::Info { nonce, info }) = decode(&buf[..n]) {
                if let Some(k) = self.sent.iter().position(|(a, n, _)| *a == from && *n == nonce) {
                    let (_, _, at) = self.sent.swap_remove(k);
                    out.push((from, info, at.elapsed().as_millis() as u32));
                }
            }
        }
        out
    }
}
