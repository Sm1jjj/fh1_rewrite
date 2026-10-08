//! The server list (`fh1-server --registry`): game servers heartbeat to it, the in-game browser asks it for the list
//! and then asks each server for its INFO directly (players, ping).
//!
//! A HEARTBEAT comes from the game server's own socket, so the address registered is the one players connect to. It is
//! verified like a join: the registry answers with a CHALLENGE cookie bound to that address, and only a heartbeat
//! carrying it registers, so nobody can list someone else's address. Entries expire [`ENTRY_MS`] after the last
//! heartbeat. Replies are never larger than their requests (CHALLENGE <= HEARTBEAT, a LIST page <= the padded LIST_REQ).

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::SocketAddr;

use crate::proto::{Packet, LIST_PAGE};

/// Game servers heartbeat this often.
pub const HEARTBEAT_MS: u64 = 30_000;
/// A server not heard from for this long is dropped from the list.
pub const ENTRY_MS: u64 = 3 * HEARTBEAT_MS + 5_000;
const COOKIE_BUCKET_MS: u64 = 60_000;
/// Listed servers in total, and per IP address.
const MAX_SERVERS: usize = 2_000;
const PER_IP: usize = 16;
/// Unauthenticated replies per second, registry-wide.
const UNAUTH_PER_S: u32 = 400;

pub struct Registry {
    secret: RandomState,
    servers: HashMap<SocketAddr, u64>,
    unauth: (u64, u32),
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self { secret: RandomState::new(), servers: HashMap::new(), unauth: (0, 0) }
    }

    pub fn servers(&self) -> Vec<SocketAddr> {
        let mut v: Vec<SocketAddr> = self.servers.keys().copied().collect();
        v.sort();
        v
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
        self.secret.hash_one((from.ip(), from.port(), bucket, 0x5Eu8)).max(1)
    }

    /// Replies to send. `registered` is set when a new server joined the list.
    pub fn handle(&mut self, from: SocketAddr, packet: Packet, now_ms: u64) -> (Vec<(SocketAddr, Packet)>, Option<SocketAddr>) {
        match packet {
            Packet::Heartbeat { cookie } => {
                let b = now_ms / COOKIE_BUCKET_MS;
                let ok = cookie != 0 && (cookie == self.cookie(from, b) || (b > 0 && cookie == self.cookie(from, b - 1)));
                if !ok {
                    if !self.unauth_ok(now_ms) {
                        return (Vec::new(), None);
                    }
                    return (vec![(from, Packet::Challenge { cookie: self.cookie(from, b) })], None);
                }
                if let Some(t) = self.servers.get_mut(&from) {
                    *t = now_ms;
                    return (Vec::new(), None);
                }
                let same_ip = self.servers.keys().filter(|a| a.ip() == from.ip()).count();
                if self.servers.len() >= MAX_SERVERS || same_ip >= PER_IP {
                    return (Vec::new(), None);
                }
                self.servers.insert(from, now_ms);
                (Vec::new(), Some(from))
            }
            Packet::ListReq { page } => {
                if !self.unauth_ok(now_ms) {
                    return (Vec::new(), None);
                }
                let all = self.servers();
                let pages = all.len().div_ceil(LIST_PAGE).max(1) as u16;
                let start = page as usize * LIST_PAGE;
                let servers = all.get(start..).map(|s| s.iter().take(LIST_PAGE).copied().collect()).unwrap_or_default();
                (vec![(from, Packet::List { page, pages, servers })], None)
            }
            _ => (Vec::new(), None),
        }
    }

    /// Drops servers that stopped heartbeating; returns them.
    pub fn expire(&mut self, now_ms: u64) -> Vec<SocketAddr> {
        let gone: Vec<SocketAddr> = self.servers.iter().filter(|(_, t)| now_ms.saturating_sub(**t) >= ENTRY_MS).map(|(a, _)| *a).collect();
        for a in &gone {
            self.servers.remove(a);
        }
        gone
    }
}

/// A game server's side of the heartbeat: call [`Self::tick`] every loop and hand it registry packets.
pub struct Heartbeat {
    pub registry: SocketAddr,
    cookie: u64,
    last: Option<u64>,
}

impl Heartbeat {
    pub fn new(registry: SocketAddr) -> Self {
        Self { registry, cookie: 0, last: None }
    }

    /// The HEARTBEAT to send now, if due.
    pub fn tick(&mut self, now_ms: u64) -> Option<Packet> {
        if self.last.is_some_and(|t| now_ms.saturating_sub(t) < HEARTBEAT_MS) {
            return None;
        }
        self.last = Some(now_ms);
        Some(Packet::Heartbeat { cookie: self.cookie })
    }

    /// A packet from the registry: a CHALLENGE gives the cookie, answered at once.
    pub fn handle(&mut self, packet: &Packet) -> Option<Packet> {
        if let Packet::Challenge { cookie } = packet {
            self.cookie = *cookie;
            return Some(Packet::Heartbeat { cookie: *cookie });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([198, 51, 100, n], 7777))
    }

    #[test]
    fn a_server_registers_only_with_the_cookie_and_expires() {
        let mut r = Registry::new();
        let mut hb = Heartbeat::new(addr(200));
        let first = hb.tick(0).unwrap();
        let (replies, reg) = r.handle(addr(1), first, 0);
        assert!(reg.is_none() && r.servers().is_empty());
        let again = hb.handle(&replies[0].1).unwrap();
        // Someone else replaying that cookie from another address gets nowhere.
        assert!(r.handle(addr(2), again.clone(), 0).1.is_none());
        assert_eq!(r.handle(addr(1), again, 0).1, Some(addr(1)));
        assert_eq!(r.servers(), vec![addr(1)]);
        assert!(hb.tick(1_000).is_none());
        assert!(hb.tick(HEARTBEAT_MS).is_some());
        assert_eq!(r.expire(ENTRY_MS), vec![addr(1)]);
    }

    #[test]
    fn list_pages() {
        let mut r = Registry::new();
        for i in 0..30u8 {
            let Packet::Challenge { cookie } = r.handle(addr(i), Packet::Heartbeat { cookie: 0 }, 0).0[0].1 else { panic!() };
            r.handle(addr(i), Packet::Heartbeat { cookie }, 0);
        }
        let (p0, _) = r.handle(addr(250), Packet::ListReq { page: 0 }, 0);
        let (p1, _) = r.handle(addr(250), Packet::ListReq { page: 1 }, 0);
        let (Packet::List { pages, servers: a, .. }, Packet::List { servers: b, .. }) = (&p0[0].1, &p1[0].1) else { panic!() };
        assert_eq!((*pages, a.len() + b.len()), (2, 30));
    }
}
