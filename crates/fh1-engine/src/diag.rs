//! Diagnostics overlay (top right): FPS, frame time, CPU, RAM, GPU, VRAM. F3 toggles it; `FH1_DIAG=0` starts hidden.
//! CPU/RAM come from sysinfo and GPU/VRAM from NVIDIA's NVML (nvml.dll, loaded at runtime; "n/a" on other GPUs),
//! both sampled on a worker thread every 0.5 s so the frame never waits on them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;

use crate::ui::UiFont;

pub struct DiagPlugin;

impl Plugin for DiagPlugin {
    fn build(&self, app: &mut App) {
        let shared = Arc::new(Mutex::new(SysStats::default()));
        let worker = shared.clone();
        std::thread::Builder::new().name("fh1-diag".into()).spawn(move || sample_loop(worker)).ok();
        app.insert_resource(Diag { shared, frames: Vec::new(), next_update: 0.0 })
            .add_systems(Startup, spawn_overlay)
            .add_systems(Update, (toggle, update_overlay));
    }
}

/// Latest system sample (also read by the gameplay perf recorder, perf/record.rs).
#[derive(Default, Clone)]
pub(crate) struct SysStats {
    pub(crate) cpu_process: f32,
    pub(crate) cpu_total: f32,
    pub(crate) ram_process: u64,
    pub(crate) ram_used: u64,
    pub(crate) ram_total: u64,
    pub(crate) gpu: Option<GpuStats>,
}

#[derive(Clone)]
pub(crate) struct GpuStats {
    pub(crate) name: String,
    pub(crate) util: u32,
    pub(crate) vram_used: u64,
    pub(crate) vram_total: u64,
    pub(crate) temp: u32,
}

#[derive(Resource)]
pub(crate) struct Diag {
    pub(crate) shared: Arc<Mutex<SysStats>>,
    /// (real time, frame dt) over the last second.
    frames: Vec<(f64, f32)>,
    next_update: f64,
}

#[derive(Component)]
struct DiagText;

fn spawn_overlay(mut commands: Commands, font: Res<UiFont>) {
    let hidden = std::env::var("FH1_DIAG").is_ok_and(|v| v == "0");
    commands.spawn((
        DiagText,
        Text::new(""),
        font.text(14.0),
        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.9)),
        TextLayout::justify(Justify::Right),
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.45)),
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(10.0),
            top: Val::Px(10.0),
            padding: UiRect::axes(Val::Px(8.0), Val::Px(5.0)),
            ..default()
        },
        GlobalZIndex(1000),
        if hidden { Visibility::Hidden } else { Visibility::Inherited },
    ));
}

fn toggle(keys: Res<ButtonInput<KeyCode>>, mut q: Query<&mut Visibility, With<DiagText>>) {
    if keys.just_pressed(KeyCode::F3) {
        for mut v in &mut q {
            *v = if *v == Visibility::Hidden { Visibility::Inherited } else { Visibility::Hidden };
        }
    }
}

fn update_overlay(time: Res<Time<Real>>, mut diag: ResMut<Diag>, mut q: Query<&mut Text, With<DiagText>>) {
    let now = time.elapsed_secs_f64();
    diag.frames.push((now, time.delta_secs()));
    diag.frames.retain(|(t, _)| now - t <= 1.0);
    if now < diag.next_update {
        return;
    }
    diag.next_update = now + 0.5;
    let Ok(mut text) = q.single_mut() else { return };

    let n = diag.frames.len().max(1) as f32;
    let avg_ms = diag.frames.iter().map(|f| f.1).sum::<f32>() / n * 1000.0;
    let max_ms = diag.frames.iter().map(|f| f.1).fold(0.0, f32::max) * 1000.0;
    let fps = if avg_ms > 0.0 { 1000.0 / avg_ms } else { 0.0 };
    let s = diag.shared.lock().map(|s| s.clone()).unwrap_or_default();

    let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    let mut out = format!(
        "FPS {fps:.0}\nFrame {avg_ms:.1} ms (worst {max_ms:.1})\nCPU {:.0}% (system {:.0}%)\nRAM {:.2} GB (system {:.1}/{:.1} GB)",
        s.cpu_process,
        s.cpu_total,
        gb(s.ram_process),
        gb(s.ram_used),
        gb(s.ram_total),
    );
    match &s.gpu {
        Some(g) => out += &format!(
            "\nGPU {}% {}°C\nVRAM {:.2}/{:.1} GB\n{}",
            g.util,
            g.temp,
            gb(g.vram_used),
            gb(g.vram_total),
            g.name
        ),
        None => out += "\nGPU n/a",
    }
    text.0 = out;
}

fn sample_loop(shared: Arc<Mutex<SysStats>>) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pid = sysinfo::get_current_pid().ok();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
    let nvml = Nvml::load();
    loop {
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let mut stats = SysStats { cpu_total: sys.global_cpu_usage(), ram_used: sys.used_memory(), ram_total: sys.total_memory(), ..default() };
        if let Some(pid) = pid {
            sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), false, ProcessRefreshKind::nothing().with_cpu().with_memory());
            if let Some(p) = sys.process(pid) {
                // sysinfo reports per-core percent (up to cores x 100); show share of the whole machine.
                stats.cpu_process = p.cpu_usage() / cores;
                stats.ram_process = p.memory();
            }
        }
        stats.gpu = nvml.as_ref().and_then(Nvml::sample);
        if let Ok(mut s) = shared.lock() {
            *s = stats;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Minimal NVML binding (first GPU only).
struct Nvml {
    _lib: libloading::Library,
    device: *mut std::ffi::c_void,
    utilization: unsafe extern "C" fn(*mut std::ffi::c_void, *mut [u32; 2]) -> i32,
    memory: unsafe extern "C" fn(*mut std::ffi::c_void, *mut [u64; 3]) -> i32,
    temperature: unsafe extern "C" fn(*mut std::ffi::c_void, i32, *mut u32) -> i32,
    name: String,
}

// The device handle is only used from the sampling thread that created it.
unsafe impl Send for Nvml {}

impl Nvml {
    fn load() -> Option<Nvml> {
        unsafe {
            let lib = libloading::Library::new("nvml.dll").ok()?;
            let init: libloading::Symbol<unsafe extern "C" fn() -> i32> = lib.get(b"nvmlInit_v2\0").ok()?;
            if init() != 0 {
                return None;
            }
            let by_index: libloading::Symbol<unsafe extern "C" fn(u32, *mut *mut std::ffi::c_void) -> i32> =
                lib.get(b"nvmlDeviceGetHandleByIndex_v2\0").ok()?;
            let mut device = std::ptr::null_mut();
            if by_index(0, &mut device) != 0 {
                return None;
            }
            let get_name: libloading::Symbol<unsafe extern "C" fn(*mut std::ffi::c_void, *mut u8, u32) -> i32> =
                lib.get(b"nvmlDeviceGetName\0").ok()?;
            let mut buf = [0u8; 96];
            let name = if get_name(device, buf.as_mut_ptr(), buf.len() as u32) == 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                String::from_utf8_lossy(&buf[..end]).into_owned()
            } else {
                String::new()
            };
            let utilization = *lib.get(b"nvmlDeviceGetUtilizationRates\0").ok()?;
            let memory = *lib.get(b"nvmlDeviceGetMemoryInfo\0").ok()?;
            let temperature = *lib.get(b"nvmlDeviceGetTemperature\0").ok()?;
            Some(Nvml { _lib: lib, device, utilization, memory, temperature, name })
        }
    }

    fn sample(&self) -> Option<GpuStats> {
        let (mut util, mut mem, mut temp) = ([0u32; 2], [0u64; 3], 0u32);
        unsafe {
            if (self.utilization)(self.device, &mut util) != 0 || (self.memory)(self.device, &mut mem) != 0 {
                return None;
            }
            // NVML_TEMPERATURE_GPU = 0.
            (self.temperature)(self.device, 0, &mut temp);
        }
        // nvmlMemory_t = { total, free, used }.
        Some(GpuStats { name: self.name.clone(), util: util[0], vram_used: mem[2], vram_total: mem[0], temp })
    }
}
