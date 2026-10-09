//! The server's session table. The binary wraps it in a UDP socket; tests call it directly.
//!
//! One server = one map ([`Config::map`]); a client on another map is told which one to load. Nothing is allocated for
//! an address until it has echoed a challenge cookie (see [`crate::proto`]), and every packet after the join must carry
//! the slot's secret token, so a forged source address can neither take a slot nor receive the relay's traffic.
//!
//! Interest management (24 players): a player's snapshots go to others at the full 20 Hz within [`NEAR_M`], at 5 Hz
//! within [`MID_M`], and once a second beyond. Worst case (everyone close): 23 x 20 x 81 bytes = 37 KB/s down per
//! client, ~0.9 MB/s up for the server.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::{IpAddr, SocketAddr};

use crate::proto::{clean, password_proof, sanitize_name, Packet, PlayerInfo, RejectReason, ServerInfo, Snapshot, MAP_LEN, MAX_PLAYERS, MOTD_LEN, NAME_LEN, SERVER_NAME_LEN, VERSION};

/// Drop a slot that has been silent this long (clients ping every second).
pub const TIMEOUT_MS: u64 = 10_000;
/// Fastest a slot's snapshots are forwarded (the game sends at 20 Hz).
const MIN_RELAY_MS: u64 = 15;
/// Distance tiers of the snapshot fan-out (m) and their minimum interval (ms).
pub const NEAR_M: f32 = 500.0;
pub const MID_M: f32 = 2_000.0;
const MID_MS: u64 = 200;
const FAR_MS: u64 = 1_000;
/// Fastest a slot's PLAYER (name / car / paint) is forwarded.
const MIN_PLAYER_MS: u64 = 500;
/// A challenge cookie is good for this bucket and the one before (so 30-60 s).
const COOKIE_BUCKET_MS: u64 = 30_000;
/// Unauthenticated replies (CHALLENGE, INFO, REJECT) per second, server-wide.
const UNAUTH_PER_S: u32 = 400;

#[derive(Clone, Debug)]
pub struct Config {
    pub name: String,
    pub map: String,
    pub max_players: usize,
    /// Empty = no password.
    pub password: String,
    pub motd: String,
    /// Slots one IP address may hold.
    pub per_ip: usize,
    pub banned: Vec<IpAddr>,
}

impl Default for Config {
    fn default() -> Self {
        Self { name: "FH1 server".into(), map: "colorado".into(), max_players: 24, password: String::new(), motd: String::new(), per_ip: 2, banned: Vec::new() }
    }
}

impl Config {
    /// Fields cut to their wire sizes, players clamped to the protocol cap.
    pub fn normalised(mut self) -> Self {
        self.name = clean(&self.name, SERVER_NAME_LEN);
        self.map = clean(&self.map, MAP_LEN);
        self.motd = clean(&self.motd, MOTD_LEN);
        self.max_players = self.max_players.clamp(1, MAX_PLAYERS);
        self.per_ip = self.per_ip.max(1);
        self
    }
}

#[derive(Clone, Debug)]
struct Slot {
    addr: SocketAddr,
    token: u64,
    name: String,
    info: Option<PlayerInfo>,
    last_ms: u64,
    last_state_ms: Option<u64>,
    last_player_ms: Option<u64>,
    position: Option<[f32; 3]>,
    /// When this slot's snapshot was last sent to each other slot (index = receiver).
    sent_to: [Option<u64>; MAX_PLAYERS],
}

/// What the server should send, and what to log.
#[derive(Clone, Debug, Default)]
pub struct Handled {
    pub packets: Vec<(SocketAddr, Packet)>,
    /// A slot was taken: (id, name).
    pub joined: Option<(u8, String)>,
    /// A slot was freed: (id, name).
    pub left: Option<(u8, String)>,
}

pub struct Relay {
    cfg: Config,
    slots: Vec<Option<Slot>>,
    secret: RandomState,
    tokens_made: u64,
    unauth: (u64, u32),
}

impl Relay {
    pub fn new(cfg: Config) -> Self {
        let cfg = cfg.normalised();
        Self { slots: vec![None; cfg.max_players], cfg, secret: RandomState::new(), tokens_made: 0, unauth: (0, 0) }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn players(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// The INFO a browser sees.
    pub fn info(&self) -> ServerInfo {
        ServerInfo { name: self.cfg.name.clone(), map: self.cfg.map.clone(), players: self.players() as u8, max_players: self.cfg.max_players as u8, password: !self.cfg.password.is_empty(), version: VERSION }
    }

    /// One datagram as received: decodes it, answers other protocol versions with a REJECT that is no larger than the
    /// datagram, and handles the rest.
    pub fn handle_datagram(&mut self, from: SocketAddr, bytes: &[u8], now_ms: u64) -> Handled {
        if let Some(packet) = crate::proto::decode(bytes) {
            return self.handle(from, packet, now_ms);
        }
        if crate::proto::peek_version(bytes).is_some_and(|v| v != VERSION) && bytes.len() >= 12 && self.unauth_ok(now_ms) {
            return Handled { packets: vec![(from, Packet::Reject { reason: RejectReason::Version, version: VERSION, map: String::new() })], ..Default::default() };
        }
        Handled::default()
    }

    pub fn handle(&mut self, from: SocketAddr, packet: Packet, now_ms: u64) -> Handled {
        match packet {
            Packet::Hello { name, map, cookie, proof, .. } => self.hello(from, name, map, cookie, proof, now_ms),
            Packet::State { token, snap } => self.state(from, token, snap, now_ms),
            Packet::Player { token, info } => self.player(from, token, info, now_ms),
            Packet::Ping { token, client_ms } => match self.by_token(token, from, now_ms) {
                Some(_) => Handled { packets: vec![(from, Packet::Pong { client_ms, server_ms: now_ms as u32 })], ..Default::default() },
                None => Handled::default(),
            },
            Packet::Leave { token, .. } => match self.by_token(token, from, now_ms) {
                Some(i) => self.disconnect(i),
                None => Handled::default(),
            },
            Packet::InfoReq { nonce } if self.unauth_ok(now_ms) => Handled { packets: vec![(from, Packet::Info { nonce, info: self.info() })], ..Default::default() },
            _ => Handled::default(),
        }
    }

    /// Free slots that stopped sending. Call a few times a second.
    pub fn expire(&mut self, now_ms: u64) -> Handled {
        let mut out = Handled::default();
        let stale: Vec<usize> = (0..self.slots.len()).filter(|&i| self.slots[i].as_ref().is_some_and(|s| now_ms.saturating_sub(s.last_ms) >= TIMEOUT_MS)).collect();
        for i in stale {
            let h = self.disconnect(i);
            out.packets.extend(h.packets);
            out.left = h.left.or(out.left);
        }
        out
    }

    /// Removes the player with this id (console `kick`). Returns their name.
    pub fn kick(&mut self, id: u8) -> Option<(String, Handled)> {
        let i = (id as usize).checked_sub(1).filter(|&i| self.slots.get(i).is_some_and(|s| s.is_some()))?;
        let name = self.slots[i].as_ref().unwrap().name.clone();
        Some((name, self.disconnect(i)))
    }

    /// Refuses this address from now on (until restart; the config's `ban` lines are permanent).
    pub fn ban(&mut self, ip: IpAddr) {
        if !self.cfg.banned.contains(&ip) {
            self.cfg.banned.push(ip);
        }
    }

    /// (id, name, address) of every player.
    pub fn list(&self) -> Vec<(u8, String, SocketAddr)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i as u8 + 1, s.name.clone(), s.addr))).collect()
    }

    fn unauth_ok(&mut self, now_ms: u64) -> bool {
        let sec = now_ms / 1000;
        if self.unauth.0 != sec {
            self.unauth = (sec, 0);
        }
        self.unauth.1 += 1;
        self.unauth.1 <= UNAUTH_PER_S
    }

    fn cookie(&self, from: SocketAddr, bucket: u64) -> u64 {
        self.secret.hash_one((from.ip(), from.port(), bucket)).max(1)
    }

    fn cookie_ok(&self, from: SocketAddr, cookie: u64, now_ms: u64) -> bool {
        let b = now_ms / COOKIE_BUCKET_MS;
        cookie != 0 && (cookie == self.cookie(from, b) || (b > 0 && cookie == self.cookie(from, b - 1)))
    }

    fn hello(&mut self, from: SocketAddr, name: String, map: String, cookie: u64, proof: u64, now_ms: u64) -> Handled {
        let reply = |p: Packet| Handled { packets: vec![(from, p)], ..Default::default() };
        if !self.cookie_ok(from, cookie, now_ms) {
            if !self.unauth_ok(now_ms) {
                return Handled::default();
            }
            return reply(Packet::Challenge { cookie: self.cookie(from, now_ms / COOKIE_BUCKET_MS) });
        }
        if self.cfg.banned.contains(&from.ip()) {
            return reply(Packet::Reject { reason: RejectReason::Banned, version: VERSION, map: String::new() });
        }
        if map != self.cfg.map {
            return reply(Packet::Reject { reason: RejectReason::WrongMap, version: VERSION, map: self.cfg.map.clone() });
        }
        if !self.cfg.password.is_empty() && proof != password_proof(&self.cfg.password, cookie) {
            return reply(Packet::Reject { reason: RejectReason::BadPassword, version: VERSION, map: String::new() });
        }
        // A repeated HELLO from a joined address (lost WELCOME): the same slot and token again.
        if let Some(i) = self.slots.iter().position(|s| s.as_ref().is_some_and(|s| s.addr == from)) {
            let s = self.slots[i].as_mut().unwrap();
            s.last_ms = now_ms;
            let token = s.token;
            return reply(self.welcome(i, token, now_ms));
        }
        if self.slots.iter().flatten().filter(|s| s.addr.ip() == from.ip()).count() >= self.cfg.per_ip {
            return reply(Packet::Reject { reason: RejectReason::TooMany, version: VERSION, map: String::new() });
        }
        let Some(i) = self.slots.iter().position(|s| s.is_none()) else { return reply(Packet::Full) };
        self.tokens_made += 1;
        let token = self.secret.hash_one((self.tokens_made, now_ms, from.ip(), from.port(), 0xF1u8)).max(1);
        // The server's name rule (proto `sanitize_name`, same as the launcher's): a newer client already sends it clean.
        let name = sanitize_name(&clean(&name, NAME_LEN)).unwrap_or_else(|| format!("Driver {}", i + 1));
        self.slots[i] = Some(Slot { addr: from, token, name: name.clone(), info: None, last_ms: now_ms, last_state_ms: None, last_player_ms: None, position: None, sent_to: [None; MAX_PLAYERS] });
        let mut packets = vec![(from, self.welcome(i, token, now_ms))];
        // The newcomer learns who is already here.
        for (j, s) in self.slots.iter().enumerate() {
            if let (true, Some(info)) = (j != i, s.as_ref().and_then(|s| s.info.clone())) {
                packets.push((from, Packet::Player { token: 0, info }));
            }
        }
        Handled { packets, joined: Some((i as u8 + 1, name)), left: None }
    }

    fn welcome(&self, i: usize, token: u64, now_ms: u64) -> Packet {
        Packet::Welcome { id: i as u8 + 1, max_players: self.cfg.max_players as u8, token, server_ms: now_ms as u32, map: self.cfg.map.clone(), server_name: self.cfg.name.clone(), motd: self.cfg.motd.clone() }
    }

    /// The slot holding `token`. A known token from a new address (NAT rebinding) moves the slot there.
    fn by_token(&mut self, token: u64, from: SocketAddr, now_ms: u64) -> Option<usize> {
        if token == 0 {
            return None;
        }
        let i = self.slots.iter().position(|s| s.as_ref().is_some_and(|s| s.token == token))?;
        let s = self.slots[i].as_mut().unwrap();
        s.addr = from;
        s.last_ms = now_ms;
        Some(i)
    }

    fn state(&mut self, from: SocketAddr, token: u64, mut snap: Snapshot, now_ms: u64) -> Handled {
        let Some(i) = self.by_token(token, from, now_ms) else { return Handled::default() };
        let s = self.slots[i].as_mut().unwrap();
        if s.last_state_ms.is_some_and(|t| now_ms.saturating_sub(t) < MIN_RELAY_MS) {
            return Handled::default();
        }
        s.last_state_ms = Some(now_ms);
        s.position = Some(snap.position);
        snap.id = i as u8 + 1;
        let here = snap.position;
        let mut packets = Vec::new();
        for j in 0..self.slots.len() {
            if j == i {
                continue;
            }
            let Some(to) = self.slots[j].as_ref() else { continue };
            let (to_addr, to_pos) = (to.addr, to.position);
            let gap = match to_pos {
                Some(p) => {
                    let d2 = (0..3).map(|k| (p[k] - here[k]).powi(2)).sum::<f32>();
                    if d2 <= NEAR_M * NEAR_M {
                        0
                    } else if d2 <= MID_M * MID_M {
                        MID_MS
                    } else {
                        FAR_MS
                    }
                }
                None => 0,
            };
            let s = self.slots[i].as_mut().unwrap();
            if s.sent_to[j].is_some_and(|t| now_ms.saturating_sub(t) < gap) {
                continue;
            }
            s.sent_to[j] = Some(now_ms);
            packets.push((to_addr, Packet::State { token: 0, snap: snap.clone() }));
        }
        Handled { packets, ..Default::default() }
    }

    fn player(&mut self, from: SocketAddr, token: u64, mut info: PlayerInfo, now_ms: u64) -> Handled {
        let Some(i) = self.by_token(token, from, now_ms) else { return Handled::default() };
        let s = self.slots[i].as_mut().unwrap();
        info.id = i as u8 + 1;
        info.name = s.name.clone();
        let changed = s.info.as_ref() != Some(&info);
        if !changed && s.last_player_ms.is_some_and(|t| now_ms.saturating_sub(t) < MIN_PLAYER_MS) {
            return Handled::default();
        }
        s.last_player_ms = Some(now_ms);
        s.info = Some(info.clone());
        let packets = self.others(i).into_iter().map(|a| (a, Packet::Player { token: 0, info: info.clone() })).collect();
        Handled { packets, ..Default::default() }
    }

    fn others(&self, i: usize) -> Vec<SocketAddr> {
        self.slots.iter().enumerate().filter(|(j, _)| *j != i).filter_map(|(_, s)| s.as_ref().map(|s| s.addr)).collect()
    }

    fn disconnect(&mut self, i: usize) -> Handled {
        let Some(s) = self.slots[i].take() else { return Handled::default() };
        for other in self.slots.iter_mut().flatten() {
            other.sent_to[i] = None;
        }
        let id = i as u8 + 1;
        let packets = self.others(i).into_iter().map(|a| (a, Packet::Leave { id, token: 0 })).collect();
        Handled { packets, joined: None, left: Some((id, s.name)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{encode, tests::snap};

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 4000))
    }

    fn hello(cookie: u64, proof: u64) -> Packet {
        Packet::Hello { name: "Ada".into(), map: "colorado".into(), car: "CAR".into(), cookie, proof }
    }

    /// Full handshake from `from`; the WELCOME's token.
    fn join(r: &mut Relay, from: SocketAddr, now: u64) -> u64 {
        let h = r.handle(from, hello(0, 0), now);
        let Packet::Challenge { cookie } = h.packets[0].1 else { panic!("no challenge: {:?}", h.packets) };
        let proof = if r.cfg.password.is_empty() { 0 } else { password_proof(&r.cfg.password, cookie) };
        let h = r.handle(from, hello(cookie, proof), now);
        match &h.packets[0].1 {
            Packet::Welcome { token, .. } => *token,
            p => panic!("not welcomed: {p:?}"),
        }
    }

    fn at(p: [f32; 3]) -> Snapshot {
        let mut s = snap();
        s.position = p;
        s
    }

    #[test]
    fn no_slot_and_no_traffic_without_the_cookie() {
        let mut r = Relay::new(Config::default());
        // A forged HELLO (any cookie) only gets a small CHALLENGE back, and nothing is allocated.
        let h = r.handle(addr(1), hello(12345, 0), 0);
        assert!(matches!(h.packets[..], [(_, Packet::Challenge { .. })]));
        assert_eq!(r.players(), 0);
        let t1 = join(&mut r, addr(2), 0);
        // A cookie is bound to the address that asked for it.
        let h = r.handle(addr(3), hello(0, 0), 0);
        let Packet::Challenge { cookie } = h.packets[0].1 else { panic!() };
        assert!(matches!(r.handle(addr(4), hello(cookie, 0), 0).packets[0].1, Packet::Challenge { .. }));
        // States need the token; a guessed token relays nothing.
        assert!(r.handle(addr(1), Packet::State { token: t1 ^ 1, snap: snap() }, 10).packets.is_empty());
    }

    #[test]
    fn cookie_expires() {
        let mut r = Relay::new(Config::default());
        let Packet::Challenge { cookie } = r.handle(addr(1), hello(0, 0), 0).packets[0].1 else { panic!() };
        assert!(matches!(r.handle(addr(1), hello(cookie, 0), COOKIE_BUCKET_MS * 2 + 1).packets[0].1, Packet::Challenge { .. }));
    }

    #[test]
    fn map_password_and_per_ip_are_enforced() {
        let mut r = Relay::new(Config { password: "pw".into(), per_ip: 1, ..Config::default() });
        let Packet::Challenge { cookie } = r.handle(addr(1), hello(0, 0), 0).packets[0].1 else { panic!() };
        let wrong_map = Packet::Hello { name: "a".into(), map: "flat".into(), car: "c".into(), cookie, proof: password_proof("pw", cookie) };
        assert!(matches!(&r.handle(addr(1), wrong_map, 0).packets[0].1, Packet::Reject { reason: RejectReason::WrongMap, map, .. } if map == "colorado"));
        assert!(matches!(r.handle(addr(1), hello(cookie, password_proof("nope", cookie)), 0).packets[0].1, Packet::Reject { reason: RejectReason::BadPassword, .. }));
        join(&mut r, addr(1), 0);
        let second = SocketAddr::from(([10, 0, 0, 1], 4001));
        let Packet::Challenge { cookie } = r.handle(second, hello(0, 0), 0).packets[0].1 else { panic!() };
        assert!(matches!(r.handle(second, hello(cookie, password_proof("pw", cookie)), 0).packets[0].1, Packet::Reject { reason: RejectReason::TooMany, .. }));
    }

    #[test]
    fn states_relay_without_tokens_and_by_distance() {
        let mut r = Relay::new(Config::default());
        let (a, b, c) = (join(&mut r, addr(1), 0), join(&mut r, addr(2), 0), join(&mut r, addr(3), 0));
        r.handle(addr(2), Packet::State { token: b, snap: at([0.0, 0.0, 100.0]) }, 0);
        r.handle(addr(3), Packet::State { token: c, snap: at([0.0, 0.0, 5_000.0]) }, 0);
        let h = r.handle(addr(1), Packet::State { token: a, snap: at([0.0, 0.0, 0.0]) }, 100);
        assert!(h.packets.iter().all(|(_, p)| matches!(p, Packet::State { token: 0, snap } if snap.id == 1)));
        assert_eq!(h.packets.len(), 2);
        // 50 ms later: the near player gets it again, the far one (5 km) waits a second.
        let h = r.handle(addr(1), Packet::State { token: a, snap: at([0.0, 0.0, 0.0]) }, 150);
        assert_eq!(h.packets.iter().map(|(to, _)| *to).collect::<Vec<_>>(), vec![addr(2)]);
        let h = r.handle(addr(1), Packet::State { token: a, snap: at([0.0, 0.0, 0.0]) }, 1_100);
        assert_eq!(h.packets.len(), 2);
    }

    #[test]
    fn flood_is_capped() {
        let mut r = Relay::new(Config::default());
        let a = join(&mut r, addr(1), 0);
        join(&mut r, addr(2), 0);
        assert_eq!(r.handle(addr(1), Packet::State { token: a, snap: snap() }, 1_000).packets.len(), 1);
        assert!(r.handle(addr(1), Packet::State { token: a, snap: snap() }, 1_005).packets.is_empty());
        assert_eq!(r.handle(addr(1), Packet::State { token: a, snap: snap() }, 1_000 + MIN_RELAY_MS).packets.len(), 1);
    }

    #[test]
    fn players_learn_each_other_and_names_are_the_servers() {
        let mut r = Relay::new(Config::default());
        let a = join(&mut r, addr(1), 0);
        let info = PlayerInfo { id: 9, name: "Impostor".into(), car: "VW".into(), paint_seq: 1, paint_rgb: 0, paint_flags: 0, rim: String::new(), kit: [crate::proto::KIT_STOCK; crate::proto::KIT_SLOTS], look_flags: 0 };
        r.handle(addr(1), Packet::Player { token: a, info }, 10);
        // The second player gets the first one's PLAYER with the WELCOME, under the server's name and id.
        let h = r.handle(addr(2), hello(0, 0), 20);
        let Packet::Challenge { cookie } = h.packets[0].1 else { panic!() };
        let h = r.handle(addr(2), hello(cookie, 0), 20);
        assert!(h.packets.iter().any(|(to, p)| *to == addr(2) && matches!(p, Packet::Player { token: 0, info } if info.id == 1 && info.name == "Ada")));
    }

    #[test]
    fn full_server_timeout_and_nat_rebinding() {
        let mut r = Relay::new(Config { max_players: 2, per_ip: 4, ..Config::default() });
        let a = join(&mut r, addr(1), 0);
        join(&mut r, addr(2), 0);
        let h = r.handle(addr(3), hello(0, 0), 0);
        let Packet::Challenge { cookie } = h.packets[0].1 else { panic!() };
        assert!(matches!(r.handle(addr(3), hello(cookie, 0), 0).packets[0].1, Packet::Full));
        // Player 1's NAT moves it to a new port: the token keeps the slot.
        let moved = SocketAddr::from(([10, 0, 0, 1], 5000));
        assert!(matches!(r.handle(moved, Packet::Ping { token: a, client_ms: 1 }, 5_000).packets[0], (to, Packet::Pong { .. }) if to == moved));
        // Player 2 never spoke again: gone after the timeout, the others are told.
        let gone = r.expire(TIMEOUT_MS);
        assert_eq!(gone.left.as_ref().map(|l| l.0), Some(2));
        assert!(gone.packets.iter().any(|(to, p)| *to == moved && matches!(p, Packet::Leave { id: 2, token: 0 })));
        assert_eq!(r.players(), 1);
    }

    #[test]
    fn other_versions_get_a_small_reject_and_info_is_small() {
        let mut r = Relay::new(Config::default());
        let mut v1 = encode(&Packet::Hello { name: "a".into(), map: "m".into(), car: "c".into(), cookie: 0, proof: 0 });
        v1[8] = 1;
        let h = r.handle_datagram(addr(1), &v1, 0);
        assert!(matches!(h.packets[..], [(_, Packet::Reject { reason: RejectReason::Version, version: VERSION, .. })]));
        assert!(encode(&h.packets[0].1).len() <= v1.len());
        let req = encode(&Packet::InfoReq { nonce: 7 });
        let h = r.handle_datagram(addr(2), &req, 0);
        let (_, reply) = &h.packets[0];
        assert!(matches!(reply, Packet::Info { nonce: 7, info } if info.max_players == 24 && info.map == "colorado"));
        assert!(encode(reply).len() <= req.len());
    }

    #[test]
    fn unauthenticated_replies_are_rate_limited() {
        let mut r = Relay::new(Config::default());
        let mut answered = 0;
        for k in 0..2_000u32 {
            let from = SocketAddr::from(([10, 1, (k >> 8) as u8, k as u8], 4000));
            answered += r.handle(from, hello(0, 0), 500).packets.len();
        }
        assert_eq!(answered, UNAUTH_PER_S as usize);
    }
}
