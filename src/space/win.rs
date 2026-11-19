//! Small Win32 bindings: drives, elevation, Explorer and the Recycle Bin.

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
    fn SHEmptyRecycleBinW(hwnd: *mut c_void, root: *const u16, flags: u32) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[derive(Clone)]
pub struct Drive {
    pub root: String,
    pub label: String,
    pub fs: String,
    pub total: u64,
    pub free: u64,
}

impl Drive {
    pub fn letter(&self) -> &str {
        self.root.trim_end_matches('\\')
    }

    pub fn name(&self) -> &str {
        if self.label.is_empty() {
            "Local Disk"
        } else {
            &self.label
        }
    }
}

pub fn drives() -> Vec<Drive> {
    let mut buf = [0u16; 512];
    let len = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
    // Don't pop "insert a disk" dialogs for empty card readers.
    unsafe { SetErrorMode(0x0001) };
    let mut out = Vec::new();
    for root in buf[..len.min(buf.len())].split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let root = String::from_utf16_lossy(root);
        let w = wide(&root);
        let kind = unsafe { GetDriveTypeW(w.as_ptr()) };
        // removable, fixed, network, RAM disk
        if !matches!(kind, 2 | 3 | 4 | 6) {
            continue;
        }
        let mut label = [0u16; 261];
        let mut fs = [0u16; 64];
        let ok = unsafe {
            GetVolumeInformationW(
                w.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            continue; // no media
        }
        let Some((total, free)) = disk_space(&root) else { continue };
        out.push(Drive { root, label: from_wide(&label), fs: from_wide(&fs), total, free });
    }
    out
}

pub fn disk_space(path: &str) -> Option<(u64, u64)> {
    let w = wide(path);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, free))
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
pub fn relaunch_as_admin(scan_target: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = wide(&exe.to_string_lossy());
    // A trailing backslash would escape the closing quote.
    let params = wide(&format!("--scan \"{}\"", scan_target.trim_end_matches('\\')));
    let op = wide("runas");
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), exe.as_ptr(), params.as_ptr(), std::ptr::null(), 1)
    };
    r > 32
}

/// Opens Explorer with the item selected.
pub fn reveal(path: &str) {
    let _ = Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
}

pub fn open(path: &str) {
    let _ = Command::new("explorer.exe").raw_arg(format!("\"{path}\"")).spawn();
}

pub fn empty_recycle_bin(drive_root: &str) -> Result<(), String> {
    const NOCONFIRMATION: u32 = 0x1;
    const NOPROGRESSUI: u32 = 0x2;
    const NOSOUND: u32 = 0x4;
    let w = wide(drive_root);
    let hr = unsafe {
        SHEmptyRecycleBinW(std::ptr::null_mut(), w.as_ptr(), NOCONFIRMATION | NOPROGRESSUI | NOSOUND)
    };
    // E_UNEXPECTED comes back when the bin is already empty.
    if hr == 0 || hr as u32 == 0x8000_FFFF {
        Ok(())
    } else {
        Err(format!("Couldn't empty the Recycle Bin (0x{:08X})", hr as u32))
    }
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
