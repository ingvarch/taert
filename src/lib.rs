#[cfg(not(target_os = "macos"))]
compile_error!("taert runs on macOS only");

pub mod cli;
pub mod core;
pub mod plain;
pub mod ui;
