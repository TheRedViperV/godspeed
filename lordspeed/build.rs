//! Embeds the LordSpeed icon (drawn in ../src/icon.rs) in lordspeed.exe.

#[path = "../src/icon.rs"]
mod icon;

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../src/icon.rs");
    let icon = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("lordspeed.ico");
    std::fs::write(&icon, icon::ico(icon::LORD)).unwrap();
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().unwrap());
    res.set("FileDescription", "LordSpeed");
    res.set("ProductName", "LordSpeed");
    res.compile().expect("couldn't compile the Windows resources");
}
