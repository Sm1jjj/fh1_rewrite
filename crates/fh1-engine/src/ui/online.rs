//! Main menu ONLINE: the server browser (docs/MULTIPLAYER.md). Lists every public server on the registry (name, map,
//! players, ping, password lock) plus Direct connect (`host:port`, kept as favourites). Joining checks the server's map
//! is installed, asks for the password of a locked server, then hands the address to net.rs ([`crate::net::NetConnect`])
//! and starts that world like HORIZON does.
//!
//! `data/online.json` (next to settings.json): `{"registry": "host:7700", "favourites": ["host:7777"]}`.
//! `FH1_REGISTRY=host:port` overrides the registry.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;
use fh1_net::{ServerBrowser, VERSION};
use serde::{Deserialize, Serialize};

use crate::ui::browser::{BrowserInput, BrowserRow};

#[derive(Serialize, Deserialize, Default, Clone)]
struct OnlineFile {
    #[serde(default)]
    registry: Option<String>,
    #[serde(default)]
    favourites: Vec<String>,
}

enum Edit {
    Direct(String),
    Password { addr: SocketAddr, map: String, text: String },
}

/// What the browser wants the main menu to do.
pub enum OnlinePick {
    Browsing { changed: bool },
    Leave,
    /// Connect to `addr` (with `password`) and load `map`.
    Join { addr: String, password: String, map: String },
}

pub struct OnlineScreen {
    browser: Option<ServerBrowser>,
    file: OnlineFile,
    path: PathBuf,
    registry: Option<String>,
    cursor: usize,
    edit: Option<Edit>,
    message: Option<String>,
    /// (rows, answered) last drawn, to redraw when answers arrive.
    seen: (usize, usize),
}

/// Rows above the server list.
const FIXED_ROWS: usize = 2;

impl OnlineScreen {
    /// `settings_path` = data/settings.json (online.json sits next to it).
    pub fn new(settings_path: &Path) -> Self {
        let path = settings_path.with_file_name("online.json");
        let file: OnlineFile = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let registry = std::env::var("FH1_REGISTRY").ok().filter(|s| !s.trim().is_empty()).or_else(|| file.registry.clone());
        let mut message = None;
        let browser = match ServerBrowser::new(registry.as_deref()) {
            Ok(mut b) => {
                for f in &file.favourites {
                    let _ = b.add_direct(f);
                }
                b.refresh();
                if let Some(e) = &b.registry_error {
                    message = Some(format!("server list: {e}"));
                }
                Some(b)
            }
            Err(e) => {
                message = Some(format!("network: {e}"));
                None
            }
        };
        Self { browser, file, path, registry, cursor: FIXED_ROWS, edit: None, message, seen: (0, 0) }
    }

    fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(&self.file) {
            crate::perf::writer::replace(self.path.clone(), b);
        }
    }

    /// Reads answers; true when the list changed (redraw).
    pub fn poll(&mut self) -> bool {
        let Some(b) = self.browser.as_mut() else { return false };
        b.poll();
        let now = (b.rows().len(), b.rows().iter().filter(|r| r.info.is_some() || r.unreachable()).count());
        let changed = now != self.seen;
        self.seen = now;
        changed
    }

    /// Servers in display order: answered first by ping, then pending, unreachable last.
    fn servers(&self) -> Vec<&fh1_net::BrowserRow> {
        let mut v: Vec<&fh1_net::BrowserRow> = self.browser.as_ref().map(|b| b.rows().iter().collect()).unwrap_or_default();
        v.sort_by_key(|r| (r.info.is_none(), r.unreachable(), r.ping_ms.unwrap_or(u32::MAX)));
        v
    }

    pub fn title(&self) -> String {
        match &self.edit {
            Some(Edit::Password { .. }) => "ONLINE  ·  password".into(),
            _ => "ONLINE  ·  choose a server".into(),
        }
    }

    pub fn hint(&self) -> String {
        match &self.edit {
            Some(_) => "type      Enter  OK      Esc  cancel".into(),
            None => "Enter / A  join      Backspace / B  back".into(),
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// `maps` = installed worlds (for map names and the installed check).
    pub fn rows(&self, maps: &[(String, Vec<(String, String)>)]) -> Vec<BrowserRow> {
        let map_name = |id: &str| maps.iter().flat_map(|g| g.1.iter()).find(|m| m.0 == id).map(|m| m.1.trim().to_owned());
        if let Some(Edit::Password { text, .. }) = &self.edit {
            return vec![BrowserRow { label: "Password".into(), value: Some(format!("{}_", "*".repeat(text.chars().count()))) }];
        }
        let direct = match &self.edit {
            Some(Edit::Direct(t)) => format!("{t}_"),
            _ => "host:port".into(),
        };
        let status = self.message.clone().unwrap_or_else(|| match &self.registry {
            Some(r) => format!("server list {r}"),
            None => "no server list set (data/online.json \"registry\"): direct connect only".into(),
        });
        let mut rows = vec![BrowserRow { label: "Direct connect".into(), value: Some(direct) }, BrowserRow { label: "Refresh".into(), value: Some(status) }];
        for r in self.servers() {
            let row = match &r.info {
                Some(i) => {
                    let map = map_name(&i.map).unwrap_or_else(|| format!("{} (not installed)", i.map));
                    let mut v = format!("{map}  ·  {}/{}  ·  {} ms", i.players, i.max_players, r.ping_ms.unwrap_or(0));
                    if i.password {
                        v += "  ·  password";
                    }
                    if i.version != VERSION {
                        v += &format!("  ·  protocol {}", i.version);
                    }
                    BrowserRow { label: if i.name.is_empty() { r.addr.to_string() } else { i.name.clone() }, value: Some(v) }
                }
                None if r.unreachable() => BrowserRow { label: r.addr.to_string(), value: Some("no answer".into()) },
                None => BrowserRow { label: r.addr.to_string(), value: Some("asking…".into()) },
            };
            rows.push(row);
        }
        rows
    }

    pub fn step(&mut self, input: &BrowserInput, typed: &[KeyboardInput], keys: &ButtonInput<KeyCode>, maps: &[(String, Vec<(String, String)>)]) -> OnlinePick {
        if self.edit.is_some() {
            return self.step_edit(typed, keys);
        }
        let n = FIXED_ROWS + self.browser.as_ref().map_or(0, |b| b.rows().len());
        if input.vertical != 0 {
            self.cursor = (self.cursor as i32 + input.vertical).rem_euclid(n as i32) as usize;
            return OnlinePick::Browsing { changed: true };
        }
        if input.back {
            return OnlinePick::Leave;
        }
        if !input.confirm {
            return OnlinePick::Browsing { changed: false };
        }
        match self.cursor {
            0 => {
                self.edit = Some(Edit::Direct(String::new()));
                self.message = None;
            }
            1 => {
                if let Some(b) = self.browser.as_mut() {
                    b.refresh();
                }
                self.message = None;
            }
            k => {
                let Some(r) = self.servers().get(k - FIXED_ROWS).map(|r| (*r).clone()) else { return OnlinePick::Browsing { changed: false } };
                let Some(info) = &r.info else {
                    self.message = Some(format!("{} hasn't answered", r.addr));
                    return OnlinePick::Browsing { changed: true };
                };
                if info.version != VERSION {
                    self.message = Some(format!("{} runs protocol {}, this game {VERSION}", info.name, info.version));
                } else if !maps.iter().flat_map(|g| g.1.iter()).any(|m| m.0 == info.map) {
                    self.message = Some(format!("{} runs {}, which isn't installed here", info.name, info.map));
                } else if info.players >= info.max_players {
                    self.message = Some(format!("{} is full", info.name));
                } else if info.password {
                    self.edit = Some(Edit::Password { addr: r.addr, map: info.map.clone(), text: String::new() });
                } else {
                    return OnlinePick::Join { addr: r.addr.to_string(), password: String::new(), map: info.map.clone() };
                }
            }
        }
        OnlinePick::Browsing { changed: true }
    }

    fn step_edit(&mut self, typed: &[KeyboardInput], keys: &ButtonInput<KeyCode>) -> OnlinePick {
        let Some(edit) = self.edit.as_mut() else { return OnlinePick::Browsing { changed: false } };
        let text = match edit {
            Edit::Direct(t) => t,
            Edit::Password { text, .. } => text,
        };
        let mut changed = false;
        for k in typed.iter().filter(|k| k.state == ButtonState::Pressed) {
            match &k.logical_key {
                Key::Backspace => {
                    text.pop();
                    changed = true;
                }
                Key::Enter | Key::Escape => {}
                _ => {
                    if let Some(s) = &k.text {
                        for c in s.chars().filter(|c| !c.is_control()) {
                            if text.len() < 96 {
                                text.push(c);
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        if keys.just_pressed(KeyCode::Escape) {
            self.edit = None;
            return OnlinePick::Browsing { changed: true };
        }
        if !(keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter)) {
            return OnlinePick::Browsing { changed };
        }
        match self.edit.take() {
            Some(Edit::Direct(t)) => {
                let t = t.trim().to_owned();
                let t = if t.contains(':') { t } else { format!("{t}:7777") };
                match self.browser.as_mut().map(|b| b.add_direct(&t)) {
                    Some(Ok(_)) => {
                        if !self.file.favourites.contains(&t) {
                            self.file.favourites.push(t.clone());
                            self.save();
                        }
                        self.message = Some(format!("asking {t}…"));
                    }
                    Some(Err(e)) => self.message = Some(format!("{t}: {e}")),
                    None => {}
                }
                OnlinePick::Browsing { changed: true }
            }
            Some(Edit::Password { addr, map, text }) => OnlinePick::Join { addr: addr.to_string(), password: text, map },
            None => OnlinePick::Browsing { changed: true },
        }
    }
}
