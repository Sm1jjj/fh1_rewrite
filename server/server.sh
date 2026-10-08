#!/usr/bin/env bash
# fh1-server: build and run on Linux (target: Ubuntu 22.04), or pack the sources for the server box.
#
# On the Windows dev machine (Git Bash), in the repo:
#   server/server.sh pack                 # -> dist/fh1-server.tar.gz (fh1-net crate + these files, ~50 KB)
#
# On the server (Ubuntu 22.04):
#   tar xzf fh1-server.tar.gz && cd fh1-server
#   ./server.sh setup                     # once: build tools + Rust (rustup, current user), then builds
#   cp server.cfg.example colorado.cfg    # edit name / map / password / registry
#   ./server.sh run colorado.cfg          # a game server (one map)
#   ./server.sh registry [port]           # the server list (UDP 7700 by default)
#   ./server.sh build                     # rebuild after unpacking a newer pack
#
# Works in both layouts: the repo (server/ next to crates/fh1-net) and the unpacked pack (fh1-net/ next to this script).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
if [ -f "$here/fh1-net/Cargo.toml" ]; then
  crate="$here/fh1-net"
  bin="$crate/target/release/fh1-server"
  build() { cargo build --release --manifest-path "$crate/Cargo.toml" --bin fh1-server; }
else
  root="$(cd "$here/.." && pwd)"
  crate="$root/crates/fh1-net"
  bin="$root/target/release/fh1-server"
  build() { (cd "$root" && cargo build --release -p fh1-net --bin fh1-server); }
fi

case "${1:-}" in
  setup)
    if command -v apt-get >/dev/null 2>&1; then
      sudo apt-get update
      sudo apt-get install -y build-essential curl
    fi
    if ! command -v cargo >/dev/null 2>&1; then
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
      # shellcheck disable=SC1091
      source "$HOME/.cargo/env"
    fi
    build
    echo "built $bin"
    ;;
  build)
    build
    echo "built $bin"
    ;;
  run)
    cfg="${2:-server.cfg}"
    [ -f "$cfg" ] || { echo "no config $cfg (copy server.cfg.example)"; exit 1; }
    exec "$bin" --config "$cfg" "${@:3}"
    ;;
  registry)
    exec "$bin" --registry --bind "0.0.0.0:${2:-7700}"
    ;;
  pack)
    [ -d "$here/../crates/fh1-net" ] || { echo "pack runs from the repo"; exit 1; }
    out="$here/../dist"
    tmp="$(mktemp -d)"
    mkdir -p "$tmp/fh1-server/fh1-net" "$out"
    cp -r "$crate/Cargo.toml" "$crate/src" "$crate/tests" "$tmp/fh1-server/fh1-net/"
    cp "$here/server.sh" "$here/server.cfg.example" "$here"/*.service "$here/README.md" "$tmp/fh1-server/"
    tar czf "$out/fh1-server.tar.gz" -C "$tmp" fh1-server
    rm -rf "$tmp"
    echo "wrote $(cd "$out" && pwd)/fh1-server.tar.gz"
    ;;
  *)
    sed -n '2,16p' "$0"
    exit 1
    ;;
esac
