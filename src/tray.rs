//! The global hotkey and the tray icon. Both arrive at a hidden window on a
//! thread of their own, so the floating search bar is one keypress away even
//! while the main window is closed. Also: starting with Windows, and handing
//! over to a copy that's already running.

use crate::{edition, icon, index::lock, win};
use eframe::egui;
use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

type Hwnd = *mut c_void;
type Proc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;

#[repr(C)]
struct WndClassExW {
    size: u32,
    style: u32,
    proc_: Proc,
    cls_extra: i32,
    wnd_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu_name: *const u16,
    class_name: *const u16,
    icon_sm: *mut c_void,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: [i32; 2],
    private: u32,
}

#[repr(C)]
struct NotifyIconData {
    size: u32,
    hwnd: Hwnd,
    id: u32,
    flags: u32,
    callback: u32,
    icon: *mut c_void,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: [u32; 4],
    balloon_icon: *mut c_void,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: Hwnd,
        menu: *mut c_void,
        instance: *mut c_void,
        param: *mut c_void,
    ) -> Hwnd;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn RegisterHotKey(hwnd: Hwnd, id: i32, mods: u32, vk: u32) -> i32;
    fn UnregisterHotKey(hwnd: Hwnd, id: i32) -> i32;
    fn FindWindowW(class: *const u16, name: *const u16) -> Hwnd;
    fn CreatePopupMenu() -> *mut c_void;
    fn AppendMenuW(menu: *mut c_void, flags: u32, id: usize, text: *const u16) -> i32;
    fn TrackPopupMenu(menu: *mut c_void, flags: u32, x: i32, y: i32, reserved: i32, hwnd: Hwnd, rect: *const c_void) -> i32;
    fn DestroyMenu(menu: *mut c_void) -> i32;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    fn RegisterWindowMessageW(name: *const u16) -> u32;
    fn ChangeWindowMessageFilterEx(hwnd: Hwnd, msg: u32, action: u32, info: *mut c_void) -> i32;
    fn SetTimer(hwnd: Hwnd, id: usize, ms: u32, f: *const c_void) -> usize;
    fn KillTimer(hwnd: Hwnd, id: usize) -> i32;
    fn CreateIcon(instance: *mut c_void, w: i32, h: i32, planes: u8, bits: u8, and: *const u8, xor: *const u8) -> *mut c_void;
    fn GetSystemMetrics(index: i32) -> i32;
    fn SetWindowLongPtrW(hwnd: Hwnd, index: i32, value: isize) -> isize;
    fn CallWindowProcW(prev: isize, hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn IsZoomed(hwnd: Hwnd) -> i32;
    fn IsIconic(hwnd: Hwnd) -> i32;
    fn GetClientRect(hwnd: Hwnd, rect: *mut [i32; 4]) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconW(op: u32, data: *mut NotifyIconData) -> i32;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegSetKeyValueW(key: isize, sub: *const u16, name: *const u16, kind: u32, data: *const c_void, len: u32) -> i32;
    fn RegDeleteKeyValueW(key: isize, sub: *const u16, name: *const u16) -> i32;
}

const WM_APP: u32 = 0x8000;
/// From the tray icon.
const WM_TRAY: u32 = WM_APP + 1;
const WM_SET_HOTKEY: u32 = WM_APP + 2;
/// From a second copy of the app: show yourself.
const WM_SHOW: u32 = WM_APP + 3;
const WM_BALLOON: u32 = WM_APP + 4;
/// From `--quit`.
const WM_QUIT_APP: u32 = WM_APP + 5;
/// To the app's window: run a frame (see `poke`).
const WM_WAKE: u32 = WM_APP + 7;
const WM_HOTKEY: u32 = 0x0312;
const WM_TIMER: u32 = 0x0113;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn copy_into(dst: &mut [u16], s: &str) {
    let room = dst.len() - 1; // keeps the final 0
    for (d, c) in dst.iter_mut().take(room).zip(s.encode_utf16()) {
        *d = c;
    }
}

// ------------------------------------------------------------------ hotkeys

pub const ALT: u32 = 1;
pub const CTRL: u32 = 2;
pub const SHIFT: u32 = 4;
pub const WIN: u32 = 8;

/// A key combination, as Windows' RegisterHotKey takes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

impl Hotkey {
    pub const DEFAULT: Hotkey = Hotkey { mods: ALT, vk: 0x20 };

    /// "Ctrl+Shift+K" and the like.
    pub fn parse(s: &str) -> Option<Hotkey> {
        let mut mods = 0;
        let mut vk = None;
        for part in s.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods |= CTRL,
                "alt" => mods |= ALT,
                "shift" => mods |= SHIFT,
                "win" => mods |= WIN,
                _ => vk = key_vk(part),
            }
        }
        vk.map(|vk| Hotkey { mods, vk }).filter(Hotkey::usable)
    }

    /// What was pressed while recording a new hotkey, if it makes a good one.
    pub fn from_key(m: egui::Modifiers, key: egui::Key) -> Option<Hotkey> {
        let mods = if m.ctrl { CTRL } else { 0 } | if m.alt { ALT } else { 0 } | if m.shift { SHIFT } else { 0 };
        key_vk(key.name()).map(|vk| Hotkey { mods, vk }).filter(Hotkey::usable)
    }

    /// Function keys work alone; anything else needs Ctrl or Alt, or it would
    /// swallow ordinary typing.
    fn usable(&self) -> bool {
        (0x70..=0x87).contains(&self.vk) || self.mods & (CTRL | ALT) != 0
    }
}

impl std::fmt::Display for Hotkey {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        for (bit, name) in [(CTRL, "Ctrl+"), (ALT, "Alt+"), (SHIFT, "Shift+"), (WIN, "Win+")] {
            if self.mods & bit != 0 {
                f.write_str(name)?;
            }
        }
        match self.vk {
            0x20 => f.write_str("Space"),
            0x30..=0x39 | 0x41..=0x5a => write!(f, "{}", self.vk as u8 as char),
            0x70..=0x87 => write!(f, "F{}", self.vk - 0x6f),
            vk => write!(f, "Key {vk}"),
        }
    }
}

/// The virtual-key code for a key's name: letters, digits, F1–F24 and Space.
fn key_vk(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    match up.as_bytes() {
        [c @ (b'A'..=b'Z' | b'0'..=b'9')] => Some(*c as u32),
        _ if up == "SPACE" => Some(0x20),
        [b'F', ..] => up[1..].parse::<u32>().ok().filter(|n| (1..=24).contains(n)).map(|n| 0x6f + n),
        _ => None,
    }
}

// --------------------------------------------------------------- the thread

pub enum Signal {
    /// The hotkey: show the search bar, or hide it.
    Hotkey,
    /// "Search" in the tray menu.
    Bar,
    /// A click on the tray icon, or the app was started again.
    Open,
    Quit,
    /// A click on a folder's size in Explorer: open it in Space.
    Analyze(String),
    /// The column's own menu changed one of its settings: save them.
    ColumnPrefs,
}

impl Signal {
    pub fn name(&self) -> &'static str {
        match self {
            Signal::Hotkey => "hotkey",
            Signal::Bar => "search from the tray",
            Signal::Open => "open",
            Signal::Quit => "quit",
            Signal::Analyze(_) => "open in Space",
            Signal::ColumnPrefs => "column settings",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum HotkeyState {
    /// Switched off, or about to be tried.
    Off,
    Ready,
    /// Another app has it.
    Taken,
}

struct State {
    tx: Mutex<Sender<Signal>>,
    wake: Box<dyn Fn() + Send + Sync>,
    hotkey: Mutex<Option<Hotkey>>,
    hotkey_state: AtomicU8,
    tries: AtomicU32,
    taskbar_created: AtomicU32,
    icon: AtomicIsize,
    balloon: Mutex<Option<(String, String)>>,
}

static STATE: OnceLock<State> = OnceLock::new();
static HWND: AtomicIsize = AtomicIsize::new(0);
/// The app's own window, and its window procedure before ours.
static APP: AtomicIsize = AtomicIsize::new(0);
static OLD_PROC: AtomicIsize = AtomicIsize::new(0);
/// Set before closing for real, so the window doesn't just hide to the tray.
pub static QUITTING: AtomicBool = AtomicBool::new(false);

fn class_name() -> String {
    format!("{}Tray", edition::get().name)
}

/// Starts the tray icon and the hotkey. Signals come out of the receiver, with
/// `wake` called after each so the app looks at them.
pub fn start(hotkey: Option<Hotkey>, wake: impl Fn() + Send + Sync + 'static) -> Option<Receiver<Signal>> {
    let (tx, rx) = channel();
    let state = State {
        tx: Mutex::new(tx),
        wake: Box::new(wake),
        hotkey: Mutex::new(hotkey),
        hotkey_state: AtomicU8::new(0),
        tries: AtomicU32::new(0),
        taskbar_created: AtomicU32::new(0),
        icon: AtomicIsize::new(0),
        balloon: Mutex::new(None),
    };
    STATE.set(state).ok()?;
    std::thread::spawn(|| unsafe { run() });
    Some(rx)
}

/// For other parts of the app (the Explorer column) to ask the window for something.
pub fn request(s: Signal) {
    send(s);
}

fn send(s: Signal) {
    crate::trace::log(format!("tray: {}", s.name()));
    if let Some(st) = STATE.get() {
        let _ = lock(&st.tx).send(s);
        (st.wake)();
        poke();
    }
}

/// Hooks the app's window so `poke` can wake it. Call from the window's own thread.
pub fn set_app_window(hwnd: isize) {
    APP.store(hwnd, Relaxed);
    let old = unsafe { SetWindowLongPtrW(hwnd as Hwnd, -4, app_window_proc as Proc as usize as isize) }; // GWLP_WNDPROC
    OLD_PROC.store(old, Relaxed);
}

/// Makes the app window run a frame, even hidden or minimized. eframe only draws
/// when Windows asks a window to paint, which it never does for those, but it
/// also draws right away after a resize. So this has the window report its own
/// size again, measured by its own thread when the message arrives: always the
/// current size, never one that's about to change.
pub fn poke() {
    let hwnd = APP.load(Relaxed);
    if hwnd != 0 {
        unsafe { PostMessageW(hwnd as Hwnd, WM_WAKE, 0, 0) };
    }
}

unsafe extern "system" fn app_window_proc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    if msg == WM_WAKE {
        let mut r = [0i32; 4];
        GetClientRect(hwnd, &mut r);
        // A minimized window measures 0 x 0, and eframe ignores that size.
        let (w, h) = ((r[2] as u32).max(1), (r[3] as u32).max(1));
        let size = ((h & 0xffff) << 16 | (w & 0xffff)) as isize;
        // SIZE_MINIMIZED, SIZE_MAXIMIZED or SIZE_RESTORED: as it is now.
        let kind = if IsIconic(hwnd) != 0 { 1 } else if IsZoomed(hwnd) != 0 { 2 } else { 0 };
        return SendMessageW(hwnd, 0x0005, kind, size); // WM_SIZE
    }
    CallWindowProcW(OLD_PROC.load(Relaxed), hwnd, msg, wparam, lparam)
}

fn post(msg: u32) {
    let hwnd = HWND.load(Relaxed);
    if hwnd != 0 {
        unsafe { PostMessageW(hwnd as Hwnd, msg, 0, 0) };
    }
}

/// None while a new hotkey is being recorded, so pressing the old one types it.
pub fn set_hotkey(k: Option<Hotkey>) {
    if let Some(st) = STATE.get() {
        *lock(&st.hotkey) = k;
        st.tries.store(0, Relaxed);
        post(WM_SET_HOTKEY);
    }
}

pub fn hotkey_state() -> HotkeyState {
    match STATE.get().map_or(0, |s| s.hotkey_state.load(Relaxed)) {
        1 => HotkeyState::Ready,
        2 => HotkeyState::Taken,
        _ => HotkeyState::Off,
    }
}

/// A Windows notification from the tray icon.
pub fn balloon(title: &str, text: &str) {
    if let Some(st) = STATE.get() {
        *lock(&st.balloon) = Some((title.into(), text.into()));
        post(WM_BALLOON);
    }
}

/// Takes the icon out of the tray (otherwise it lingers after the app quits).
pub fn remove() {
    let hwnd = HWND.load(Relaxed);
    if hwnd != 0 {
        let mut d = icon_data(hwnd as Hwnd);
        unsafe { Shell_NotifyIconW(2, &mut d) };
    }
}

/// Closes a running copy (`--quit`). False if there's none.
pub fn quit_running() -> bool {
    let class = wide(&class_name());
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    !hwnd.is_null() && unsafe { PostMessageW(hwnd, WM_QUIT_APP, 0, 0) } != 0
}

/// True if this edition is already running, in which case it's told to show itself
/// (unless `show` is false).
pub fn hand_over(show: bool) -> bool {
    let class = wide(&class_name());
    let hwnd = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    if hwnd.is_null() {
        return false;
    }
    if show {
        unsafe { PostMessageW(hwnd, WM_SHOW, 0, 0) };
    }
    true
}

unsafe fn run() {
    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide(&class_name());
    let wc = WndClassExW {
        size: std::mem::size_of::<WndClassExW>() as u32,
        style: 0,
        proc_: window_proc,
        cls_extra: 0,
        wnd_extra: 0,
        instance,
        icon: std::ptr::null_mut(),
        cursor: std::ptr::null_mut(),
        background: std::ptr::null_mut(),
        menu_name: std::ptr::null(),
        class_name: class.as_ptr(),
        icon_sm: std::ptr::null_mut(),
    };
    RegisterClassExW(&wc);
    const WS_EX_TOOLWINDOW: u32 = 0x80;
    const WS_POPUP: u32 = 0x8000_0000;
    let title = wide(edition::get().name);
    let null = std::ptr::null_mut();
    let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), title.as_ptr(), WS_POPUP, 0, 0, 0, 0, null, null, instance, null);
    if hwnd.is_null() {
        return;
    }
    HWND.store(hwnd as isize, Relaxed);
    let st = STATE.get().unwrap();
    st.taskbar_created.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Relaxed);
    // These come from Explorer and from a second copy of the app, which don't run
    // as administrator; an elevated Godspeed has to let them through.
    for m in [WM_TRAY, WM_SHOW, WM_QUIT_APP, st.taskbar_created.load(Relaxed)] {
        ChangeWindowMessageFilterEx(hwnd, m, 1, null);
    }
    st.icon.store(make_icon(GetSystemMetrics(49).clamp(16, 64) as u32) as isize, Relaxed); // SM_CXSMICON, at this DPI
    add_icon(hwnd);
    register(hwnd);
    let mut msg: Msg = std::mem::zeroed();
    while GetMessageW(&mut msg, null, 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn window_proc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    let Some(st) = STATE.get() else { return DefWindowProcW(hwnd, msg, wparam, lparam) };
    match msg {
        WM_HOTKEY => send(Signal::Hotkey),
        WM_TRAY => match lparam as u32 {
            0x0202 => send(Signal::Open), // left button up
            0x0205 => menu(hwnd),         // right button up
            _ => {}
        },
        WM_SHOW => send(Signal::Open),
        WM_QUIT_APP => send(Signal::Quit),
        WM_SET_HOTKEY => register(hwnd),
        WM_TIMER => {
            KillTimer(hwnd, 1);
            register(hwnd);
        }
        WM_BALLOON => {
            if let Some((title, text)) = lock(&st.balloon).take() {
                let mut d = icon_data(hwnd);
                d.flags = 0x10; // NIF_INFO
                copy_into(&mut d.info_title, &title);
                copy_into(&mut d.info, &text);
                Shell_NotifyIconW(1, &mut d);
            }
        }
        // Explorer restarted: the tray is new and empty.
        m if m != 0 && m == st.taskbar_created.load(Relaxed) => add_icon(hwnd),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    0
}

fn icon_data(hwnd: Hwnd) -> NotifyIconData {
    let mut d: NotifyIconData = unsafe { std::mem::zeroed() };
    d.size = std::mem::size_of::<NotifyIconData>() as u32;
    d.hwnd = hwnd;
    d.id = 1;
    d
}

fn tip() -> String {
    let name = edition::get().name;
    match STATE.get().and_then(|s| *lock(&s.hotkey)) {
        Some(k) if hotkey_state() == HotkeyState::Ready => format!("{name} · {k} to search"),
        _ => name.to_string(),
    }
}

unsafe fn add_icon(hwnd: Hwnd) {
    let mut d = icon_data(hwnd);
    d.flags = 1 | 2 | 4; // message, icon, tip
    d.callback = WM_TRAY;
    d.icon = STATE.get().map_or(0, |s| s.icon.load(Relaxed)) as *mut c_void;
    copy_into(&mut d.tip, &tip());
    Shell_NotifyIconW(0, &mut d);
}

/// The app's icon, drawn rather than loaded, `n` pixels square.
unsafe fn make_icon(n: u32) -> *mut c_void {
    let mut bgra = icon::rgba(n, edition::get().palette);
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let mask = vec![0u8; (n.div_ceil(16) * 2 * n) as usize];
    CreateIcon(GetModuleHandleW(std::ptr::null()), n as i32, n as i32, 1, 32, mask.as_ptr(), bgra.as_ptr())
}

/// (Re)registers the hotkey. If another program holds it (often a copy of this
/// app that's just closing, after an update), tries again for a few seconds.
unsafe fn register(hwnd: Hwnd) {
    let st = STATE.get().unwrap();
    UnregisterHotKey(hwnd, 1);
    let state = match *lock(&st.hotkey) {
        None => 0,
        Some(k) if RegisterHotKey(hwnd, 1, k.mods | 0x4000, k.vk) != 0 => 1, // MOD_NOREPEAT
        Some(_) if st.tries.fetch_add(1, Relaxed) < 12 => {
            SetTimer(hwnd, 1, 500, std::ptr::null());
            0
        }
        Some(_) => 2,
    };
    st.hotkey_state.store(state, Relaxed);
    crate::trace::log(format!("hotkey {:?}: {}", *lock(&st.hotkey), ["off", "ready", "taken"][state as usize]));
    let mut d = icon_data(hwnd);
    d.flags = 4;
    copy_into(&mut d.tip, &tip());
    Shell_NotifyIconW(1, &mut d);
    (st.wake)();
}

unsafe fn menu(hwnd: Hwnd) {
    let m = CreatePopupMenu();
    let name = edition::get().name;
    let search = match STATE.get().and_then(|s| *lock(&s.hotkey)) {
        Some(k) => format!("Search\t{k}"),
        None => "Search".to_string(),
    };
    AppendMenuW(m, 0, 1, wide(&search).as_ptr());
    AppendMenuW(m, 0, 2, wide(&format!("Open {name}")).as_ptr());
    AppendMenuW(m, 0x800, 0, std::ptr::null()); // separator
    AppendMenuW(m, 0, 3, wide("Quit").as_ptr());
    let (x, y) = win::cursor_pos();
    // Without this the menu doesn't close when clicking elsewhere.
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(m, 0x100 | 0x2 | 0x20, x, y, 0, hwnd, std::ptr::null()); // return the pick, right button, above the cursor
    PostMessageW(hwnd, 0, 0, 0);
    DestroyMenu(m);
    match cmd {
        1 => send(Signal::Bar),
        2 => send(Signal::Open),
        3 => send(Signal::Quit),
        _ => {}
    }
}

// ------------------------------------------------------------ with Windows

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Windows' on/off switch for each Run entry (Task Manager's Startup apps page).
/// Recent builds skip an entry that has no record here at all.
const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Starts the app, in the tray, when you sign in to Windows. An elevated Godspeed
/// uses a sign-in task, which can give it admin rights without a UAC prompt each
/// time; otherwise it's the usual Run entry.
pub fn set_autostart(on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy().into_owned();
    let name = edition::get().name;
    let (key, approved, value) = (wide(RUN_KEY), wide(APPROVED_KEY), wide(name));
    if edition::get().admin && win::is_elevated() {
        unsafe {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr());
            RegDeleteKeyValueW(HKEY_CURRENT_USER, approved.as_ptr(), value.as_ptr());
        }
        let schtasks = |args: &[&str]| {
            std::process::Command::new("schtasks.exe").args(args).creation_flags(CREATE_NO_WINDOW).output()
        };
        if !on {
            let _ = schtasks(&["/delete", "/tn", name, "/f"]);
            return Ok(());
        }
        let file = std::env::temp_dir().join(format!("{}-task.xml", name.to_lowercase()));
        let xml: Vec<u16> = std::iter::once(0xfeff).chain(task_xml(&exe).encode_utf16()).collect();
        let bytes: Vec<u8> = xml.iter().flat_map(|c| c.to_le_bytes()).collect();
        std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
        let out = schtasks(&["/create", "/tn", name, "/xml", &file.to_string_lossy(), "/f"]);
        let _ = std::fs::remove_file(&file);
        return match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => Err(e.to_string()),
        };
    }
    let r = if on {
        let data = wide(&format!("\"{exe}\" --background"));
        // Marked enabled: 2, then the time it was turned off (none).
        let on = [2u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        match unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), 1, data.as_ptr() as _, data.len() as u32 * 2) } {
            0 => unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, approved.as_ptr(), value.as_ptr(), 3, on.as_ptr() as _, on.len() as u32) },
            r => r,
        }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, approved.as_ptr(), value.as_ptr()) };
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) } {
            2 => 0, // wasn't there
            r => r,
        }
    };
    if r == 0 {
        Ok(())
    } else {
        Err(format!("couldn't change the startup entry (error {r})"))
    }
}

/// A sign-in task without the defaults that would stop it on battery or after three days.
fn task_xml(exe: &str) -> String {
    let user = format!("{}\\{}", std::env::var("USERDOMAIN").unwrap_or_default(), std::env::var("USERNAME").unwrap_or_default());
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts {name} in the tray when you sign in.</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>--background</Arguments></Exec></Actions>
</Task>
"#,
        name = edition::get().name,
        user = esc(&user),
        exe = esc(exe),
    )
}
