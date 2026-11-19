//! Updates. New versions are published as releases of a public GitHub repo
//! (see release.ps1). The app reads the latest release's `latest.txt`, and on
//! request downloads its own new .exe, checks it against the published SHA-256,
//! swaps it in for the running one and restarts.

use crate::{edition, trace, win};
use eframe::egui;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const REPO: &str = "TheRedViperV/godspeed";
const FIRST_CHECK: Duration = Duration::from_secs(5);
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);

#[link(name = "urlmon")]
extern "system" {
    fn URLDownloadToFileW(caller: *mut c_void, url: *const u16, file: *const u16, reserved: u32, cb: *mut c_void) -> i32;
}

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptHash(alg: usize, secret: *const u8, secret_len: u32, input: *const u8, input_len: u32, out: *mut u8, out_len: u32) -> i32;
}

const BCRYPT_SHA256_ALG_HANDLE: usize = 0x41;

/// This build's version. SPEED_VERSION pretends to be another one (for testing updates).
pub fn version() -> String {
    std::env::var("SPEED_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

#[derive(Clone)]
pub struct Release {
    pub version: String,
    url: String,
    sha256: String,
}

#[derive(Clone)]
pub enum State {
    Idle,
    UpToDate,
    Available(Release),
    Installing,
    Failed(String),
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Downloads through Windows itself, so company proxy settings apply.
fn download(url: &str, to: &Path) -> Result<(), String> {
    let hr = unsafe {
        URLDownloadToFileW(
            std::ptr::null_mut(),
            wide(url).as_ptr(),
            wide(&to.to_string_lossy()).as_ptr(),
            0,
            std::ptr::null_mut(),
        )
    };
    if hr == 0 {
        Ok(())
    } else {
        Err(format!("download failed (0x{:08X})", hr as u32))
    }
}

fn sha256(path: &Path) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut out = [0u8; 32];
    let status = unsafe {
        BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0, data.as_ptr(), data.len() as u32, out.as_mut_ptr(), 32)
    };
    if status != 0 {
        return Err(format!("couldn't check the download (0x{:08X})", status as u32));
    }
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

/// "1.2.3" → (1, 2, 3); anything missing counts as 0.
fn parse_version(v: &str) -> (u32, u32, u32) {
    let mut n = v.trim().trim_start_matches('v').split('.').map(|p| p.trim().parse().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0))
}

fn temp_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}-{}-{}", edition::get().name.to_lowercase(), std::process::id(), name))
}

/// Reads `latest.txt` from the newest release: `key=value` lines with the version
/// and, per app, the download link and its SHA-256.
fn check() -> Result<Option<Release>, String> {
    // A changing query string keeps Windows from answering with a cached copy.
    let url = format!("https://github.com/{REPO}/releases/latest/download/latest.txt?t={}", crate::index::now());
    let file = temp_file("latest.txt");
    download(&url, &file)?;
    let text = std::fs::read_to_string(&file).map_err(|e| e.to_string());
    let _ = std::fs::remove_file(&file);
    let text = text?;
    let get = |key: &str| {
        text.lines().find_map(|l| l.split_once('=').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()))
    };
    let app = edition::get().name.to_lowercase();
    let (Some(version), Some(url), Some(sha256)) = (get("version"), get(&app), get(&format!("{app}_sha256"))) else {
        return Err("the update information is incomplete".into());
    };
    Ok((parse_version(&version) > parse_version(&version_now())).then_some(Release { version, url, sha256 }))
}

fn version_now() -> String {
    version()
}

/// Downloads the new .exe, checks it, puts it in place of the running one and
/// starts it. The caller then closes this copy.
fn install(r: &Release) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("can't tell where the app is")?;
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("app").to_string();
    let fresh = dir.join(format!("{stem}.new.exe"));
    let old = dir.join(format!("{stem}.old.exe"));
    download(&r.url, &fresh)?;
    let got = sha256(&fresh)?;
    if !got.eq_ignore_ascii_case(&r.sha256) {
        let _ = std::fs::remove_file(&fresh);
        return Err("the download didn't match the published checksum, so it wasn't used".into());
    }
    // A running .exe can't be overwritten, but it can be renamed out of the way.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("couldn't replace the app ({e}); is its folder read-only?"))?;
    if let Err(e) = std::fs::rename(&fresh, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("couldn't put the new version in place ({e})"));
    }
    // An elevated Godspeed starts the new one elevated too: no second UAC prompt.
    // --updated: it takes over from this copy rather than handing back to it.
    std::process::Command::new(&exe).args(["--elevated", "--updated"]).spawn().map_err(|e| format!("couldn't restart ({e})"))?;
    Ok(())
}

/// Headless update for testing: `--self-update report.txt` checks, installs if
/// there's something newer, and writes what happened.
pub fn cli(report: &str) {
    let result = match check() {
        Ok(Some(r)) => install(&r).map(|()| format!("updated to {}", r.version)),
        Ok(None) => Ok(format!("already up to date ({})", version())),
        Err(e) => Err(e),
    };
    let _ = std::fs::write(report, format!("{result:?}\n"));
}

/// The copy an update replaced, left behind because it was running at the time.
/// Right after an update that copy may still be closing, so keep trying a while.
pub fn clean_up() {
    let Ok(exe) = std::env::current_exe() else { return };
    let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem().and_then(|s| s.to_str())) else { return };
    let (old, fresh) = (dir.join(format!("{stem}.old.exe")), dir.join(format!("{stem}.new.exe")));
    let _ = std::fs::remove_file(&fresh);
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..40 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

pub struct Updater {
    state: Arc<Mutex<State>>,
}

impl Updater {
    /// Checks a few seconds after launch, then once a day.
    pub fn start(ctx: egui::Context) -> Updater {
        let state = Arc::new(Mutex::new(State::Idle));
        let s = state.clone();
        std::thread::spawn(move || {
            win::low_priority_thread();
            std::thread::sleep(FIRST_CHECK);
            loop {
                let found = check();
                trace::log(format!("update check: {}", match &found {
                    Ok(Some(r)) => format!("{} available", r.version),
                    Ok(None) => "up to date".into(),
                    Err(e) => e.clone(),
                }));
                {
                    let mut st = crate::index::lock(&s);
                    if !matches!(*st, State::Installing) {
                        *st = match found {
                            Ok(Some(r)) => State::Available(r),
                            Ok(None) => State::UpToDate,
                            // Offline, blocked, or nothing published yet: stay quiet.
                            Err(_) => State::Idle,
                        };
                    }
                }
                ctx.request_repaint();
                std::thread::sleep(CHECK_EVERY);
            }
        });
        Updater { state }
    }

    pub fn state(&self) -> State {
        crate::index::lock(&self.state).clone()
    }

    /// Updates in the background; the app closes itself once the new one is running.
    pub fn install(&self, ctx: &egui::Context) {
        let release = match self.state() {
            State::Available(r) => r,
            _ => return,
        };
        *crate::index::lock(&self.state) = State::Installing;
        let (state, ctx) = (self.state.clone(), ctx.clone());
        std::thread::spawn(move || {
            let res = install(&release);
            trace::log(format!("update to {}: {:?}", release.version, res));
            match res {
                Ok(()) => {
                    crate::tray::QUITTING.store(true, std::sync::atomic::Ordering::Relaxed);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Err(e) => *crate::index::lock(&state) = State::Failed(e),
            }
            ctx.request_repaint();
        });
    }
}
