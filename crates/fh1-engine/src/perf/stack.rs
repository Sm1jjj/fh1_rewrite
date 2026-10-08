//! Thread stack capture for the stall watchdog (Windows x64). The 2026-10-07 freezes (10-40 s at ~1 % CPU) landed in a
//! different system every time and the main thread was only ever seen parked waiting on worker tasks, so the watchdog
//! samples EVERY thread of the process (FH1_STALL_ALL_THREADS=0 = main thread only): each one is suspended on its own,
//! unwound with the x64 unwind tables (RtlLookupFunctionEntry / RtlVirtualUnwind) into a fixed array, timed
//! (GetThreadTimes) and resumed. Nothing allocates and no lock is taken while a thread is suspended: it may hold the heap
//! lock. Frames are named later with dbghelp (the PDB must sit next to the exe or in FH1_PDB_DIR; launch.bat copies it),
//! and [`warm_up`] loads the PDB at startup on a background thread: the first stall report used to spend ~25 s there.

/// Process-wide counters, read at the start and end of a stall: did the process do I/O, fault pages or burn CPU while
/// the main thread stood still?
#[derive(Clone, Copy, Default)]
pub struct Counters {
    pub read_ops: u64,
    pub read_bytes: u64,
    pub write_ops: u64,
    pub write_bytes: u64,
    pub other_ops: u64,
    pub page_faults: u64,
    pub working_set: u64,
    pub cpu_100ns: u64,
}

impl Counters {
    /// "reads 12 (3.4 MB), writes ..., page faults +N, CPU x.x s over the stall" against an earlier reading.
    pub fn delta(&self, then: &Counters, secs: f64) -> String {
        let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
        let cpu = self.cpu_100ns.saturating_sub(then.cpu_100ns) as f64 / 1e7;
        format!(
            "process during the stall: reads {} ({:.1} MB), writes {} ({:.1} MB), other I/O {}, page faults +{}, working set {:.0} -> {:.0} MB, CPU {:.2} s in {:.1} s ({:.0} % of one core)",
            self.read_ops.saturating_sub(then.read_ops),
            mb(self.read_bytes.saturating_sub(then.read_bytes)),
            self.write_ops.saturating_sub(then.write_ops),
            mb(self.write_bytes.saturating_sub(then.write_bytes)),
            self.other_ops.saturating_sub(then.other_ops),
            self.page_faults.saturating_sub(then.page_faults),
            mb(then.working_set),
            mb(self.working_set),
            cpu,
            secs,
            if secs > 0.0 { cpu / secs * 100.0 } else { 0.0 },
        )
    }
}

/// One thread's stack at one moment: return addresses innermost first, and its CPU time so far (kernel + user, 100 ns).
#[derive(Clone)]
pub struct ThreadStack {
    pub tid: u32,
    pub name: String,
    pub main: bool,
    pub pcs: Vec<u64>,
    pub cpu_100ns: u64,
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind, SymFromAddrW, SymGetLineFromAddrW64, SymInitializeW, SymSetOptions,
        CONTEXT, IMAGEHLP_LINEW64, SYMBOL_INFOW,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread};

    use super::{Counters, ThreadStack};

    // Declared here rather than through more windows-sys features (Diagnostics_ToolHelp, ProcessStatus).
    #[repr(C)]
    struct ThreadEntry32 {
        size: u32,
        usage: u32,
        tid: u32,
        owner_pid: u32,
        base_pri: i32,
        delta_pri: i32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_ops: u64,
        write_ops: u64,
        other_ops: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct MemoryCounters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
        fn Thread32First(snap: HANDLE, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snap: HANDLE, entry: *mut ThreadEntry32) -> i32;
        fn GetCurrentProcessId() -> u32;
        fn GetThreadDescription(thread: HANDLE, out: *mut *mut u16) -> i32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
        fn GetThreadTimes(thread: HANDLE, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
        fn GetProcessTimes(process: HANDLE, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
        fn GetProcessIoCounters(process: HANDLE, counters: *mut IoCounters) -> i32;
        fn K32GetProcessMemoryInfo(process: HANDLE, counters: *mut MemoryCounters, cb: u32) -> i32;
    }

    const TH32CS_SNAPTHREAD: u32 = 0x4;
    // THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION
    const ACCESS: u32 = 0x0002 | 0x0008 | 0x0040;
    const MAX: usize = 48;

    static MAIN: AtomicU32 = AtomicU32::new(0);
    /// dbghelp is single-threaded: every Sym* call goes through this lock (initialised flag + name cache).
    static DBGHELP: Mutex<Option<HashMap<u64, String>>> = Mutex::new(None);

    /// Call on the main thread (the watchdog plugin's build).
    pub fn remember_main() {
        MAIN.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    }

    fn all_threads() -> bool {
        !std::env::var("FH1_STALL_ALL_THREADS").is_ok_and(|v| v == "0")
    }

    /// The process's thread ids (main first), without the calling thread.
    fn thread_ids() -> Vec<u32> {
        let main = MAIN.load(Ordering::Relaxed);
        let me = unsafe { GetCurrentThreadId() };
        let mut ids = Vec::new();
        if main != 0 && main != me {
            ids.push(main);
        }
        if !all_threads() {
            return ids;
        }
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap.is_null() || snap as isize == -1 {
                return ids;
            }
            let pid = GetCurrentProcessId();
            let mut e: ThreadEntry32 = std::mem::zeroed();
            e.size = std::mem::size_of::<ThreadEntry32>() as u32;
            let mut ok = Thread32First(snap, &mut e) != 0;
            while ok {
                if e.owner_pid == pid && e.tid != me && e.tid != main {
                    ids.push(e.tid);
                }
                e.size = std::mem::size_of::<ThreadEntry32>() as u32;
                ok = Thread32Next(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        ids
    }

    fn thread_name(h: HANDLE) -> String {
        unsafe {
            let mut p: *mut u16 = std::ptr::null_mut();
            if GetThreadDescription(h, &mut p) < 0 || p.is_null() {
                return String::new();
            }
            let mut n = 0;
            while *p.add(n) != 0 && n < 256 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            LocalFree(p as *mut c_void);
            s
        }
    }

    /// Suspends `h`, unwinds its stack into `out`, reads its CPU time, resumes it. No allocation, no locks in between.
    fn capture(h: HANDLE, out: &mut [u64; MAX]) -> (usize, u64) {
        unsafe {
            let mut n = 0;
            let mut cpu = 0u64;
            if SuspendThread(h) == u32::MAX {
                return (0, 0);
            }
            let (mut c, mut x, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
            if GetThreadTimes(h, &mut c, &mut x, &mut k, &mut u) != 0 {
                cpu = k + u;
            }
            // GetThreadContext needs a 16-byte aligned CONTEXT on x64; windows-sys declares it plain repr(C), so an
            // unaligned one made every call fail and every stall report came out without frames (2026-10-08).
            #[repr(C, align(16))]
            struct Aligned(CONTEXT);
            let mut aligned: Aligned = std::mem::zeroed();
            let ctx = &mut aligned.0;
            ctx.ContextFlags = 0x0010_000B; // CONTEXT_FULL (AMD64)
            if GetThreadContext(h, ctx) != 0 {
                while n < MAX && ctx.Rip != 0 {
                    out[n] = ctx.Rip;
                    n += 1;
                    let mut base = 0u64;
                    let f = RtlLookupFunctionEntry(ctx.Rip, &mut base, std::ptr::null_mut());
                    if f.is_null() {
                        // Leaf function: the return address is on top of the stack.
                        if ctx.Rsp == 0 {
                            break;
                        }
                        ctx.Rip = *(ctx.Rsp as *const u64);
                        ctx.Rsp += 8;
                    } else {
                        let mut data = std::ptr::null_mut();
                        let mut frame = 0u64;
                        RtlVirtualUnwind(0, base, ctx.Rip, f, ctx, &mut data, &mut frame, std::ptr::null_mut());
                    }
                }
            }
            ResumeThread(h);
            (n, cpu)
        }
    }

    /// Every thread's stack (only the main thread with FH1_STALL_ALL_THREADS=0). Each thread is stopped only for its
    /// own unwind.
    pub fn sample() -> Vec<ThreadStack> {
        let main = MAIN.load(Ordering::Relaxed);
        let mut out = Vec::new();
        for tid in thread_ids() {
            let h: HANDLE = unsafe { OpenThread(ACCESS, 0, tid) };
            if h.is_null() {
                continue;
            }
            // Allocations before the suspend (the name) or after the resume (the frames), never in between.
            let name = thread_name(h);
            let mut pcs = [0u64; MAX];
            let (n, cpu) = capture(h, &mut pcs);
            unsafe { CloseHandle(h) };
            out.push(ThreadStack { tid, name, main: tid == main, pcs: pcs[..n].to_vec(), cpu_100ns: cpu });
        }
        out
    }

    pub fn counters() -> Counters {
        let mut c = Counters::default();
        unsafe {
            let p = GetCurrentProcess();
            let mut io = IoCounters::default();
            if GetProcessIoCounters(p, &mut io) != 0 {
                c.read_ops = io.read_ops;
                c.read_bytes = io.read_bytes;
                c.write_ops = io.write_ops;
                c.write_bytes = io.write_bytes;
                c.other_ops = io.other_ops;
            }
            let mut m = MemoryCounters { cb: std::mem::size_of::<MemoryCounters>() as u32, ..Default::default() };
            if K32GetProcessMemoryInfo(p, &mut m, m.cb) != 0 {
                c.page_faults = m.page_faults as u64;
                c.working_set = m.working_set as u64;
            }
            let (mut a, mut b, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
            if GetProcessTimes(p, &mut a, &mut b, &mut k, &mut u) != 0 {
                c.cpu_100ns = k + u;
            }
        }
        c
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// dbghelp initialised (once) and the name cache, under the dbghelp lock.
    fn with_dbghelp<R>(f: impl FnOnce(&mut HashMap<u64, String>) -> R) -> R {
        let mut g = DBGHELP.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            let mut path = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.display().to_string())).unwrap_or_default();
            if let Ok(extra) = std::env::var("FH1_PDB_DIR") {
                path = format!("{path};{extra}");
            }
            let w = wide(&path);
            unsafe {
                SymSetOptions(0x2 | 0x4 | 0x10); // UNDNAME | DEFERRED_LOADS | LOAD_LINES
                SymInitializeW(GetCurrentProcess(), w.as_ptr(), 1);
            }
            *g = Some(HashMap::new());
        }
        f(g.as_mut().unwrap())
    }

    /// Loads dbghelp and the exe's PDB on a background thread, so a stall report doesn't wait for it.
    pub fn warm_up() {
        let _ = std::thread::Builder::new().name("fh1-stall-symbols".into()).spawn(|| {
            let t = std::time::Instant::now();
            // Any address inside the exe makes dbghelp load its PDB.
            let pc = warm_up as fn() as usize as u64;
            let name = symbol(pc);
            bevy::log::info!("stall watchdog: symbols ready in {:.1} s ({name})", t.elapsed().as_secs_f32());
        });
    }

    /// `symbol+0xoff (file:line)` for one return address (the address itself when there are no symbols).
    pub fn symbol(pc: u64) -> String {
        with_dbghelp(|cache| {
            if let Some(s) = cache.get(&pc) {
                return s.clone();
            }
            let process = unsafe { GetCurrentProcess() };
            // SYMBOL_INFOW + room for a 1 KiB name, 8-byte aligned.
            let mut buf = vec![0u64; (std::mem::size_of::<SYMBOL_INFOW>() + 2048) / 8 + 1];
            let sym = buf.as_mut_ptr() as *mut SYMBOL_INFOW;
            let mut disp = 0u64;
            let name = unsafe {
                (*sym).SizeOfStruct = std::mem::size_of::<SYMBOL_INFOW>() as u32;
                (*sym).MaxNameLen = 1000;
                if SymFromAddrW(process, pc, &mut disp, sym) != 0 {
                    let len = (*sym).NameLen as usize;
                    let p = std::ptr::addr_of!((*sym).Name) as *const u16;
                    Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
                } else {
                    None
                }
            };
            let mut line: IMAGEHLP_LINEW64 = unsafe { std::mem::zeroed() };
            line.SizeOfStruct = std::mem::size_of::<IMAGEHLP_LINEW64>() as u32;
            let mut ld = 0u32;
            let at = unsafe {
                if SymGetLineFromAddrW64(process, pc, &mut ld, &mut line) != 0 && !line.FileName.is_null() {
                    let mut l = 0;
                    while *line.FileName.add(l) != 0 && l < 1024 {
                        l += 1;
                    }
                    let f = String::from_utf16_lossy(std::slice::from_raw_parts(line.FileName, l));
                    let short = f.rsplit(['\\', '/']).take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
                    format!(" ({short}:{})", line.LineNumber)
                } else {
                    String::new()
                }
            };
            let s = match name {
                Some(n) => format!("{n}+0x{disp:x}{at}"),
                None => format!("0x{pc:016x}"),
            };
            cache.insert(pc, s.clone());
            s
        })
    }
}

#[cfg(windows)]
pub use imp::{counters, remember_main, sample, symbol, warm_up};

#[cfg(not(windows))]
pub fn remember_main() {}

#[cfg(not(windows))]
pub fn warm_up() {}

#[cfg(not(windows))]
pub fn sample() -> Vec<ThreadStack> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn counters() -> Counters {
    Counters::default()
}

#[cfg(not(windows))]
pub fn symbol(pc: u64) -> String {
    format!("0x{pc:016x}")
}
