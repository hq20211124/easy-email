// A desktop app must never show a console window. The Tauri template ships
// `cfg_attr(not(debug_assertions), windows_subsystem = "windows")`, which leaves every debug build
// as a console-subsystem binary: launching `target\debug\easy-email.exe` from Explorer then opens
// a cmd window behind the app, which looks broken.
//
// Without a console, diagnostics would vanish, so there are two ways back in:
//   * `easy-email.exe --console` — re-attaches stdout/stderr to the terminal that launched it
//   * `easy-email.exe > log.txt 2>&1` — a GUI-subsystem process inherits handles it was handed,
//     so redirection works with no console at all
// Panics additionally go to `startup-error.log`; see `log_line` in lib.rs.
#![windows_subsystem = "windows"]

/// Console plumbing for `--console`.
///
/// The four kernel32 calls are declared directly rather than pulling in `windows-sys` as a
/// dependency for a handful of flat functions.
#[cfg(windows)]
mod console {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn SetConsoleOutputCP(code_page: u32) -> i32;
        fn SetStdHandle(std_handle: u32, handle: isize) -> i32;
    }

    /// `ATTACH_PARENT_PROCESS`: attach to the console of our parent, i.e. the terminal we came from.
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // Negative pseudo-handle values; std has no constants for these.
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const CP_UTF8: u32 = 65_001;

    /// Is this stream already pointing somewhere we can write to?
    ///
    /// A GUI-subsystem process still inherits handles its launcher passed it, so under
    /// `--console > log.txt` stdout is already a valid file handle. Overwriting that with the
    /// console would silently discard the redirect the caller asked for.
    fn stream_is_usable(std_handle: u32) -> bool {
        let handle = unsafe { GetStdHandle(std_handle) };
        !handle.is_null() && handle as isize != -1
    }

    fn point_stream_at_console(std_handle: u32) {
        // Safety: `OpenOptions` returns a real handle and the console is attached by now.
        unsafe {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
                SetStdHandle(std_handle, file.as_raw_handle() as isize);
                // The handle must outlive this function: dropping the File would close it and
                // break every later write.
                std::mem::forget(file);
            }
        }
    }

    pub fn attach_to_parent() {
        let output_missing = !stream_is_usable(STD_OUTPUT_HANDLE);
        let error_missing = !stream_is_usable(STD_ERROR_HANDLE);

        // Safety: every call takes plain integers/handles and each result is checked.
        unsafe {
            // Only go looking for a console for the streams that need one. Fails when launched
            // from Explorer or a shortcut — no parent console exists — which is not an error.
            if (output_missing || error_missing) && AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
                // Without this, Chinese diagnostics come out as mojibake on a default OEM code page
                // console; PowerShell hit exactly that in this project already.
                SetConsoleOutputCP(CP_UTF8);
                if output_missing {
                    point_stream_at_console(STD_OUTPUT_HANDLE);
                }
                if error_missing {
                    point_stream_at_console(STD_ERROR_HANDLE);
                }
            }
        }

        // Written to both streams unconditionally, which is what makes this feature testable:
        // whichever way the caller arranged for output to arrive, they see which streams are live.
        println!("[easy-email] --console: stdout 已就绪");
        eprintln!("[easy-email] --console: stderr 已就绪");
    }
}

fn main() {
    #[cfg(windows)]
    {
        if std::env::args().any(|arg| arg == "--console") {
            console::attach_to_parent();
        }
    }

    easy_email_lib::run();
}
