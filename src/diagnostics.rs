//! Persistent logs, crash reports and visible startup failures.
//!
//! Release builds on Windows have no console, and desktop launches on Linux and
//! macOS discard stderr, so diagnostics are also written under the data directory.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const LOG_FILE: &str = "nutty.log";
/// The active log rotates once at startup when it exceeds this size; one older file is kept.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// Only our crates log informational events by default; dependencies log warnings.
const DEFAULT_FILTER: &str =
    "warn,Nutty=info,nutty=info,nutty_core=info,nutty_ssh=info,nutty_terminal=info,nutty_ai=info";

static LOG_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn log_directory(data: &Path) -> PathBuf {
    data.join("logs")
}

/// Sends events to stderr and, when the log directory is writable, to a bounded file.
/// `RUST_LOG` overrides the default filter.
pub(crate) fn init_logging(data: &Path) {
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let directory = log_directory(data);
    let file = open_log(&directory).ok();
    if file.is_some() {
        let _ = LOG_DIRECTORY.set(directory);
    }
    let file_layer = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(Mutex::new(file))
    });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .try_init();
}

fn open_log(directory: &Path) -> std::io::Result<fs::File> {
    crate::config::create_private_dir(directory)?;
    let path = directory.join(LOG_FILE);
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES) {
        let _ = fs::rename(&path, directory.join(format!("{LOG_FILE}.1")));
    }
    private_options().append(true).open(path)
}

fn private_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

/// Records panics with a backtrace in the log and a separate crash report, then
/// keeps the default hook so stderr output and abort behavior are unchanged.
pub(crate) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%thread, %location, %message, "panic");
        if let Some(directory) = LOG_DIRECTORY.get() {
            let report = format!(
                "Nutty {} panicked\nthread: {thread}\nlocation: {location}\nmessage: {message}\n\n{backtrace}\n",
                env!("CARGO_PKG_VERSION")
            );
            let seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default();
            let path = directory.join(format!("crash-{seconds}-{}.log", std::process::id()));
            if let Ok(mut file) = private_options().write(true).open(path) {
                let _ = file.write_all(report.as_bytes());
                let _ = file.sync_all();
            }
        }
        previous(info);
    }));
}

/// Logs a fatal startup error and shows it where the user can see it.
pub(crate) fn report_startup_failure(error: &anyhow::Error) {
    tracing::error!(error = format!("{error:#}"), "Nutty failed to start");
    let mut text = format!("{error:#}");
    if let Some(directory) = LOG_DIRECTORY.get() {
        text.push_str(&format!("\n\nLog: {}", directory.join(LOG_FILE).display()));
    }
    show_error("Nutty", &text);
}

#[cfg(windows)]
fn show_error(title: &str, text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |value: &str| -> Vec<u16> {
        value
            .encode_utf16()
            .map(|unit| if unit == 0 { u16::from(b' ') } else { unit })
            .chain(Some(0))
            .collect()
    };
    let (title, text) = (wide(title), wide(text));
    // SAFETY: Both buffers are NUL-terminated UTF-16 and outlive the modal call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_error(_: &str, text: &str) {
    // A terminal launch already shows the error that `main` returns; desktop
    // launches rely on the log written above.
    let _ = text;
}
