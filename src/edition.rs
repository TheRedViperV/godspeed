//! Godspeed and LordSpeed are the same app built two ways. Godspeed asks for admin
//! rights to read the NTFS file table (fastest); LordSpeed never does, for work and
//! client PCs where admin isn't available.

use crate::icon::Palette;
use std::path::PathBuf;
use std::sync::OnceLock;

pub struct Edition {
    pub name: &'static str,
    /// Ask for admin rights (one UAC prompt) at launch.
    pub admin: bool,
    pub palette: Palette,
}

pub static GODSPEED: Edition = Edition { name: "Godspeed", admin: true, palette: crate::icon::GOD };
pub static LORDSPEED: Edition = Edition { name: "LordSpeed", admin: false, palette: crate::icon::LORD };

static CURRENT: OnceLock<&'static Edition> = OnceLock::new();

pub fn set(e: &'static Edition) {
    let _ = CURRENT.set(e);
}

pub fn get() -> &'static Edition {
    CURRENT.get().copied().unwrap_or(&GODSPEED)
}

/// %LOCALAPPDATA%\<name>: history and the saved index live here.
pub fn data_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(get().name))
}
