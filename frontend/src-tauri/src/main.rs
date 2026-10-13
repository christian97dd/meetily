#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use log;
use env_logger;

/// Past this size the log is rotated to `meetily.log.1` at startup, so it never grows unbounded.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Platform log folder, matching the app identifier (on macOS: ~/Library/Logs/com.meetily.ai).
fn log_dir() -> Option<PathBuf> {
    const APP_ID: &str = "com.meetily.ai";
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        Some(home?.join("Library").join("Logs").join(APP_ID))
    } else if cfg!(target_os = "windows") {
        Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(APP_ID).join("logs"))
    } else {
        let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| Some(home?.join(".local").join("state")))?;
        Some(state.join(APP_ID).join("logs"))
    }
}

fn open_log_file() -> Option<File> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("meetily.log");
    if std::fs::metadata(&path).map_or(false, |m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, dir.join("meetily.log.1"));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Sends every log line to stderr (as before) and to the log file, so problems that happen
/// while nobody is watching the terminal (e.g. a recording that stops) can be diagnosed later.
struct Tee {
    file: File,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        self.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        self.file.flush()
    }
}

fn main() {
    std::env::set_var("RUST_LOG", "info");
    let mut logger = env_logger::Builder::from_default_env();
    if let Some(file) = open_log_file() {
        logger.target(env_logger::Target::Pipe(Box::new(Tee { file })));
    }
    logger.init();

    // Async logger will be initialized lazily when first needed (after Tauri runtime starts)
    log::info!("Starting application...");
    app_lib::run();
}
