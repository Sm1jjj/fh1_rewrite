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
//! <root>/bin/ffmpeg.exe         XMA audio decoding during setup (FH1_FFMPEG)
//! <root>/data/                  converted assets (installation.json, installations/<id>/...), launcher.json, logs/
//! ```
//! Only FH1 is required. Games that are not imported stay locked (greyed) in the game's menus.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, RichText};
use serde::{Deserialize, Serialize};

/// No console window for the child processes.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Bump with every release whose setup output changes, so existing installs are offered an update.
const RELEASE: &str = env!("CARGO_PKG_VERSION");
const ACCENT: Color32 = Color32::from_rgb(255, 120, 30);

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FH1 Rewrite")
            .with_inner_size([760.0, 600.0])
            .with_min_inner_size([620.0, 480.0]),
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
        let bin = if root.join("bin/fh1setup.exe").is_file() { root.join("bin") } else { root.clone() };
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
#[derive(Serialize, Deserialize, Default, Clone)]
struct Saved {
    fh1: String,
    own_fh2: bool,
    fh2: String,
    own_fm4: bool,
    fm4_play: String,
    fm4_content: String,
    /// Release whose setup last finished (empty = never).
    release: String,
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
}

impl Launcher {
    fn new(ctx: &egui::Context) -> Self {
        ctx.set_visuals(egui::Visuals::dark());
        let paths = Paths::find();
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
        let setup = self.paths.bin.join("fh1setup.exe");
        let ffmpeg = self.paths.bin.join("ffmpeg.exe");
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
                cmd.args(&step.args).current_dir(&root).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
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
        let mut cmd = Command::new(self.paths.bin.join("fh1-engine.exe"));
        cmd.arg("--data").arg(&self.paths.data).current_dir(&self.paths.root).creation_flags(CREATE_NO_WINDOW);
        if std::env::var_os("FH1_WINDOW").is_none() {
            cmd.env("FH1_WINDOW", "borderless");
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
        ui.label("FH1 Rewrite converts your own game discs into its own files. Nothing is downloaded and no game files are included.");
        ui.add_space(12.0);

        section(ui, "Forza Horizon (required)");
        ui.label("Your Forza Horizon (Xbox 360) disc image: the .iso, a .zip holding it, or the folder it is in.");
        path_row(ui, "fh1", &mut self.saved.fh1);
        ui.add_space(14.0);

        section(ui, "Do you also own these games? (optional)");
        ui.label(RichText::new("Content from games you don't add stays locked in the game's menus.").weak());
        ui.add_space(4.0);
        ui.checkbox(&mut self.saved.own_fh2, "Forza Horizon 2 (Xbox 360): its cars and the Southern Europe map");
        if self.saved.own_fh2 {
            ui.indent("fh2", |ui| path_row(ui, "fh2", &mut self.saved.fh2));
        }
        ui.checkbox(&mut self.saved.own_fm4, "Forza Motorsport 4: its cars and circuits (MOTORSPORT mode)");
        if self.saved.own_fm4 {
            ui.indent("fm4", |ui| {
                ui.label("Play Disc (Disc 1):");
                path_row(ui, "fm4a", &mut self.saved.fm4_play);
                ui.label("Content Install Disc (Disc 2, optional: adds the car-pack cars):");
                path_row(ui, "fm4b", &mut self.saved.fm4_content);
            });
        }
        let mut no = false;
        ui.add_enabled(false, egui::Checkbox::new(&mut no, "Forza Motorsport 3 (coming soon)"));
        ui.add_space(14.0);

        ui.checkbox(&mut self.delete_work, "Delete the extracted disc files afterwards (saves space; your disc images are never touched)");
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
        ui.label(RichText::new(format!("Needs about {gb} GB free next to this program while installing. It takes a while: up to an hour on slower PCs.")).weak());
        ui.add_space(10.0);

        let missing = self.missing();
        ui.horizontal(|ui| {
            let go = ui.add_enabled(missing.is_none(), egui::Button::new(RichText::new("  Install  ").size(18.0).strong()).fill(ACCENT));
            if go.clicked() {
                self.start_install(ui.ctx());
            }
            if self.paths.private().is_some() && ui.button("Back").clicked() {
                self.screen = Screen::Home;
            }
            if let Some(m) = missing {
                ui.label(RichText::new(m).color(Color32::LIGHT_RED));
            }
        });
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
        if !self.paths.bin.join("fh1setup.exe").is_file() {
            return Some("fh1setup.exe is missing next to this program (bin\\). Re-extract the release.");
        }
        None
    }

    fn ui_installing(&mut self, ui: &mut egui::Ui) {
        let running = self.rx.is_some();
        for (i, title) in self.steps.iter().enumerate() {
            let (mark, color) = if i < self.step || (!running && self.error.is_none() && !self.steps.is_empty()) {
                ("✔", Color32::LIGHT_GREEN)
            } else if i == self.step {
                if self.error.is_some() { ("✖", Color32::LIGHT_RED) } else { ("▶", ACCENT) }
            } else {
                ("•", Color32::GRAY)
            };
            ui.label(RichText::new(format!("{mark}  {title}")).color(color).size(16.0));
        }
        ui.add_space(8.0);
        if running {
            let text = if self.progress_text.is_empty() { "working...".to_owned() } else { self.progress_text.clone() };
            ui.add(egui::ProgressBar::new(self.progress).text(text).animate(true));
        }
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(Color32::LIGHT_RED).strong());
            if let Some(last) = self.log.iter().rev().find(|l| l.to_ascii_lowercase().contains("error")) {
                ui.label(RichText::new(last).color(Color32::LIGHT_RED));
            }
        }
        ui.horizontal(|ui| {
            if running {
                if ui.button("Cancel").clicked() {
                    *self.cancelled.lock().unwrap() = true;
                    if let Some(c) = self.child.lock().unwrap().as_mut() {
                        let _ = c.kill();
                    }
                }
            } else if ui.button("Back").clicked() {
                self.screen = Screen::Setup;
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new("Details").weak());
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for l in &self.log {
                ui.label(RichText::new(l).monospace().size(11.0));
            }
        });
    }

    fn ui_home(&mut self, ui: &mut egui::Ui) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            let play = ui.add_enabled(
                !self.game_running,
                egui::Button::new(RichText::new(if self.game_running { "  Running...  " } else { "  PLAY  " }).size(30.0).strong())
                    .fill(ACCENT)
                    .min_size(egui::vec2(260.0, 64.0)),
            );
            if play.clicked() {
                self.play(ui.ctx());
            }
        });
        ui.add_space(24.0);
        section(ui, "Installed games");
        let fh1 = self.paths.private().is_some();
        for (name, ok, note) in [
            ("Forza Horizon", fh1, ""),
            ("Forza Horizon 2", self.paths.imported("fh2"), "locked in the menus"),
            ("Forza Motorsport 4", self.paths.imported("fm4"), "locked in the menus"),
            ("Forza Motorsport 3", false, "coming soon"),
        ] {
            let (mark, color) = if ok { ("✔", Color32::LIGHT_GREEN) } else { ("—", Color32::GRAY) };
            let text = if ok || note.is_empty() { format!("{mark}  {name}") } else { format!("{mark}  {name}  ({note})") };
            ui.label(RichText::new(text).color(color).size(15.0));
        }
        ui.add_space(12.0);
        if !self.saved.release.is_empty() && self.saved.release != RELEASE {
            ui.label(RichText::new("This version converts some files differently: run Update before playing.").color(ACCENT));
        }
        ui.horizontal(|ui| {
            if ui.button("Add games / update").clicked() {
                self.screen = Screen::Setup;
            }
            if ui.button("Open data folder").clicked() {
                let _ = Command::new("explorer").arg(&self.paths.data).spawn();
            }
        });
    }
}

impl eframe::App for Launcher {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("FH1 Rewrite").size(26.0).strong().color(ACCENT));
                ui.label(RichText::new(format!("v{RELEASE}  ·  unofficial; not affiliated with Microsoft, Turn 10 or Playground Games")).weak());
            });
            ui.separator();
            if let Some(s) = &self.status {
                ui.label(RichText::new(s).color(Color32::LIGHT_YELLOW));
            }
            egui::ScrollArea::vertical().id_salt("page").auto_shrink([false, false]).show(ui, |ui| match self.screen {
                Screen::Setup => self.ui_setup(ui),
                Screen::Installing => self.ui_installing(ui),
                Screen::Home => self.ui_home(ui),
            });
        });
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(17.0).strong());
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
