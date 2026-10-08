//! Non-blocking stdout / stderr (2026-10-08). The stall watchdog's stacks for the 31 s freeze in log 20261008_140912 showed
//! `scenery::stream` inside `ZwWriteFile` writing one `info!` line to stderr (launch.bat redirects it to
//! data/perf_logs/game_<id>.log) for the whole stall, with the main thread waiting on it: every game thread that logs writes
//! the file synchronously, so one slow write froze the game.
//!
//! At startup both standard handles are pointed at the write end of an anonymous pipe with a large buffer, and a writer
//! thread copies the pipe into the original handle. Rust's std looks the standard handle up on every write
//! (`GetStdHandle`), so `println!` / `eprintln!` / Bevy's log layer all land in the pipe. A slow file write now only delays
//! the writer thread; game threads would block only once [`PIPE_BYTES`] of unwritten output piled up.
//! `FH1_LOG_PIPE=0` = write the handles directly (old).

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const INVALID_HANDLE: Handle = -1isize as Handle;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn SetStdHandle(which: u32, handle: Handle) -> i32;
        fn CreatePipe(read: *mut Handle, write: *mut Handle, attrs: *mut c_void, size: u32) -> i32;
        fn ReadFile(h: Handle, buf: *mut u8, len: u32, read: *mut u32, overlapped: *mut c_void) -> i32;
        fn WriteFile(h: Handle, buf: *const u8, len: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
    }

    /// Pipe buffer: hours of normal logging (a whole session writes ~50 KB).
    const PIPE_BYTES: u32 = 8 << 20;

    struct SendHandle(Handle);
    unsafe impl Send for SendHandle {}

    pub fn install() {
        if std::env::var("FH1_LOG_PIPE").is_ok_and(|v| v == "0") {
            return;
        }
        unsafe {
            let out = GetStdHandle(STD_OUTPUT_HANDLE);
            let err = GetStdHandle(STD_ERROR_HANDLE);
            // Both point at the same place (launch.bat `> game.log 2>&1`) or at a console: one pipe, written to stderr's
            // target (or stdout's when stderr is missing).
            let target = if !err.is_null() && err != INVALID_HANDLE { err } else { out };
            if target.is_null() || target == INVALID_HANDLE {
                return;
            }
            let (mut r, mut w): (Handle, Handle) = (std::ptr::null_mut(), std::ptr::null_mut());
            if CreatePipe(&mut r, &mut w, std::ptr::null_mut(), PIPE_BYTES) == 0 {
                return;
            }
            let (r, target) = (SendHandle(r), SendHandle(target));
            let spawned = std::thread::Builder::new().name("fh1-log-writer".into()).spawn(move || {
                let (r, target) = (r, target);
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let mut n = 0u32;
                    if ReadFile(r.0, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) == 0 || n == 0 {
                        break;
                    }
                    let mut off = 0u32;
                    while off < n {
                        let mut done = 0u32;
                        if WriteFile(target.0, buf.as_ptr().add(off as usize), n - off, &mut done, std::ptr::null_mut()) == 0 || done == 0 {
                            break;
                        }
                        off += done;
                    }
                }
            });
            if spawned.is_err() {
                return;
            }
            SetStdHandle(STD_OUTPUT_HANDLE, w);
            SetStdHandle(STD_ERROR_HANDLE, w);
        }
        // A panic's message goes into the pipe too: give the writer time to drain it before the process exits.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            prev(info);
            std::thread::sleep(std::time::Duration::from_millis(500));
        }));
    }
}

#[cfg(windows)]
pub use imp::install;

#[cfg(not(windows))]
pub fn install() {}
