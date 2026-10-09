//! Little-endian UDP packets, protocol version 2 (2026-10-08: v1 had no handshake, so a forged Hello made a public
//! server stream other players' snapshots at any address; and every snapshot carried map/car/name strings, ~290 bytes,
//! too much for 24 players). One datagram is one message.
//!
//! ```text
//! 0..8  magic b"FH1NET01"
//! 8     version (2)
//! 9     kind
//! 10..  body
//! ```
//!
//! Join: HELLO (cookie 0) -> CHALLENGE(cookie) -> HELLO(cookie, password proof) -> WELCOME(id, token, server info), or
//! REJECT / FULL. The cookie proves the client receives at its address; nothing is allocated before it. Every later
//! client packet carries the secret token, and tokens are never relayed. Every reply to a packet that is not from a
//! joined player is no larger than that packet, so the server can't amplify traffic at a forged address.
//!
//! In play: STATE (motion only, 81 bytes) 20 times a second; PLAYER (name, car, paint) when it changes and every few
//! seconds; PING/PONG once a second for liveness and the ping shown in the browser and HUD.
//!
//! Additive extensions (2026-10-09, "v2 ext 1", still VERSION 2): fields appended to the END of a packet. A decoder
//! never checks that a datagram is fully read, so an older peer or server reads the old prefix and ignores the tail; a
//! newer decoder treats a missing tail as "not sent" (defaults). The relay decodes and re-encodes, so an old server
//! drops the tail (receivers then fall back) and a new one carries it.
//! - STATE + [`WheelExt`] (19 bytes): per-wheel spin, normalised slip and slip angle, contact bits, boost, shift count
//!   (tyre smoke / skid marks / surface FX and engine audio of remote cars).
//! - PLAYER + looks (38 bytes): rim style media name, body kit Sequence per slot, look flags (roll cage). Sent with
//!   PLAYER (on change, every 3 s, to newcomers), never per tick.

use std::net::{SocketAddr, ToSocketAddrs};

pub const MAGIC: &[u8; 8] = b"FH1NET01";
pub const VERSION: u8 = 2;
/// Protocol cap on players per server (ids 1..=MAX_PLAYERS); a server's `max_players` is at most this.
pub const MAX_PLAYERS: usize = 32;

pub const NAME_LEN: usize = 24;
pub const MAP_LEN: usize = 64;
pub const CAR_LEN: usize = 96;
pub const SERVER_NAME_LEN: usize = 48;
pub const MOTD_LEN: usize = 64;
/// PLAYER ext: the rim style's media name.
pub const RIM_LEN: usize = 32;
/// PLAYER ext: body kit slots (front bumper, rear bumper, side skirts, hood, rear wing), each a gamedb Sequence.
pub const KIT_SLOTS: usize = 5;
/// PLAYER ext: kit slot not set (stock).
pub const KIT_STOCK: u8 = 0xFF;
/// PLAYER ext look flags: the roll cage (race weight reduction) is fitted.
pub const LOOK_CAGE: u8 = 1;
/// Longest display name in characters (the wire field is [`NAME_LEN`] bytes).
pub const NAME_MAX_CHARS: usize = 16;

const HELLO: u8 = 1;
const WELCOME: u8 = 2;
const STATE: u8 = 3;
const LEAVE: u8 = 4;
const FULL: u8 = 5;
const CHALLENGE: u8 = 6;
const REJECT: u8 = 7;
const INFO_REQ: u8 = 8;
const INFO: u8 = 9;
const PING: u8 = 10;
const PONG: u8 = 11;
const PLAYER: u8 = 12;
const HEARTBEAT: u8 = 13;
const LIST_REQ: u8 = 14;
const LIST: u8 = 15;

/// LIST_REQ is padded to this many bytes; a LIST page is never larger.
pub const LIST_REQ_LEN: usize = 512;
/// Servers per LIST page (19 bytes each: family, 16-byte address, port).
pub const LIST_PAGE: usize = 26;

/// INFO_REQ is padded to this many bytes (the INFO reply is shorter), so a browser query can't amplify.
pub const INFO_REQ_LEN: usize = 160;

pub const FLAG_BRAKE: u8 = 1;
pub const FLAG_REVERSE: u8 = 2;
/// PLAYER paint flags.
pub const FLAG_CUSTOM_PAINT: u8 = 4;
pub const FLAG_METALLIC: u8 = 8;

/// INFO flags.
pub const INFO_PASSWORD: u8 = 1;

/// Speed, position and spin caps. A snapshot outside these is dropped, not relayed.
const MAX_SPEED: f32 = 200.0;
const MAX_ABS_POS: f32 = 30_000.0;
const MAX_SPIN: f32 = 80.0;
const MAX_RPM: f32 = 20_000.0;
const MAX_STEER: f32 = 1.2;
/// Angular velocity on the wire: i16 per axis in units of 1/400 rad/s (±81.9 rad/s).
const SPIN_SCALE: f32 = 400.0;
/// Wheel spin on the wire: i16 in units of 1/16 rad/s (±2048 rad/s).
const OMEGA_SCALE: f32 = 16.0;
/// Normalised slip / slip angle on the wire: i8 in units of 1/32 (±3.97).
const SLIP_SCALE: f32 = 32.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// The other side speaks another protocol version (`version` = the server's).
    Version,
    /// This server runs another map (`map` = the server's).
    WrongMap,
    BadPassword,
    /// Too many players from this address.
    TooMany,
    Banned,
}

impl RejectReason {
    fn code(self) -> u8 {
        match self {
            Self::Version => 1,
            Self::WrongMap => 2,
            Self::BadPassword => 3,
            Self::TooMany => 4,
            Self::Banned => 5,
        }
    }
    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => Self::Version,
            2 => Self::WrongMap,
            3 => Self::BadPassword,
            4 => Self::TooMany,
            5 => Self::Banned,
            _ => return None,
        })
    }
}

/// A car's motion, 20 times a second.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// Set by the server.
    pub id: u8,
    pub seq: u32,
    /// Sender's clock (ms since its client started): receivers interpolate on this timeline, not on arrival times.
    pub sent_ms: u32,
    /// FLAG_BRAKE | FLAG_REVERSE.
    pub flags: u8,
    pub gear: u8,
    pub rpm: f32,
    pub steer: f32,
    /// 0..1
    pub throttle: f32,
    /// 0..1
    pub brake: f32,
    /// Suspension drop from the modelled hub, millimetres, LF RF LR RR.
    pub wheel_drop_mm: [i8; 4],
    pub position: [f32; 3],
    /// xyzw (sent as four i16, normalised again on arrival).
    pub rotation: [f32; 4],
    pub velocity: [f32; 3],
    pub angular: [f32; 3],
    /// Per-wheel state for the receivers' tyre FX (None = an older sender, or relayed by an older server).
    pub ext: Option<WheelExt>,
}

/// STATE extension (module doc): what the tyre effects and engine audio of a remote car need beyond its motion.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct WheelExt {
    /// Wheel spin (rad/s), LF RF LR RR.
    pub omega: [f32; 4],
    /// Normalised longitudinal slip (1 = the grip peak).
    pub slip: [f32; 4],
    /// Normalised slip angle (1 = the grip peak).
    pub slip_angle: [f32; 4],
    /// Bit i = wheel i on the ground.
    pub grounded: u8,
    /// Boost 0..1 (turbo / supercharger).
    pub boost: f32,
    /// Gear changes so far, wrapping (receivers play a shift on each step).
    pub shifts: u8,
}

/// The multiplayer display name `raw` stands for: letters, digits, spaces and `- _ . '` only (others dropped), runs of
/// spaces collapsed, trimmed, at most [`NAME_MAX_CHARS`] characters and [`NAME_LEN`] bytes. None = fewer than 2
/// characters left (the caller falls back to a default). Same rule as the launcher's name box.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | '\'') {
            if out.chars().count() >= NAME_MAX_CHARS || out.len() + ch.len_utf8() > NAME_LEN {
                break;
            }
            out.push(ch);
        }
    }
    let out = out.trim().to_owned();
    (out.chars().count() >= 2).then_some(out)
}

impl Snapshot {
    pub fn sane(&self) -> bool {
        let finite = |v: &[f32]| v.iter().all(|x| x.is_finite());
        if !finite(&self.position) || !finite(&self.velocity) || !finite(&self.angular) || !finite(&self.rotation) {
            return false;
        }
        if !self.rpm.is_finite() || !self.steer.is_finite() || !self.throttle.is_finite() || !self.brake.is_finite() {
            return false;
        }
        if self.position.iter().any(|x| x.abs() > MAX_ABS_POS) {
            return false;
        }
        let speed2 = self.velocity.iter().map(|v| v * v).sum::<f32>();
        if speed2 > MAX_SPEED * MAX_SPEED {
            return false;
        }
        if self.angular.iter().any(|x| x.abs() > MAX_SPIN) || !(0.0..MAX_RPM).contains(&self.rpm) || self.steer.abs() > MAX_STEER {
            return false;
        }
        if !(0.0..=1.0).contains(&self.throttle) || !(0.0..=1.0).contains(&self.brake) {
            return false;
        }
        let q = self.rotation.iter().map(|v| v * v).sum::<f32>();
        (0.5..1.5).contains(&q)
    }
}

/// Who a player is and what they drive. Sent on join, on change and every few seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerInfo {
    /// Set by the server.
    pub id: u8,
    /// Set by the server from the joined name (clients can't rename themselves).
    pub name: String,
    /// Car folder under `cars/`, or an imported path (`../imported/...`); receivers only load cars in their own garage.
    pub car: String,
    pub paint_seq: u32,
    pub paint_rgb: u32,
    /// FLAG_CUSTOM_PAINT | FLAG_METALLIC.
    pub paint_flags: u8,
    /// Ext: the rim style (media name; empty = stock). Receivers only use rims their own install lists.
    pub rim: String,
    /// Ext: body kit Sequence per slot ([`KIT_STOCK`] = stock); receivers only use rows their install has.
    pub kit: [u8; KIT_SLOTS],
    /// Ext: [`LOOK_CAGE`].
    pub look_flags: u8,
}

impl PlayerInfo {
    /// Stock looks (no ext): what an older sender's PLAYER decodes to.
    pub fn stock_looks(&mut self) {
        self.rim.clear();
        self.kit = [KIT_STOCK; KIT_SLOTS];
        self.look_flags = 0;
    }
}

/// What WELCOME and INFO tell a client about the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub map: String,
    pub players: u8,
    pub max_players: u8,
    pub password: bool,
    pub version: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Packet {
    /// `cookie` 0 = asking for a CHALLENGE. `proof` = [`password_proof`] of the cookie (0 without a password).
    Hello { name: String, map: String, car: String, cookie: u64, proof: u64 },
    Challenge { cookie: u64 },
    Welcome { id: u8, max_players: u8, token: u64, server_ms: u32, map: String, server_name: String, motd: String },
    Reject { reason: RejectReason, version: u8, map: String },
    Full,
    /// Client -> server: `token` set. Server -> clients: `token` 0.
    State { token: u64, snap: Snapshot },
    /// Client -> server: `token` set. Server -> clients: `token` 0.
    Player { token: u64, info: PlayerInfo },
    /// Client -> server: `token` (and `id` 0). Server -> clients: `id` of the player who left (`token` 0).
    Leave { id: u8, token: u64 },
    Ping { token: u64, client_ms: u32 },
    Pong { client_ms: u32, server_ms: u32 },
    InfoReq { nonce: u32 },
    Info { nonce: u32, info: ServerInfo },
    /// Game server -> registry, from its game socket (cookie 0 = asking for a CHALLENGE).
    Heartbeat { cookie: u64 },
    /// Browser -> registry: page `page` of the server list.
    ListReq { page: u16 },
    /// Registry -> browser.
    List { page: u16, pages: u16, servers: Vec<SocketAddr> },
}

pub fn encode(packet: &Packet) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    match packet {
        Packet::Hello { name, map, car, cookie, proof } => {
            out.push(HELLO);
            put_str(&mut out, NAME_LEN, name);
            put_str(&mut out, MAP_LEN, map);
            put_str(&mut out, CAR_LEN, car);
            put_u64(&mut out, *cookie);
            put_u64(&mut out, *proof);
        }
        Packet::Challenge { cookie } => {
            out.push(CHALLENGE);
            put_u64(&mut out, *cookie);
        }
        Packet::Welcome { id, max_players, token, server_ms, map, server_name, motd } => {
            out.push(WELCOME);
            out.push(*id);
            out.push(*max_players);
            put_u64(&mut out, *token);
            put_u32(&mut out, *server_ms);
            put_str(&mut out, MAP_LEN, map);
            put_str(&mut out, SERVER_NAME_LEN, server_name);
            put_str(&mut out, MOTD_LEN, motd);
        }
        Packet::Reject { reason, version, map } => {
            out.push(REJECT);
            out.push(reason.code());
            out.push(*version);
            if *reason == RejectReason::WrongMap {
                put_str(&mut out, MAP_LEN, map);
            }
        }
        Packet::Full => out.push(FULL),
        Packet::State { token, snap: s } => {
            out.push(STATE);
            put_u64(&mut out, *token);
            out.push(s.id);
            put_u32(&mut out, s.seq);
            put_u32(&mut out, s.sent_ms);
            out.push(s.flags);
            out.push(s.gear);
            put_f32(&mut out, s.rpm);
            put_f32(&mut out, s.steer);
            out.push((s.throttle.clamp(0.0, 1.0) * 255.0).round() as u8);
            out.push((s.brake.clamp(0.0, 1.0) * 255.0).round() as u8);
            out.extend_from_slice(&s.wheel_drop_mm.map(|v| v as u8));
            for v in s.position {
                put_f32(&mut out, v);
            }
            let n = s.rotation.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
            for v in s.rotation {
                put_i16(&mut out, ((v / n).clamp(-1.0, 1.0) * 32767.0).round() as i16);
            }
            for v in s.velocity {
                put_f32(&mut out, v);
            }
            for v in s.angular {
                put_i16(&mut out, (v * SPIN_SCALE).round().clamp(-32767.0, 32767.0) as i16);
            }
            if let Some(x) = &s.ext {
                for v in x.omega {
                    put_i16(&mut out, if v.is_finite() { (v * OMEGA_SCALE).round().clamp(-32767.0, 32767.0) as i16 } else { 0 });
                }
                let q8 = |v: f32| if v.is_finite() { (v * SLIP_SCALE).round().clamp(-127.0, 127.0) as i8 as u8 } else { 0 };
                out.extend_from_slice(&x.slip.map(q8));
                out.extend_from_slice(&x.slip_angle.map(q8));
                out.push(x.grounded & 0x0F);
                out.push((x.boost.clamp(0.0, 1.0) * 255.0).round() as u8);
                out.push(x.shifts);
            }
        }
        Packet::Player { token, info } => {
            out.push(PLAYER);
            put_u64(&mut out, *token);
            out.push(info.id);
            put_u32(&mut out, info.paint_seq);
            put_u32(&mut out, info.paint_rgb);
            out.push(info.paint_flags);
            put_str(&mut out, NAME_LEN, &info.name);
            put_str(&mut out, CAR_LEN, &info.car);
            put_str(&mut out, RIM_LEN, &info.rim);
            out.extend_from_slice(&info.kit);
            out.push(info.look_flags);
        }
        Packet::Leave { id, token } => {
            out.push(LEAVE);
            out.push(*id);
            put_u64(&mut out, *token);
        }
        Packet::Ping { token, client_ms } => {
            out.push(PING);
            put_u64(&mut out, *token);
            put_u32(&mut out, *client_ms);
        }
        Packet::Pong { client_ms, server_ms } => {
            out.push(PONG);
            put_u32(&mut out, *client_ms);
            put_u32(&mut out, *server_ms);
        }
        Packet::InfoReq { nonce } => {
            out.push(INFO_REQ);
            put_u32(&mut out, *nonce);
            out.resize(INFO_REQ_LEN, 0);
        }
        Packet::Info { nonce, info } => {
            out.push(INFO);
            put_u32(&mut out, *nonce);
            out.push(info.version);
            out.push(info.players);
            out.push(info.max_players);
            out.push(if info.password { INFO_PASSWORD } else { 0 });
            put_str(&mut out, SERVER_NAME_LEN, &info.name);
            put_str(&mut out, MAP_LEN, &info.map);
        }
        Packet::Heartbeat { cookie } => {
            out.push(HEARTBEAT);
            put_u64(&mut out, *cookie);
        }
        Packet::ListReq { page } => {
            out.push(LIST_REQ);
            out.extend_from_slice(&page.to_le_bytes());
            out.resize(LIST_REQ_LEN, 0);
        }
        Packet::List { page, pages, servers } => {
            out.push(LIST);
            out.extend_from_slice(&page.to_le_bytes());
            out.extend_from_slice(&pages.to_le_bytes());
            let servers = &servers[..servers.len().min(LIST_PAGE)];
            out.push(servers.len() as u8);
            for a in servers {
                match a {
                    SocketAddr::V4(v4) => {
                        out.push(4);
                        let mut ip = [0u8; 16];
                        ip[..4].copy_from_slice(&v4.ip().octets());
                        out.extend_from_slice(&ip);
                    }
                    SocketAddr::V6(v6) => {
                        out.push(6);
                        out.extend_from_slice(&v6.ip().octets());
                    }
                }
                out.extend_from_slice(&a.port().to_le_bytes());
            }
        }
    }
    out
}

/// The header's version byte of a packet with our magic (the server answers other versions with a REJECT).
pub fn peek_version(buf: &[u8]) -> Option<u8> {
    (buf.len() >= 10 && &buf[0..8] == MAGIC).then(|| buf[8])
}

pub fn decode(buf: &[u8]) -> Option<Packet> {
    if buf.len() < 10 || &buf[0..8] != MAGIC {
        return None;
    }
    // REJECT is readable from any version, so a client of another version still learns why it can't join.
    if buf[8] != VERSION && buf[9] != REJECT {
        return None;
    }
    let mut r = Reader { buf, i: 10 };
    let packet = match buf[9] {
        HELLO => Packet::Hello { name: r.str(NAME_LEN)?, map: r.str(MAP_LEN)?, car: r.str(CAR_LEN)?, cookie: r.u64()?, proof: r.u64()? },
        CHALLENGE => Packet::Challenge { cookie: r.u64()? },
        WELCOME => {
            let id = r.u8()?;
            let max_players = r.u8()?;
            if id == 0 || id as usize > MAX_PLAYERS {
                return None;
            }
            Packet::Welcome { id, max_players, token: r.u64()?, server_ms: r.u32()?, map: r.str(MAP_LEN)?, server_name: r.str(SERVER_NAME_LEN)?, motd: r.str(MOTD_LEN)? }
        }
        REJECT => {
            let reason = RejectReason::from_code(r.u8()?)?;
            let version = r.u8()?;
            let map = if reason == RejectReason::WrongMap { r.str(MAP_LEN)? } else { String::new() };
            Packet::Reject { reason, version, map }
        }
        FULL => Packet::Full,
        STATE => {
            let token = r.u64()?;
            let id = r.u8()?;
            let seq = r.u32()?;
            let sent_ms = r.u32()?;
            let flags = r.u8()?;
            let gear = r.u8()?;
            let rpm = r.f32()?;
            let steer = r.f32()?;
            let throttle = r.u8()? as f32 / 255.0;
            let brake = r.u8()? as f32 / 255.0;
            let wheel_drop_mm = [r.i8()?, r.i8()?, r.i8()?, r.i8()?];
            let position = [r.f32()?, r.f32()?, r.f32()?];
            let q = [r.i16()?, r.i16()?, r.i16()?, r.i16()?].map(|v| v as f32 / 32767.0);
            let n = q.iter().map(|v| v * v).sum::<f32>().sqrt();
            let rotation = if n > 0.5 { q.map(|v| v / n) } else { q };
            let velocity = [r.f32()?, r.f32()?, r.f32()?];
            let angular = [r.i16()?, r.i16()?, r.i16()?].map(|v| v as f32 / SPIN_SCALE);
            // Optional tail (module doc): absent from older senders / servers.
            let ext = (|| {
                let omega = [r.i16()?, r.i16()?, r.i16()?, r.i16()?].map(|v| v as f32 / OMEGA_SCALE);
                let slip = [r.i8()?, r.i8()?, r.i8()?, r.i8()?].map(|v| v as f32 / SLIP_SCALE);
                let slip_angle = [r.i8()?, r.i8()?, r.i8()?, r.i8()?].map(|v| v as f32 / SLIP_SCALE);
                Some(WheelExt { omega, slip, slip_angle, grounded: r.u8()? & 0x0F, boost: r.u8()? as f32 / 255.0, shifts: r.u8()? })
            })();
            let snap = Snapshot { id, seq, sent_ms, flags, gear, rpm, steer, throttle, brake, wheel_drop_mm, position, rotation, velocity, angular, ext };
            if !snap.sane() {
                return None;
            }
            Packet::State { token, snap }
        }
        PLAYER => {
            let token = r.u64()?;
            let id = r.u8()?;
            let mut info = PlayerInfo {
                id,
                paint_seq: r.u32()?,
                paint_rgb: r.u32()?,
                paint_flags: r.u8()?,
                name: r.str(NAME_LEN)?,
                car: r.str(CAR_LEN)?,
                rim: String::new(),
                kit: [KIT_STOCK; KIT_SLOTS],
                look_flags: 0,
            };
            // Optional looks tail (module doc): absent from older senders / servers = stock looks.
            match (|| Some((r.str(RIM_LEN)?, r.take::<KIT_SLOTS>()?, r.u8()?)))() {
                Some((rim, kit, flags)) => {
                    info.rim = rim;
                    info.kit = kit;
                    info.look_flags = flags & LOOK_CAGE;
                }
                None => info.stock_looks(),
            }
            Packet::Player { token, info }
        }
        LEAVE => {
            let id = r.u8()?;
            if id as usize > MAX_PLAYERS {
                return None;
            }
            Packet::Leave { id, token: r.u64()? }
        }
        PING => Packet::Ping { token: r.u64()?, client_ms: r.u32()? },
        PONG => Packet::Pong { client_ms: r.u32()?, server_ms: r.u32()? },
        INFO_REQ => {
            if buf.len() < INFO_REQ_LEN {
                return None;
            }
            Packet::InfoReq { nonce: r.u32()? }
        }
        INFO => {
            let nonce = r.u32()?;
            let version = r.u8()?;
            let players = r.u8()?;
            let max_players = r.u8()?;
            let flags = r.u8()?;
            Packet::Info { nonce, info: ServerInfo { version, players, max_players, password: flags & INFO_PASSWORD != 0, name: r.str(SERVER_NAME_LEN)?, map: r.str(MAP_LEN)? } }
        }
        HEARTBEAT => Packet::Heartbeat { cookie: r.u64()? },
        LIST_REQ => {
            if buf.len() < LIST_REQ_LEN {
                return None;
            }
            Packet::ListReq { page: r.u16()? }
        }
        LIST => {
            let page = r.u16()?;
            let pages = r.u16()?;
            let n = r.u8()? as usize;
            if n > LIST_PAGE {
                return None;
            }
            let mut servers = Vec::with_capacity(n);
            for _ in 0..n {
                let family = r.u8()?;
                let ip: [u8; 16] = r.take()?;
                let port = r.u16()?;
                servers.push(match family {
                    4 => SocketAddr::from(([ip[0], ip[1], ip[2], ip[3]], port)),
                    6 => SocketAddr::from((ip, port)),
                    _ => return None,
                });
            }
            Packet::List { page, pages, servers }
        }
        _ => return None,
    };
    Some(packet)
}

/// The password proof a HELLO carries: SipHash of the server's cookie keyed by the password. The password never goes on
/// the wire, and a proof is only good for that cookie (it expires within a minute).
#[allow(deprecated)]
pub fn password_proof(password: &str, cookie: u64) -> u64 {
    use std::hash::{Hash, Hasher, SipHasher};
    let key = |salt: u64| {
        let mut h = SipHasher::new_with_keys(0x4648_315f_4e45_5432, salt);
        password.hash(&mut h);
        h.finish()
    };
    let mut h = SipHasher::new_with_keys(key(1), key(2));
    cookie.hash(&mut h);
    // 0 means "no proof".
    h.finish().max(1)
}

struct Reader<'a> {
    buf: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let b = self.buf.get(self.i..self.i + N)?;
        self.i += N;
        b.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    fn i8(&mut self) -> Option<i8> {
        self.u8().map(|b| b as i8)
    }
    fn u16(&mut self) -> Option<u16> {
        self.take::<2>().map(u16::from_le_bytes)
    }
    fn i16(&mut self) -> Option<i16> {
        self.take::<2>().map(i16::from_le_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take::<4>().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take::<8>().map(u64::from_le_bytes)
    }
    fn f32(&mut self) -> Option<f32> {
        self.u32().map(f32::from_bits)
    }
    fn str(&mut self, n: usize) -> Option<String> {
        let b = self.buf.get(self.i..self.i + n)?;
        self.i += n;
        Some(cstring(b))
    }
}

fn put_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_f32(out: &mut Vec<u8>, v: f32) {
    put_u32(out, v.to_bits());
}
fn put_str(out: &mut Vec<u8>, len: usize, text: &str) {
    let start = out.len();
    out.resize(start + len, 0);
    let bytes = clean(text, len);
    out[start..start + bytes.len()].copy_from_slice(bytes.as_bytes());
}

/// `text` without control characters, cut to `len` bytes on a char boundary (exactly what goes on the wire).
pub fn clean(text: &str, len: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().filter(|c| !c.is_control()) {
        if out.len() + ch.len_utf8() > len {
            break;
        }
        out.push(ch);
    }
    out
}

fn cstring(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).chars().filter(|c| !c.is_control()).collect()
}

/// First resolved address, for the client and the server's `bind`.
pub fn resolve(text: &str) -> std::io::Result<SocketAddr> {
    let mut it = text.to_socket_addrs()?;
    it.next().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn snap() -> Snapshot {
        Snapshot {
            id: 3,
            seq: 9,
            sent_ms: 1234,
            flags: FLAG_BRAKE,
            gear: 4,
            rpm: 4500.0,
            steer: -0.2,
            throttle: 0.5,
            brake: 1.0,
            wheel_drop_mm: [1, -2, 3, -4],
            position: [10.0, 2.0, -30.0],
            rotation: [0.0, 0.38268343, 0.0, 0.9238795],
            velocity: [12.0, 0.0, -1.0],
            angular: [0.0, 0.4, -1.25],
            ext: None,
        }
    }

    #[test]
    fn state_ext_roundtrip_and_old_format() {
        let mut s = snap();
        s.ext = Some(WheelExt { omega: [40.0, -3.5, 120.25, 0.0], slip: [0.5, -1.25, 2.0, 0.0], slip_angle: [0.0, 0.75, -0.5, 1.0], grounded: 0b1011, boost: 0.5, shifts: 7 });
        let bytes = encode(&Packet::State { token: 1, snap: s.clone() });
        assert_eq!(bytes.len(), 81 + 19);
        let Packet::State { snap: back, .. } = decode(&bytes).unwrap() else { panic!("kind") };
        let (a, b) = (s.ext.unwrap(), back.ext.unwrap());
        for k in 0..4 {
            assert!((a.omega[k] - b.omega[k]).abs() < 0.04);
            assert!((a.slip[k] - b.slip[k]).abs() < 0.02 && (a.slip_angle[k] - b.slip_angle[k]).abs() < 0.02);
        }
        assert_eq!((b.grounded, b.shifts), (0b1011, 7));
        // An older peer's 81-byte STATE still decodes (no ext), and our tail is ignored by its decoder (prefix intact).
        let Packet::State { snap: old, .. } = decode(&bytes[..81]).unwrap() else { panic!("kind") };
        assert!(old.ext.is_none());
        assert_eq!(old.position, s.position);
    }

    #[test]
    fn player_looks_roundtrip_and_old_format() {
        let info = PlayerInfo { id: 4, name: "Ada".into(), car: "VW_Corrado_95".into(), paint_seq: 2, paint_rgb: 0, paint_flags: 0, rim: "BBS_RE".into(), kit: [1, KIT_STOCK, 2, KIT_STOCK, 3], look_flags: LOOK_CAGE };
        let bytes = encode(&Packet::Player { token: 9, info: info.clone() });
        let Packet::Player { info: back, .. } = decode(&bytes).unwrap() else { panic!("kind") };
        assert_eq!(back, info);
        // Without the tail (an older sender): stock looks.
        let Packet::Player { info: old, .. } = decode(&bytes[..bytes.len() - (RIM_LEN + KIT_SLOTS + 1)]).unwrap() else { panic!("kind") };
        assert_eq!((old.rim.as_str(), old.kit, old.look_flags, old.car.as_str()), ("", [KIT_STOCK; KIT_SLOTS], 0, "VW_Corrado_95"));
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("  Ada   Lovelace "), Some("Ada Lovelace".into()));
        assert_eq!(sanitize_name("<script>x"), Some("scriptx".into()));
        assert_eq!(sanitize_name("a"), None);
        assert_eq!(sanitize_name("\u{202e}\n"), None);
        assert_eq!(sanitize_name("ABCDEFGHIJKLMNOPQRSTUVWXYZ").map(|n| n.chars().count()), Some(NAME_MAX_CHARS));
        assert!(sanitize_name("ééééééééééééééé").is_some_and(|n| n.len() <= NAME_LEN));
    }

    #[test]
    fn state_roundtrip_and_size() {
        let s = snap();
        let bytes = encode(&Packet::State { token: 77, snap: s.clone() });
        assert_eq!(bytes.len(), 81);
        let Packet::State { token, snap: back } = decode(&bytes).unwrap() else { panic!("kind") };
        assert_eq!(token, 77);
        assert_eq!((back.id, back.seq, back.sent_ms, back.flags, back.gear), (s.id, s.seq, s.sent_ms, s.flags, s.gear));
        assert_eq!(back.wheel_drop_mm, s.wheel_drop_mm);
        assert!((back.rpm - s.rpm).abs() < 1e-3);
        assert!((back.throttle - 0.5).abs() < 0.01);
        assert!((back.position[2] + 30.0).abs() < 1e-4);
        for k in 0..4 {
            assert!((back.rotation[k] - s.rotation[k]).abs() < 1e-4);
        }
        for k in 0..3 {
            assert!((back.angular[k] - s.angular[k]).abs() < 0.003);
        }
    }

    #[test]
    fn player_roundtrip() {
        let info = PlayerInfo { id: 4, name: "Ada".into(), car: "../imported/fm4/cars/X".into(), paint_seq: 2, paint_rgb: 0xC01020, paint_flags: FLAG_CUSTOM_PAINT, rim: String::new(), kit: [KIT_STOCK; KIT_SLOTS], look_flags: 0 };
        let Packet::Player { token, info: back } = decode(&encode(&Packet::Player { token: 9, info: info.clone() })).unwrap() else { panic!("kind") };
        assert_eq!((token, back), (9, info));
    }

    #[test]
    fn hello_roundtrip_truncates_controls() {
        let pkt = Packet::Hello { name: "a\nb".into(), map: "colorado".into(), car: "x".into(), cookie: 5, proof: 6 };
        let Packet::Hello { name, cookie, proof, .. } = decode(&encode(&pkt)).unwrap() else { panic!("kind") };
        assert_eq!((name.as_str(), cookie, proof), ("ab", 5, 6));
    }

    #[test]
    fn replies_are_never_larger_than_their_requests() {
        let hello = encode(&Packet::Hello { name: String::new(), map: String::new(), car: String::new(), cookie: 0, proof: 0 }).len();
        let challenge = encode(&Packet::Challenge { cookie: 1 }).len();
        let welcome = encode(&Packet::Welcome { id: 1, max_players: 24, token: 1, server_ms: 0, map: "m".into(), server_name: "n".into(), motd: "x".into() }).len();
        let wrong_map = encode(&Packet::Reject { reason: RejectReason::WrongMap, version: VERSION, map: "m".into() }).len();
        let info_req = encode(&Packet::InfoReq { nonce: 1 }).len();
        let info = encode(&Packet::Info { nonce: 1, info: ServerInfo { name: "n".into(), map: "m".into(), players: 1, max_players: 24, password: false, version: VERSION } }).len();
        assert!(challenge <= hello && welcome <= hello && wrong_map <= hello, "{challenge} {welcome} {wrong_map} vs {hello}");
        assert_eq!(info_req, INFO_REQ_LEN);
        assert!(info <= info_req, "{info} vs {info_req}");
        // The version reject answers anything with our magic, so it must fit in a bare header-sized request.
        assert!(encode(&Packet::Reject { reason: RejectReason::Version, version: VERSION, map: String::new() }).len() <= 12);
        assert!(encode(&Packet::Full).len() <= hello);
        let ping = encode(&Packet::Ping { token: 1, client_ms: 1 }).len();
        assert!(encode(&Packet::Pong { client_ms: 1, server_ms: 1 }).len() <= ping);
    }

    #[test]
    fn list_roundtrip_and_size() {
        let mut servers: Vec<SocketAddr> = (0..LIST_PAGE as u8).map(|i| SocketAddr::from(([203, 0, 113, i], 7777))).collect();
        servers[1] = "[2001:db8::1]:7778".parse().unwrap();
        let bytes = encode(&Packet::List { page: 1, pages: 3, servers: servers.clone() });
        assert!(bytes.len() <= LIST_REQ_LEN, "{}", bytes.len());
        assert_eq!(decode(&bytes), Some(Packet::List { page: 1, pages: 3, servers }));
        assert_eq!(encode(&Packet::ListReq { page: 0 }).len(), LIST_REQ_LEN);
        assert!(encode(&Packet::Challenge { cookie: 1 }).len() <= encode(&Packet::Heartbeat { cookie: 0 }).len());
    }

    #[test]
    fn short_info_request_is_ignored() {
        let mut b = encode(&Packet::InfoReq { nonce: 1 });
        b.truncate(20);
        assert!(decode(&b).is_none());
    }

    #[test]
    fn reject_is_readable_from_another_version() {
        let mut b = encode(&Packet::Reject { reason: RejectReason::Version, version: 9, map: String::new() });
        b[8] = 9;
        assert!(matches!(decode(&b), Some(Packet::Reject { reason: RejectReason::Version, version: 9, .. })));
        let mut s = encode(&Packet::State { token: 1, snap: snap() });
        s[8] = 1;
        assert!(decode(&s).is_none());
    }

    #[test]
    fn proof_depends_on_password_and_cookie() {
        assert_eq!(password_proof("pw", 7), password_proof("pw", 7));
        assert_ne!(password_proof("pw", 7), password_proof("pw", 8));
        assert_ne!(password_proof("pw", 7), password_proof("pX", 7));
        assert_ne!(password_proof("pw", 7), 0);
    }

    #[test]
    fn rejects_garbage_and_teleports() {
        assert!(decode(b"nope").is_none());
        assert!(decode(&encode(&Packet::Welcome { id: 0, max_players: 1, token: 1, server_ms: 0, map: String::new(), server_name: String::new(), motd: String::new() })).is_none());
        let mut s = snap();
        s.position = [100_000.0, 0.0, 0.0];
        assert!(decode(&encode(&Packet::State { token: 1, snap: s.clone() })).is_none());
        s.position = [0.0, 0.0, 0.0];
        s.velocity = [0.0, 0.0, 500.0];
        assert!(decode(&encode(&Packet::State { token: 1, snap: s })).is_none());
        // Truncated packets of every kind decode to nothing, never panic.
        for kind in 0..=20u8 {
            let mut b = MAGIC.to_vec();
            b.push(VERSION);
            b.push(kind);
            for len in 0..40 {
                let _ = decode(&b);
                b.push(len as u8);
            }
        }
    }
}
