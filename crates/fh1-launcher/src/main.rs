//! FH1 Rewrite launcher: the exe release users open. First run = a short install wizard (pick the Forza Horizon
//! disc: ISO, a .zip holding it, or the folder it's in; optionally Forza Horizon 2 / Forza Motorsport 4 discs), which
//! runs `fh1setup` (and its `import-fh2` / `import-fm4`) as child processes; afterwards it is the Play button.
//!
//! Release layout (tools/release.ps1):
//! ```text
//! <root>/FH1 Rewrite.exe        this launcher
//! <root>/bin/fh1-engine.exe     the game
//! <root>/bin/fh1setup.exe       asset converter (built with --features fh2,fm4)
//! <root>/bin/extract-xiso.exe   ISO extraction (fh1setup finds it next to itself)
//! <root>/bin/ffmpeg.exe         XMA audio decoding during setup, FMV playback in the game (FH1_FFMPEG)
//! <root>/data/                  converted assets (installation.json, installations/<id>/...), launcher.json, logs/
//! ```
//! Only FH1 is required. Games that are not imported stay locked (greyed) in the game's menus.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, RichText};

mod style;
mod update;
use serde::{Deserialize, Serialize};

/// `.exe` on Windows, nothing on Linux.
const EXE: &str = std::env::consts::EXE_SUFFIX;

/// No console window for child processes (Windows); nothing to do elsewhere.
pub(crate) fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Open a folder or web page with the desktop's default handler.
pub(crate) fn open_external(target: impl AsRef<std::ffi::OsStr>) {
    let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = Command::new(opener).arg(target).spawn();
}
/// The converted-data revision: bump ONLY when a release's setup output changes (a new fh1setup group version), so
/// existing installs are offered an update. Game-only releases keep it, so nobody re-converts their disc for nothing.
const RELEASE: &str = "0.1.3";
/// Shown in the title bar.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const ACCENT: Color32 = style::MAGENTA;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FH1 Rewrite")
            .with_inner_size([1100.0, 640.0])
            .with_min_inner_size([860.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("FH1 Rewrite", options, Box::new(|cc| Ok(Box::new(Launcher::new(&cc.egui_ctx)))))
}

/// Where everything lives. `<root>` = the launcher's folder; tools in `<root>/bin`, or next to the launcher in a
/// dev build (target/release). `FH1_DATA` overrides the data folder.
struct Paths {
    root: PathBuf,
    bin: PathBuf,
    data: PathBuf,
}

impl Paths {
    fn find() -> Self {
        let root = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_else(|| ".".into());
        let bin = if root.join(format!("bin/fh1setup{EXE}")).is_file() { root.join("bin") } else { root.clone() };
        let data = std::env::var_os("FH1_DATA").map(PathBuf::from).unwrap_or_else(|| root.join("data"));
        Paths { root, bin, data }
    }

    /// `<data>/installations/<id>/assets/private` of the active install, if FH1 is installed.
    fn private(&self) -> Option<PathBuf> {
        let inst: serde_json::Value = serde_json::from_slice(&std::fs::read(self.data.join("installation.json")).ok()?).ok()?;
        let p = self.data.join("installations").join(inst["id"].as_str()?).join("assets/private");
        p.join("cars/index.json").is_file().then_some(p)
    }

    fn imported(&self, game: &str) -> bool {
        self.private().is_some_and(|p| p.join("imported").join(game).join("cars/index.json").is_file())
    }
}

/// The user's choices, kept in `<data>/launcher.json` so "Update" can re-run setup without asking again.
/// `serde(default)`: a launcher.json from an older launcher (no `name`) still loads.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
struct Saved {
    fh1: String,
    own_fh2: bool,
    fh2: String,
    own_fm4: bool,
    fm4_play: String,
    fm4_content: String,
    /// Release whose setup last finished (empty = never).
    release: String,
    /// Multiplayer display name (the game's `FH1_NAME`, docs/MULTIPLAYER.md "Player name"); empty = the game's default.
    name: String,
}

/// Longest display name in characters (the wire field is 24 UTF-8 bytes, fh1-net `NAME_LEN`).
const NAME_MAX_CHARS: usize = 16;

/// The multiplayer display name `raw` stands for (same rule as fh1-net `proto::sanitize_name`): letters, digits, spaces
/// and `- _ . '` only (others dropped), runs of spaces collapsed, trimmed, at most 16 characters and 24 bytes. None = fewer
/// than 2 characters left.
fn sanitize_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    for ch in raw.chars() {
        let ok = ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | '\'');
        if ch.is_whitespace() {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else if ok {
            if out.chars().count() >= NAME_MAX_CHARS || out.len() + ch.len_utf8() > 24 {
                break;
            }
            out.push(ch);
        }
    }
    let out = out.trim().to_owned();
    (out.chars().count() >= 2).then_some(out)
}

enum Msg {
    Line(String),
    Progress(f32, String),
    Step(usize),
    Finished(Result<(), String>),
    GameExited(Option<i32>),
}

#[derive(PartialEq)]
enum Screen {
    Setup,
    Installing,
    Home,
}

struct Step {
    title: String,
    args: Vec<String>,
}

struct Launcher {
    paths: Paths,
    saved: Saved,
    screen: Screen,
    delete_work: bool,
    steps: Vec<String>,
    step: usize,
    progress: f32,
    progress_text: String,
    log: Vec<String>,
    error: Option<String>,
    rx: Option<Receiver<Msg>>,
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<Mutex<bool>>,
    game_running: bool,
    game_rx: Option<Receiver<Msg>>,
    status: Option<String>,
    /// FH1's loading backdrop from the converted data (None before install: painted gradient).
    backdrop: Option<egui::TextureHandle>,
    backdrop_pending: bool,
    /// A newer release on GitHub (update.rs), its download progress, and messages from the update thread.
    update: Option<update::Release>,
    update_progress: Option<f32>,
    update_error: Option<String>,
    update_rx: Option<Receiver<update::UpdateMsg>>,
}

impl Launcher {
    fn new(ctx: &egui::Context) -> Self {
        style::install(ctx);
        let paths = Paths::find();
        let backdrop = paths.private().and_then(|p| style::load_backdrop(ctx, &p));
        // Only a release layout updates itself (not a dev build in target\release).
        let update_rx = (update::enabled() && paths.root.join(update::LAUNCHER).is_file()).then(|| {
            update::cleanup(&paths.root);
            let (tx, rx) = channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(update::UpdateMsg::Checked(update::check(VERSION)));
                ctx.request_repaint();
            });
            rx
        });
        let saved: Saved =
            std::fs::read(paths.data.join("launcher.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let screen = if paths.private().is_some() { Screen::Home } else { Screen::Setup };
        Launcher {
            paths,
            saved,
            screen,
            delete_work: true,
            steps: Vec::new(),
            step: 0,
            progress: 0.0,
            progress_text: String::new(),
            log: Vec::new(),
            error: None,
            rx: None,
            child: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(Mutex::new(false)),
            game_running: false,
            game_rx: None,
            status: None,
            backdrop,
            backdrop_pending: false,
            update: None,
            update_progress: None,
            update_error: None,
            update_rx,
        }
    }

    fn save(&self) {
        let _ = std::fs::create_dir_all(&self.paths.data);
        let _ = std::fs::write(self.paths.data.join("launcher.json"), serde_json::to_vec_pretty(&self.saved).unwrap_or_default());
    }

    /// The setup runs for the current choices: FH1 (always; groups already up to date are skipped quickly), then the
    /// optional imports.
    fn plan(&self) -> Vec<Step> {
        let data = self.paths.data.display().to_string();
        let s = &self.saved;
        // Installs already done by this release are skipped (re-running would re-extract the disc).
        let current = s.release == RELEASE;
        let mut steps = Vec::new();
        if !(current && self.paths.private().is_some()) {
            steps.push(Step { title: "Forza Horizon".into(), args: vec![s.fh1.clone(), "--data".into(), data.clone()] });
        }
        if s.own_fh2 && !s.fh2.is_empty() && !(current && self.paths.imported("fh2")) {
            steps.push(Step { title: "Forza Horizon 2".into(), args: vec!["import-fh2".into(), s.fh2.clone(), "--data".into(), data.clone()] });
        }
        if s.own_fm4 && !s.fm4_play.is_empty() && !(current && self.paths.imported("fm4")) {
            let mut args = vec!["import-fm4".into(), s.fm4_play.clone()];
            if !s.fm4_content.is_empty() {
                args.extend(["--content".into(), s.fm4_content.clone()]);
            }
            args.extend(["--data".into(), data]);
            steps.push(Step { title: "Forza Motorsport 4".into(), args });
        }
        steps
    }

    fn start_install(&mut self, ctx: &egui::Context) {
        self.save();
        let steps = self.plan();
        self.steps = steps.iter().map(|s| s.title.clone()).collect();
        self.step = 0;
        self.progress = 0.0;
        self.progress_text.clear();
        self.log.clear();
        self.error = None;
        *self.cancelled.lock().unwrap() = false;
        self.screen = Screen::Installing;
        let (tx, rx) = channel();
        self.rx = Some(rx);
        let setup = self.paths.bin.join(format!("fh1setup{EXE}"));
        let ffmpeg = self.paths.bin.join(format!("ffmpeg{EXE}"));
        let (root, data, delete_work) = (self.paths.root.clone(), self.paths.data.clone(), self.delete_work);
        let (child, cancelled, ctx) = (self.child.clone(), self.cancelled.clone(), ctx.clone());
        std::thread::spawn(move || {
            let send = |m: Msg| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            let _ = std::fs::create_dir_all(&data);
            for (i, step) in steps.iter().enumerate() {
                send(Msg::Step(i));
                let mut cmd = Command::new(&setup);
                hide_console(cmd.args(&step.args).current_dir(&root).stdout(Stdio::piped()).stderr(Stdio::piped()));
                if ffmpeg.is_file() && std::env::var_os("FH1_FFMPEG").is_none() {
                    cmd.env("FH1_FFMPEG", &ffmpeg);
                }
                let result = run_child(cmd, &child, &tx, &ctx);
                if *cancelled.lock().unwrap() {
                    send(Msg::Finished(Err("Cancelled. Run the install again to continue where it stopped.".into())));
                    return;
                }
                if let Err(e) = result {
                    send(Msg::Finished(Err(format!("{}: {e}", step.title))));
                    return;
                }
            }
            if delete_work {
                send(Msg::Line("removing extracted disc files...".into()));
                // The FM4 merge holds junctions into the extracted Play Disc: remove it first (junctions are removed,
                // not followed).
                for d in ["work_fm4", "work_fm4_disc1", "work_fm4_disc2", "work_fh2", "work"] {
                    let _ = std::fs::remove_dir_all(data.join(d));
                }
            }
            send(Msg::Finished(Ok(())));
        });
    }

    fn play(&mut self, ctx: &egui::Context) {
        let logs = self.paths.data.join("logs");
        let _ = std::fs::create_dir_all(&logs);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let log_path = logs.join(format!("game_{stamp}.log"));
        let Ok(log) = std::fs::File::create(&log_path) else {
            self.status = Some(format!("Could not write {}", log_path.display()));
            return;
        };
        let mut cmd = Command::new(self.paths.bin.join(format!("fh1-engine{EXE}")));
        hide_console(cmd.arg("--data").arg(&self.paths.data).current_dir(&self.paths.root));
        if std::env::var_os("FH1_WINDOW").is_none() {
            cmd.env("FH1_WINDOW", "borderless");
        }
        // The name typed on the home screen is the multiplayer name (the game validates it again).
        if let Some(name) = sanitize_name(&self.saved.name) {
            cmd.env("FH1_NAME", name);
        }
        // The engine decodes the intro / FMV movies with the bundled ffmpeg (fh1-video).
        let ffmpeg = self.paths.bin.join(format!("ffmpeg{EXE}"));
        if ffmpeg.is_file() && std::env::var_os("FH1_FFMPEG").is_none() {
            cmd.env("FH1_FFMPEG", &ffmpeg);
        }
        match log.try_clone() {
            Ok(err) => {
                cmd.stdout(log).stderr(err);
            }
            Err(_) => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        match cmd.spawn() {
            Ok(mut child) => {
                self.game_running = true;
                self.status = None;
                let (tx, rx) = channel();
                self.game_rx = Some(rx);
                let repaint = ctx.clone();
                std::thread::spawn(move || {
                    let code = child.wait().ok().and_then(|s| s.code());
                    let _ = tx.send(Msg::GameExited(code));
                    repaint.request_repaint();
                });
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            Err(e) => self.status = Some(format!("Could not start the game: {e}")),
        }
    }

    fn poll_update(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.update_rx else { return };
        while let Ok(m) = rx.try_recv() {
            match m {
                update::UpdateMsg::Checked(r) => self.update = r,
                update::UpdateMsg::Progress(p) => self.update_progress = Some(p),
                update::UpdateMsg::Failed(e) => {
                    self.update_progress = None;
                    self.update_error = Some(e);
                }
                update::UpdateMsg::Installed => {
                    update::restart(&self.paths.root);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn start_update(&mut self, ctx: &egui::Context) {
        let Some(rel) = self.update.clone() else { return };
        self.update_progress = Some(0.0);
        self.update_error = None;
        let (tx, rx) = channel();
        self.update_rx = Some(rx);
        let (root, data, ctx) = (self.paths.root.clone(), self.paths.data.clone(), ctx.clone());
        std::thread::spawn(move || {
            let repaint = || ctx.request_repaint();
            update::install(&rel, &root, &data, &tx, &repaint);
        });
    }

    /// The update card on the home screen: offer, progress or error.
    fn ui_update(&mut self, ui: &mut egui::Ui) {
        let Some(rel) = self.update.clone() else { return };
        style::panel(ui, |ui| {
            style::header(ui, &format!("Update available  ·  v{}", rel.version));
            if let Some(p) = self.update_progress {
                style::progress(ui, p, if p < 0.9 { "downloading" } else { "installing" });
            } else {
                ui.label(RichText::new(format!("You have v{VERSION}. Updating keeps your converted games; the launcher restarts when it's done.")).color(style::DIM));
                if let Some(e) = &self.update_error {
                    ui.label(RichText::new(e).color(style::BAD));
                }
                let busy = self.game_running || self.rx.is_some();
                ui.horizontal(|ui| {
                    if style::menu_item(ui, "Update now", 26.0, !busy, !busy).clicked() {
                        self.start_update(ui.ctx());
                    }
                    if !rel.page.is_empty() && ui.link("What's new").clicked() {
                        open_external(&rel.page);
                    }
                });
                if busy {
                    ui.label(RichText::new("Close the game (or finish the install) to update.").color(style::DIM));
                }
            }
        });
        ui.add_space(10.0);
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.rx {
            let mut finished = None;
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Line(l) => {
                        self.log.push(l);
                        if self.log.len() > 2000 {
                            self.log.drain(..500);
                        }
                    }
                    Msg::Progress(p, t) => {
                        self.progress = p;
                        self.progress_text = t;
                    }
                    Msg::Step(i) => {
                        self.step = i;
                        self.progress = 0.0;
                        self.progress_text.clear();
                    }
                    Msg::Finished(r) => finished = Some(r),
                    Msg::GameExited(_) => {}
                }
            }
            match finished {
                Some(Ok(())) => {
                    self.rx = None;
                    self.saved.release = RELEASE.into();
                    self.save();
                    self.screen = Screen::Home;
                    self.status = Some("Install complete.".into());
                    if self.backdrop.is_none() {
                        self.backdrop_pending = true;
                    }
                }
                Some(Err(e)) => {
                    self.rx = None;
                    self.error = Some(e);
                }
                None => {}
            }
        }
        if let Some(Ok(Msg::GameExited(code))) = self.game_rx.as_ref().map(|rx| rx.try_recv()) {
            self.game_running = false;
            self.game_rx = None;
            if code.is_some_and(|c| c != 0) {
                self.status = Some(format!("The game exited with an error (code {}). Logs: {}", code.unwrap(), self.paths.data.join("logs").display()));
            }
        }
    }

    fn ui_setup(&mut self, ui: &mut egui::Ui) {
        style::panel(ui, |ui| {
            style::header(ui, "Forza Horizon  ·  required");
            ui.label(RichText::new("Your Xbox 360 disc image: the .iso, a .zip holding it, or the folder it's in.").color(style::DIM));
            path_row(ui, "fh1", &mut self.saved.fh1);
        });
        ui.add_space(10.0);
        style::panel(ui, |ui| {
            style::header(ui, "Got other Forza discs?  ·  optional");
            ui.label(RichText::new("Games you don't add stay locked in the menus.").color(style::DIM));
            ui.add_space(2.0);
            ui.checkbox(&mut self.saved.own_fh2, RichText::new("FORZA HORIZON 2  ·  216 cars + the Southern Europe open world").strong());
            if self.saved.own_fh2 {
                ui.indent("fh2", |ui| path_row(ui, "fh2", &mut self.saved.fh2));
            }
            ui.checkbox(&mut self.saved.own_fm4, RichText::new("FORZA MOTORSPORT 4  ·  501 cars + 83 circuits (MOTORSPORT mode)").strong());
            if self.saved.own_fm4 {
                ui.indent("fm4", |ui| {
                    ui.label(RichText::new("Play Disc (Disc 1)").color(style::DIM));
                    path_row(ui, "fm4a", &mut self.saved.fm4_play);
                    ui.label(RichText::new("Content Install Disc (Disc 2, optional: the car-pack cars)").color(style::DIM));
                    path_row(ui, "fm4b", &mut self.saved.fm4_content);
                });
            }
            let mut no = false;
            ui.add_enabled(false, egui::Checkbox::new(&mut no, RichText::new("FORZA MOTORSPORT 3  ·  coming soon").strong()));
        });
        ui.add_space(10.0);
        ui.checkbox(&mut self.delete_work, "Delete the extracted disc files afterwards (your disc images are never touched)");
        let mut gb = 12 + 15;
        if self.saved.release == RELEASE && self.paths.private().is_some() {
            gb = 0;
        }
        if self.saved.own_fh2 {
            gb += 10 + 15;
        }
        if self.saved.own_fm4 {
            gb += 42 + 15;
        }
        ui.label(RichText::new(format!("Needs about {gb} GB free next to this program while installing; up to an hour on slower PCs.")).color(style::DIM));
        ui.add_space(12.0);

        let missing = self.missing();
        ui.horizontal(|ui| {
            if style::menu_item(ui, "Install", 34.0, missing.is_none(), true).clicked() {
                self.start_install(ui.ctx());
            }
            if self.paths.private().is_some() && style::menu_item(ui, "Back", 26.0, true, false).clicked() {
                self.screen = Screen::Home;
            }
        });
        if let Some(m) = missing {
            ui.label(RichText::new(m).color(style::BAD));
        }
    }

    /// Why Install is disabled, if it is.
    fn missing(&self) -> Option<&'static str> {
        let s = &self.saved;
        let bad = |p: &str| p.is_empty() || !Path::new(p).exists();
        let fh1_done = s.release == RELEASE && self.paths.private().is_some();
        if !fh1_done && bad(&s.fh1) {
            return Some("Choose your Forza Horizon disc.");
        }
        if s.own_fh2 && bad(&s.fh2) {
            return Some("Choose your Forza Horizon 2 disc, or untick it.");
        }
        if s.own_fm4 && bad(&s.fm4_play) {
            return Some("Choose your Forza Motorsport 4 Play Disc, or untick it.");
        }
        if s.own_fm4 && !s.fm4_content.is_empty() && bad(&s.fm4_content) {
            return Some("The FM4 Content Install Disc path doesn't exist.");
        }
        if !self.paths.bin.join(format!("fh1setup{EXE}")).is_file() {
            return Some("fh1setup is missing from the bin folder next to this program. Re-extract the release.");
        }
        None
    }

    fn ui_installing(&mut self, ui: &mut egui::Ui) {
        let running = self.rx.is_some();
        style::panel(ui, |ui| {
            style::header(ui, if running { "Installing" } else if self.error.is_some() { "Install stopped" } else { "Installed" });
            ui.add_space(4.0);
            for (i, title) in self.steps.iter().enumerate() {
                let (mark, color) = if i < self.step || (!running && self.error.is_none() && !self.steps.is_empty()) {
                    ("✔", style::OK)
                } else if i == self.step {
                    if self.error.is_some() { ("✖", style::BAD) } else { ("▶", ACCENT) }
                } else {
                    ("•", style::DIM)
                };
                ui.label(RichText::new(format!("{mark}  {}", title.to_uppercase())).font(style::heavy(17.0)).color(color));
            }
            ui.add_space(8.0);
            if running {
                let text = if self.progress_text.is_empty() { "working...".to_owned() } else { self.progress_text.clone() };
                style::progress(ui, self.progress, &text);
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(style::BAD).strong());
                if let Some(last) = self.log.iter().rev().find(|l| l.to_ascii_lowercase().contains("error")) {
                    ui.label(RichText::new(last).color(style::BAD));
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if running {
                if style::menu_item(ui, "Cancel", 24.0, true, false).clicked() {
                    *self.cancelled.lock().unwrap() = true;
                    if let Some(c) = self.child.lock().unwrap().as_mut() {
                        let _ = c.kill();
                    }
                }
            } else if style::menu_item(ui, "Back", 24.0, true, false).clicked() {
                self.screen = Screen::Setup;
            }
        });
        ui.add_space(8.0);
        egui::CollapsingHeader::new(RichText::new("Details").color(style::DIM)).default_open(false).show(ui, |ui| {
            egui::Frame::new().fill(style::PANEL).inner_margin(8).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                    for l in &self.log {
                        ui.label(RichText::new(l).monospace().size(11.0).color(style::DIM));
                    }
                });
            });
        });
    }

    fn ui_home(&mut self, ui: &mut egui::Ui) {
        self.ui_update(ui);
        self.name_row(ui);
        ui.add_space(6.0);
        let play_text = if self.game_running { "Running..." } else { "Play" };
        if style::menu_item(ui, play_text, 52.0, !self.game_running, !self.game_running).clicked() {
            self.play(ui.ctx());
        }
        ui.add_space(4.0);
        if style::menu_item(ui, "Add games / update", 30.0, true, false).clicked() {
            self.screen = Screen::Setup;
        }
        if style::menu_item(ui, "Data folder", 30.0, true, false).clicked() {
            open_external(&self.paths.data);
        }
        if style::menu_item(ui, "Quit", 30.0, true, false).clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ui.add_space(18.0);
        if !self.saved.release.is_empty() && self.saved.release != RELEASE {
            ui.label(RichText::new("This version converts some files differently: choose ADD GAMES / UPDATE before playing.").color(style::ORANGE));
        }
    }
}

impl Launcher {
    /// The multiplayer name box (saved in launcher.json when it loses focus; passed to the game as FH1_NAME).
    fn name_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Online name").color(style::DIM));
            let edit = egui::TextEdit::singleline(&mut self.saved.name).hint_text("Driver").char_limit(32).desired_width(220.0);
            if ui.add_enabled(!self.game_running, edit).lost_focus() {
                // Store what the game will show (the sanitized name), or nothing.
                self.saved.name = sanitize_name(&self.saved.name).unwrap_or_default();
                self.save();
            }
        });
        let typed = self.saved.name.trim();
        let note = match sanitize_name(typed) {
            Some(n) if n == typed => None,
            Some(n) => Some((format!("Shown online as \"{n}\" (letters, digits, spaces and - _ . ' only; up to {NAME_MAX_CHARS})"), style::ORANGE)),
            None if typed.is_empty() => Some(("Empty: the game uses your computer's user name, else \"Driver\"".to_owned(), style::DIM)),
            None => Some(("Too short: at least 2 letters or digits".to_owned(), style::BAD)),
        };
        if let Some((text, color)) = note {
            ui.label(RichText::new(text).size(12.0).color(color));
        }
    }

    /// Installed games card (bottom right of the home screen).
    fn games_card(&self, ui: &mut egui::Ui) {
        style::panel(ui, |ui| {
            style::header(ui, "Your games");
            let fh1 = self.paths.private().is_some();
            for (name, ok, note) in [
                ("Forza Horizon", fh1, ""),
                ("Forza Horizon 2", self.paths.imported("fh2"), "locked"),
                ("Forza Motorsport 4", self.paths.imported("fm4"), "locked"),
                ("Forza Motorsport 3", false, "coming soon"),
            ] {
                let (mark, color) = if ok { ("✔", style::OK) } else { ("—", style::DIM) };
                let text = if ok || note.is_empty() { format!("{mark}  {name}") } else { format!("{mark}  {name}  ·  {note}") };
                ui.label(RichText::new(text).color(color));
            }
        });
    }
}

impl eframe::App for Launcher {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.poll_update(ui.ctx());
        if self.backdrop_pending {
            self.backdrop_pending = false;
            self.backdrop = self.paths.private().and_then(|p| style::load_backdrop(ui.ctx(), &p));
        }
        let full = ui.max_rect();
        style::paint_backdrop(ui.painter(), full, self.backdrop.as_ref());
        // Version + disclaimer, bottom right.
        ui.painter().text(
            full.right_bottom() + egui::vec2(-16.0, -12.0),
            egui::Align2::RIGHT_BOTTOM,
            format!("v{VERSION}  ·  unofficial, not affiliated with Microsoft, Turn 10 or Playground Games"),
            style::body(12.0),
            Color32::from_white_alpha(150),
        );
        let home = self.screen == Screen::Home;
        if home {
            egui::Area::new(egui::Id::new("games")).anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -36.0)).show(ui.ctx(), |ui| self.games_card(ui));
        }
        let column = egui::Rect::from_min_size(
            full.min + egui::vec2(48.0, 32.0),
            egui::vec2((full.width() * 0.55).clamp(520.0, 680.0), full.height() - 64.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(column), |ui| {
            style::logo(ui, if home { 1.0 } else { 0.62 });
            ui.add_space(if home { 26.0 } else { 12.0 });
            if let Some(s) = &self.status {
                ui.label(RichText::new(s).color(Color32::from_rgb(255, 226, 140)));
            }
            egui::ScrollArea::vertical().id_salt("page").auto_shrink([false, false]).show(ui, |ui| match self.screen {
                Screen::Setup => self.ui_setup(ui),
                Screen::Installing => self.ui_installing(ui),
                Screen::Home => self.ui_home(ui),
            });
        });
    }
}

/// A path box with "ISO / ZIP..." and "Folder..." pickers.
fn path_row(ui: &mut egui::Ui, id: &str, path: &mut String) {
    ui.push_id(id, |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(path).desired_width(ui.available_width() - 190.0).hint_text("not chosen"));
            if ui.button("ISO / ZIP...").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("Disc image", &["iso", "zip"]).pick_file() {
                    *path = p.display().to_string();
                }
            }
            if ui.button("Folder...").clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    *path = p.display().to_string();
                }
            }
        });
    });
}

/// Run one fh1setup process to the end, streaming its output as [`Msg`]s.
fn run_child(mut cmd: Command, slot: &Arc<Mutex<Option<Child>>>, tx: &Sender<Msg>, ctx: &egui::Context) -> Result<(), String> {
    let mut child = cmd.spawn().map_err(|e| format!("could not start fh1setup: {e}"))?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    *slot.lock().unwrap() = Some(child);
    let readers: Vec<_> = [out.map(|o| Box::new(o) as Box<dyn std::io::Read + Send>), err.map(|e| Box::new(e) as Box<dyn std::io::Read + Send>)]
        .into_iter()
        .flatten()
        .map(|r| {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let mut last_error = None;
                for line in BufReader::new(r).lines().map_while(Result::ok) {
                    if let Some(p) = parse_progress(&line) {
                        let _ = tx.send(p);
                    } else {
                        if line.starts_with("Error") || line.to_ascii_lowercase().contains("error:") {
                            last_error = Some(line.clone());
                        }
                        let _ = tx.send(Msg::Line(line));
                    }
                    ctx.request_repaint();
                }
                last_error
            })
        })
        .collect();
    let mut last_error = None;
    for r in readers {
        if let Ok(Some(e)) = r.join() {
            last_error = Some(e);
        }
    }
    let status = loop {
        let mut guard = slot.lock().unwrap();
        match guard.as_mut().map(|c| c.try_wait()) {
            Some(Ok(Some(s))) => {
                *guard = None;
                break Some(s);
            }
            Some(Ok(None)) => {}
            _ => {
                *guard = None;
                break None;
            }
        }
        drop(guard);
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    match status {
        Some(s) if s.success() => Ok(()),
        _ => Err(last_error.unwrap_or_else(|| "setup failed (see Details)".into())),
    }
}

/// `[progress] <done>/<total> <group>` from fh1setup's install loop.
fn parse_progress(line: &str) -> Option<Msg> {
    let rest = line.strip_prefix("[progress] ")?;
    let (frac, group) = rest.split_once(' ').unwrap_or((rest, ""));
    let (a, b) = frac.split_once('/')?;
    let (a, b): (f32, f32) = (a.parse().ok()?, b.parse().ok()?);
    let text = if group == "done" { "done".to_owned() } else { format!("converting {group} ({}/{})", a as u32 + 1, b as u32) };
    Some(Msg::Progress(if b > 0.0 { a / b } else { 0.0 }, text))
}
