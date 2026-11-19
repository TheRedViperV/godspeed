//! Small Win32 bindings: drives, elevation, raw device control and Explorer.

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::process::Command;

type Handle = *mut c_void;
type Bool = i32;

#[link(name = "kernel32")]
extern "system" {
    fn GetLogicalDriveStringsW(len: u32, buf: *mut u16) -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetVolumeInformationW(
        root: *const u16,
        name: *mut u16,
        name_len: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        fs_name: *mut u16,
        fs_name_len: u32,
    ) -> Bool;
    fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> Bool;
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentThread() -> Handle;
    fn SetThreadPriority(thread: Handle, priority: i32) -> Bool;
    fn SetPriorityClass(process: Handle, class: u32) -> Bool;
    fn CloseHandle(h: Handle) -> Bool;
    fn FileTimeToLocalFileTime(ft: *const u64, local: *mut u64) -> Bool;
    fn SetErrorMode(mode: u32) -> u32;
    pub fn DeviceIoControl(
        device: Handle,
        code: u32,
        in_buf: *const c_void,
        in_len: u32,
        out_buf: *mut c_void,
        out_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> Bool;
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> Bool;
    fn GetTokenInformation(token: Handle, class: u32, info: *mut c_void, len: u32, returned: *mut u32) -> Bool;
}

#[link(name = "mpr")]
extern "system" {
    fn WNetGetConnectionW(local: *const u16, remote: *mut u16, len: *mut u32) -> u32;
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut c_void,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> isize;
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: [i32; 4],
    work: [i32; 4],
    flags: u32,
}

#[link(name = "user32")]
extern "system" {
    fn GetCursorPos(pt: *mut Point) -> Bool;
    fn MonitorFromPoint(pt: Point, flags: u32) -> Handle;
    fn GetMonitorInfoW(monitor: Handle, info: *mut MonitorInfo) -> Bool;
    fn GetWindowLongPtrW(hwnd: Handle, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: Handle, index: i32, value: isize) -> isize;
}

/// A tool window has no taskbar button and stays out of Alt+Tab: right for a
/// floating search bar. The taskbar decides when the window is shown, so set it
/// before then; Alt+Tab reads it as it goes.
pub fn set_tool_window(hwnd: isize, tool: bool) {
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_TOOLWINDOW: isize = 0x80;
    const WS_EX_APPWINDOW: isize = 0x40000;
    let h = hwnd as Handle;
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let ex = if tool { (ex | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW } else { ex & !WS_EX_TOOLWINDOW };
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex);
    }
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: Handle, attr: u32, value: *const c_void, len: u32) -> i32;
}

pub fn cursor_pos() -> (i32, i32) {
    let mut pt = Point { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut pt) };
    (pt.x, pt.y)
}

/// The usable part (minus the taskbar) of the screen the mouse is on, in pixels:
/// left, top, right, bottom.
pub fn work_area_at_cursor() -> Option<[i32; 4]> {
    unsafe {
        let mut pt = Point { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, 2); // the nearest one
        let mut info = MonitorInfo { size: std::mem::size_of::<MonitorInfo>() as u32, monitor: [0; 4], work: [0; 4], flags: 0 };
        (GetMonitorInfoW(mon, &mut info) != 0).then_some(info.work)
    }
}

/// Whether a point (in pixels) is on any screen.
pub fn on_screen(x: i32, y: i32) -> bool {
    unsafe { !MonitorFromPoint(Point { x, y }, 0).is_null() } // MONITOR_DEFAULTTONULL
}

/// Keeps a window from being drawn on screen while it still works normally, so
/// it can be resized and painted first and appear only in its final shape.
pub fn cloak(hwnd: isize, on: bool) {
    const DWMWA_CLOAK: u32 = 13;
    let value: i32 = on as i32;
    unsafe { DwmSetWindowAttribute(hwnd as Handle, DWMWA_CLOAK, &value as *const i32 as _, 4) };
}

/// Windows 11's rounded corners, for a window without a title bar.
pub fn round_corners(hwnd: isize) {
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWCP_ROUND: u32 = 2;
    unsafe { DwmSetWindowAttribute(hwnd as Handle, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND as *const u32 as _, 4) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

pub struct Drive {
    pub letter: char,
    pub fs: String,
    /// A mapped network share.
    pub network: bool,
    /// Reachable right now (a network share can be mapped but disconnected).
    pub online: bool,
    /// For network drives, the share it points at (`\\server\share`).
    pub remote: String,
}

/// Drives worth indexing: fixed, removable and RAM disks, plus mapped network
/// shares (which are only indexed if switched on).
pub fn drives() -> Vec<Drive> {
    let mut buf = [0u16; 512];
    let len = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
    // Don't pop "insert a disk" dialogs for empty card readers.
    unsafe { SetErrorMode(0x0001) };
    let mut out = Vec::new();
    for root in buf[..len.min(buf.len())].split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let root = String::from_utf16_lossy(root);
        let w = wide(&root);
        // removable, fixed, network, RAM disk
        let kind = unsafe { GetDriveTypeW(w.as_ptr()) };
        if !matches!(kind, 2 | 3 | 4 | 6) {
            continue;
        }
        let network = kind == 4;
        let mut fs = [0u16; 64];
        let ok = unsafe {
            GetVolumeInformationW(
                w.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        let Some(letter) = root.chars().next().filter(|c| c.is_ascii_alphabetic()) else { continue };
        let letter = letter.to_ascii_uppercase();
        if ok != 0 || network {
            let remote = if network { remote_name(letter).unwrap_or_default() } else { String::new() };
            out.push(Drive { letter, fs: from_wide(&fs), network, online: ok != 0, remote });
        }
    }
    out
}

/// The share a mapped drive letter points at, like `\\server\share`.
fn remote_name(letter: char) -> Option<String> {
    let w = wide(&format!("{letter}:"));
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    (unsafe { WNetGetConnectionW(w.as_ptr(), buf.as_mut_ptr(), &mut len) } == 0).then(|| from_wide(&buf))
}

/// (total, free) bytes on a drive.
pub fn disk_space(letter: char) -> Option<(u64, u64)> {
    let w = wide(&format!("{letter}:\\"));
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, free))
}

/// Lowest CPU priority for the calling thread, so anything else running (a game,
/// say) always goes first.
pub fn low_priority_thread() {
    const THREAD_PRIORITY_LOWEST: i32 = -2;
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
}

/// Background mode for the calling thread: lowest CPU priority and low disk
/// priority, for big reads like the file table.
pub fn background_thread() {
    const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// Idle priority for the whole app while it isn't in front.
pub fn set_idle(idle: bool) {
    const IDLE_PRIORITY_CLASS: u32 = 0x40;
    const NORMAL_PRIORITY_CLASS: u32 = 0x20;
    unsafe { SetPriorityClass(GetCurrentProcess(), if idle { IDLE_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS }) };
}

pub fn is_elevated() -> bool {
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ELEVATION: u32 = 20;
    unsafe {
        let mut token: Handle = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevated = 0u32;
        let mut ret = 0u32;
        let ok = GetTokenInformation(token, TOKEN_ELEVATION, &mut elevated as *mut u32 as _, 4, &mut ret);
        CloseHandle(token);
        ok != 0 && elevated != 0
    }
}

/// Starts a new elevated copy of the app (Windows shows the UAC prompt).
/// Returns false if that didn't happen, for example because the prompt was declined.
pub fn relaunch_as_admin() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = wide(&exe.to_string_lossy());
    let params = wide("--elevated");
    let op = wide("runas");
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), exe.as_ptr(), params.as_ptr(), std::ptr::null(), 1)
    };
    r > 32
}

// Both go through explorer.exe, which hands off to the user's own (non-elevated)
// shell, so files don't open with admin rights just because Godspeed has them.

/// Opens Explorer with the item selected.
pub fn reveal(path: &str) {
    let _ = Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
}

/// Opens a file with its default app, or a folder in Explorer.
pub fn open(path: &str) {
    let _ = Command::new("explorer.exe").raw_arg(format!("\"{path}\"")).spawn();
}

pub fn to_local_filetime(ft: u64) -> u64 {
    let mut out = 0u64;
    let ok = unsafe { FileTimeToLocalFileTime(&ft, &mut out) };
    if ok == 0 {
        ft
    } else {
        out
    }
}
