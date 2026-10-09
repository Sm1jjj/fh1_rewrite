//! Clients and an in-process relay on 127.0.0.1, real sockets and the full v2 handshake. The game is not involved.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use fh1_net::{encode, Client, Config, Event, InfoQuery, PlayerInfo, Relay, RejectReason, Snapshot, KIT_SLOTS, KIT_STOCK};

fn snap() -> Snapshot {
    Snapshot {
        id: 0,
        seq: 0,
        sent_ms: 0,
        flags: 0,
        gear: 3,
        rpm: 3000.0,
        steer: 0.1,
        throttle: 0.4,
        brake: 0.0,
        wheel_drop_mm: [0; 4],
        position: [5.0, 1.0, -8.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        velocity: [10.0, 0.0, 0.0],
        angular: [0.0; 3],
        ext: None,
    }
}

fn me(car: &str) -> PlayerInfo {
    PlayerInfo { id: 0, name: String::new(), car: car.into(), paint_seq: 2, paint_rgb: 0, paint_flags: 0, rim: String::new(), kit: [KIT_STOCK; KIT_SLOTS], look_flags: 0 }
}

fn pump(sock: &UdpSocket, relay: &mut Relay, started: Instant) {
    let mut buf = [0u8; 1500];
    let now = started.elapsed().as_millis() as u64;
    while let Ok((n, from)) = sock.recv_from(&mut buf) {
        let h = relay.handle_datagram(from, &buf[..n], now);
        for (to, packet) in h.packets {
            sock.send_to(&encode(&packet), to).unwrap();
        }
    }
}

fn server(cfg: Config) -> (UdpSocket, String, Relay) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    let addr = sock.local_addr().unwrap().to_string();
    (sock, addr, Relay::new(cfg))
}

#[test]
fn two_clients_join_and_see_each_other() {
    let (sock, addr, mut relay) = server(Config::default());
    let mut a = Client::connect(&addr, "Ada", "", "colorado").unwrap();
    let mut b = Client::connect(&addr, "Bea", "", "colorado").unwrap();
    let started = Instant::now();
    let (mut saw_state, mut saw_player, mut welcomed) = (false, false, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !(saw_state && saw_player) {
        a.tick(Some(&snap()), &me("VW_Corrado_95"));
        b.tick(Some(&snap()), &me("AUD_R8GT_11"));
        pump(&sock, &mut relay, started);
        for ev in b.poll() {
            match ev {
                Event::Welcome { .. } => welcomed += 1,
                Event::State(s) if (s.position[0] - 5.0).abs() < 1e-3 && s.id == a.id().unwrap_or(0) => saw_state = true,
                Event::Player(p) if p.name == "Ada" && p.car == "VW_Corrado_95" => saw_player = true,
                _ => {}
            }
        }
        welcomed += a.poll().iter().filter(|e| matches!(e, Event::Welcome { .. })).count();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(welcomed, 2, "both clients should be welcomed once");
    assert!(saw_state, "Bea never got Ada's snapshot");
    assert!(saw_player, "Bea never learnt Ada's car");
    assert!(a.rtt_ms().is_some(), "no ping answered");
}

#[test]
fn password_and_map_are_checked() {
    let (sock, addr, mut relay) = server(Config { password: "secret".into(), ..Config::default() });
    let mut good = Client::connect(&addr, "Ada", "secret", "colorado").unwrap();
    let mut bad = Client::connect(&addr, "Bea", "nope", "colorado").unwrap();
    let mut lost = Client::connect(&addr, "Cy", "secret", "flat").unwrap();
    let started = Instant::now();
    let (mut ok, mut bad_pw, mut wrong_map) = (false, false, false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !(ok && bad_pw && wrong_map) {
        for c in [&mut good, &mut bad, &mut lost] {
            c.tick(Some(&snap()), &me("VW_Corrado_95"));
        }
        pump(&sock, &mut relay, started);
        ok |= good.poll().iter().any(|e| matches!(e, Event::Welcome { .. }));
        bad_pw |= bad.poll().iter().any(|e| matches!(e, Event::Rejected { reason: RejectReason::BadPassword, .. }));
        wrong_map |= lost.poll().iter().any(|e| matches!(e, Event::Rejected { reason: RejectReason::WrongMap, map, .. } if map == "colorado"));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ok && bad_pw && wrong_map, "ok {ok} bad password {bad_pw} wrong map {wrong_map}");
}

#[test]
fn browser_info_query() {
    let (sock, addr, mut relay) = server(Config { name: "Test server".into(), max_players: 24, ..Config::default() });
    let mut q = InfoQuery::new().unwrap();
    q.send(addr.parse().unwrap(), 42);
    let started = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        pump(&sock, &mut relay, started);
        if let Some((_, info, _)) = q.poll().into_iter().next() {
            assert_eq!((info.name.as_str(), info.map.as_str(), info.max_players, info.players), ("Test server", "colorado", 24, 0));
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no INFO answer");
}

#[test]
fn browser_finds_a_server_through_the_registry() {
    use fh1_net::{decode, Heartbeat, Registry, ServerBrowser};
    let reg_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    reg_sock.set_nonblocking(true).unwrap();
    let reg_addr = reg_sock.local_addr().unwrap();
    let (game_sock, game_addr, mut relay) = server(Config { name: "Listed".into(), ..Config::default() });
    let mut registry = Registry::new();
    let mut hb = Heartbeat::new(reg_addr);
    let mut browser = ServerBrowser::new(Some(&reg_addr.to_string())).unwrap();
    let started = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 1500];
    let mut refreshed = false;
    while Instant::now() < deadline {
        let now = started.elapsed().as_millis() as u64;
        // The game server: heartbeat out, registry challenges in, INFO answers.
        if let Some(p) = hb.tick(now) {
            game_sock.send_to(&encode(&p), reg_addr).unwrap();
        }
        while let Ok((n, from)) = game_sock.recv_from(&mut buf) {
            if from == reg_addr {
                if let Some(reply) = decode(&buf[..n]).and_then(|p| hb.handle(&p)) {
                    game_sock.send_to(&encode(&reply), reg_addr).unwrap();
                }
                continue;
            }
            for (to, p) in relay.handle_datagram(from, &buf[..n], now).packets {
                game_sock.send_to(&encode(&p), to).unwrap();
            }
        }
        // The registry.
        while let Ok((n, from)) = reg_sock.recv_from(&mut buf) {
            if let Some(p) = decode(&buf[..n]) {
                for (to, reply) in registry.handle(from, p, now).0 {
                    reg_sock.send_to(&encode(&reply), to).unwrap();
                }
            }
        }
        // The browser asks once the server is listed.
        if !refreshed && !registry.servers().is_empty() {
            browser.refresh();
            refreshed = true;
        }
        browser.poll();
        if let Some(row) = browser.rows().iter().find(|r| r.info.is_some()) {
            let info = row.info.as_ref().unwrap();
            assert_eq!(row.addr.to_string(), game_addr);
            assert_eq!((info.name.as_str(), info.map.as_str()), ("Listed", "colorado"));
            assert!(row.ping_ms.is_some());
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the browser never saw the listed server (listed: {:?})", registry.servers());
}
