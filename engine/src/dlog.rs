//! Engine-wide trace channel for the app's debug.log.
//!
//! The engine does the actual file/network work, so "which file is it on
//! right now" lines must originate here. The app installs a sink (the open
//! debug.log) for the duration of a sync; every line also goes through
//! mini-log, so `LOG_LEVEL=DEBUG` streams the same trace to the console in
//! dev runs. Message closures are lazy — with no sink and no console level,
//! a trace call costs one atomic load.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub use mini_log::Level;

type Sink = Box<dyn Fn(&str) + Send + Sync>;

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static SINK_SET: AtomicBool = AtomicBool::new(false);

/// Install (or clear) the line sink. Lines arrive pre-formatted:
/// `[timestamp] - LEVEL - message`.
pub fn set_sink(sink: Option<Sink>) {
    SINK_SET.store(sink.is_some(), Ordering::Relaxed);
    *SINK.lock().unwrap() = sink;
}

fn active(level: Level) -> bool {
    SINK_SET.load(Ordering::Relaxed) || mini_log::is_enabled(level)
}

fn emit(level: Level, msg: String) {
    // mini-log prints to the console when LOG_LEVEL allows and gives us the
    // canonical formatted line for the file sink.
    let line = mini_log::LogMessage::new(level, msg);
    if SINK_SET.load(Ordering::Relaxed) {
        if let Some(sink) = SINK.lock().unwrap().as_ref() {
            sink(&line.to_string());
        }
    }
}

pub fn debug(msg: impl FnOnce() -> String) {
    if active(Level::Debug) {
        emit(Level::Debug, msg());
    }
}

pub fn info(msg: impl FnOnce() -> String) {
    if active(Level::Info) {
        emit(Level::Info, msg());
    }
}

pub fn warn(msg: impl FnOnce() -> String) {
    if active(Level::Warning) {
        emit(Level::Warning, msg());
    }
}

pub fn error(msg: impl FnOnce() -> String) {
    if active(Level::Error) {
        emit(Level::Error, msg());
    }
}
