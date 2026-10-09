# FH1 multiplayer server

`fh1-server` hosts one map for up to 24 players (protocol cap 32). It relays each player's car to the others; it has
no world, no physics, no GPU and no game files. Players need their own setup from their own disc.

## Ubuntu 22.04

```sh
tar xzf fh1-server.tar.gz && cd fh1-server     # the pack made by `server/server.sh pack` on the dev machine
./server.sh setup                              # once: build-essential + curl, Rust via rustup, then builds
cp server.cfg.example colorado.cfg             # name, map, max_players, password, motd, registry
./server.sh run colorado.cfg                   # foreground (console: list | kick <id> | ban <id> | quit)
./server.sh registry                           # the server list, UDP 7700 (one per community)
```

Firewall: open the game port (UDP 7777 by default, one per server) and, on the registry host, UDP 7700:
`sudo ufw allow 7777/udp && sudo ufw allow 7700/udp`.

As services (copy the pack to /opt/fh1-server, run `./server.sh setup` there, create a `fh1` user):

```sh
sudo cp fh1-server@.service fh1-registry.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now fh1-registry
sudo systemctl enable --now fh1-server@colorado      # /opt/fh1-server/colorado.cfg
journalctl -u fh1-server@colorado -f
```

More maps = more config files and instances (each with its own `bind` port), e.g. `fh1-server@anthem` with
`map = fh2/anthem` and `bind = 0.0.0.0:7778`.

### Updating (2026-10-09: looks / tyre FX ext)

Same protocol version (2): old and new clients still join each other's servers. Puppets show rims, body kits and
tyre smoke only through an updated server (an old one relays the packets without the new fields). On the dev machine
`server/server.sh pack`, copy `dist/fh1-server.tar.gz` to the box, unpack over the old folder, `./server.sh setup`
(rebuilds), then `sudo systemctl restart fh1-server@<name>`.

## Windows

`server.bat` (repo root): uses `server\server.cfg` when present; `server.bat --registry` runs the list.

## Players

The game's ONLINE menu lists every public server on the registry (name, map, players, ping, lock = password) and
has Direct connect (`host:port`). Without the menu: `set FH1_SERVER=host:7777` (plus `FH1_SERVER_PASSWORD`,
`FH1_NAME`) before `launch.bat`. Details, security model and protocol: docs/MULTIPLAYER.md.
