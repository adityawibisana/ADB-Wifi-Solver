//! Small Windows console helpers.

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleMode,
        CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };

    /// Turns on ANSI escape handling. Returns false when stdout is not a console.
    pub fn enable_ansi() -> bool {
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return false;
            }
            mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
                || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }

    /// Visible window size as (columns, rows).
    pub fn window_size() -> Option<(usize, usize)> {
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(handle, &mut info) == 0 {
                return None;
            }
            let w = info.srWindow.Right - info.srWindow.Left + 1;
            let h = info.srWindow.Bottom - info.srWindow.Top + 1;
            Some((w.max(0) as usize, h.max(0) as usize))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn enable_ansi() -> bool {
        true
    }
    pub fn window_size() -> Option<(usize, usize)> {
        None
    }
}

pub use imp::{enable_ansi, window_size};
