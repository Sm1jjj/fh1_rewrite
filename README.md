# FH1 Rewrite

An unofficial, clean reimplementation of **Forza Horizon** (Xbox 360, 2012) in Rust + Bevy. It is
not affiliated with Microsoft, Turn 10 or Playground Games.

**No game assets, keys or decompiled code are included.** You need your own copy of the game disc;
the setup tool converts it locally into a git-ignored `data/` folder.

## Setup

1. Rust (stable, MSVC toolchain on Windows).
2. Your own Forza Horizon disc image (`.iso`) or an extracted disc folder.
3. The Xbox 360 retail XEX key (32 hex digits), needed to read the shaders inside `default.xex`. It is not
   shipped here: put it in `data/xex_key.txt` or set `FH1_XEX_KEY`.

```
cargo run --release -p fh1setup -- "path/to/Forza Horizon.iso"   # or an extracted disc folder
cargo run --release -p fh1-engine
```

`fh1setup` writes `data/installations/<id>/assets/private/`, with one pipeline hash per asset group, so later
updates re-convert only what changed (`--force`, `--only <group>,...`).

### Optional imports (off by default)

If you also own Forza Horizon 2 or Forza Motorsport 4 (Xbox 360), their cars and maps can be imported into the
same install. These importers are behind cargo features:

```
cargo run --release -p fh1setup --features fh2 -- import-fh2 "path/to/FH2.iso"
cargo run --release -p fh1setup --features fm4 -- import-fm4 "path/to/FM4 disc folder"
```

## Controls

| | Keyboard | Gamepad |
|---|---|---|
| Throttle / brake | W / S | RT / LT |
| Steer | A / D | Left stick |
| Handbrake | Space | A |
| Shift up / down (manual) | E / Q | B / X |
| Clutch | Left Shift | LB |
| Rewind (hold) | X | Back |
| Reset car | R | Y |
| Camera | C | RB |
| Radio station | , / . | D-pad left / right |
| Pause | Esc | Start |

## Layout

| Crate | Purpose |
|---|---|
| `fh1-formats` | Parsers: Forza zips (XMemCompress LZX, headerless world archive), `.xds`/`.bix` textures, `.carbin` models, `.rmb.bin` scenery, PVS zones |
| `fh1setup` | Disc → converted assets (glTF, DDS, JSON) with per-group hashing |
| `fh1-engine` | The game: vehicle simulation, cameras, world streaming, races, AI, traffic, HUD and menus |
| `fh1-world` | Track collision and surfaces |
| `fh1-shaders` / `fh1-render` | Xenos shader microcode → WGSL, and the renderer that runs the game's own shaders |
| `fh1-remaster` | The default PBR "remaster" renderer |
| `fh1-audio` / `fh1-radio` | Engine audio from the game's FMOD banks; the in-game radio |
| `fh1-ui` | Game UI data (string tables, fonts, HUD scenes) |
| `fh1-net` | Multiplayer protocol and server |

Research notes are in `docs/`. Vendored crates keep their own licenses: `vendor/lzxd` (patched for XMemCompress
streams, see `vendor/lzxd/FH1_PATCHES.md`) and `vendor/bevy_solarik`.

## License

The code in this repository is dual-licensed under MIT or Apache-2.0, at your option. Forza Horizon and all game
content belong to their owners; this project contains none of it.
