//! Space: where the disk space went (formerly the separate SpaceLens app). It
//! scans a drive or folder into a size tree and shows it as a folder table with a
//! treemap, the largest files, file types and a cleanup list. Lives beside search
//! as the app's second view; `App` is its whole UI.

// Some helpers are only used by the old standalone bench.
#![allow(dead_code)]

mod analysis;
mod cache;
mod clutter;
mod fmt;
mod live;
mod ntfs;
mod theme;
mod tree;
mod treemap;
mod views;
mod walk;
mod win;

pub use views::App;
