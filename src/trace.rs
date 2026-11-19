//! Timing log for chasing slowness: set SPEED_TRACE to a file path and the app
//! writes what it's doing, with milliseconds since launch. Off otherwise.

use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

pub fn init() {
    START.get_or_init(Instant::now);
    FILE.get_or_init(|| {
        let path = std::env::var_os("SPEED_TRACE")?;
        std::fs::File::create(path).ok().map(Mutex::new)
    });
}

pub fn on() -> bool {
    matches!(FILE.get(), Some(Some(_)))
}

pub fn log(msg: impl AsRef<str>) {
    let Some(Some(f)) = FILE.get() else { return };
    let ms = START.get().map_or(0.0, |s| s.elapsed().as_secs_f64() * 1000.0);
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("?");
    if let Ok(mut f) = f.lock() {
        let _ = writeln!(f, "{ms:9.1} ms  [{name}]  {}", msg.as_ref());
    }
}
