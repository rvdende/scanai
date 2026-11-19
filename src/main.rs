//! scanai: scans whatever is put in the scanner and files it.
//!
//! - `watcher`: scans from the printer panel (and optionally the feeder) and saves every
//!   scan with metadata.
//! - `postprocess`: turns the saved pages upright, OCRs them, and has a local vision
//!   model work out what each document is.
//!
//! The watcher starts post-processing after each scan, so `scanai` on its own does both.

mod postprocess;
mod watcher;

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use clap::{Parser, Subcommand};
use log::{error, info};

#[derive(Parser)]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("SCANAI_GIT_HASH"), ")"), about = "Scan documents as they are loaded into the scanner, then OCR and classify them")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan from the printer panel (and with --watch-feeder, the feeder) and save every scan (the default)
    Watcher(watcher::Cli),
    /// Turn scans upright, OCR them, and extract what they are with a local vision model
    Postprocess(postprocess::Cli),
}

/// `<data>/<name>`, where `<data>` is `$SCANAI_DATA`, or `data` in the current directory.
pub fn data_dir(name: &str) -> PathBuf {
    let data = std::env::var_os("SCANAI_DATA")
        .filter(|d| !d.is_empty())
        .map_or_else(|| PathBuf::from("data"), PathBuf::from);
    std::path::absolute(&data).unwrap_or(data).join(name)
}

/// Addresses that don't belong in the source come from `.env` (see `.env.example`)
/// in or above the current directory. Variables already set in the environment win.
fn load_env() {
    if let Err(e) = dotenvy::dotenv()
        && !e.not_found()
    {
        eprintln!("warning: ignoring .env: {e}");
    }
}

/// Where the log is also written: `$SCANAI_LOG`, or `<data>/scanai.log`.
fn log_path() -> PathBuf {
    std::env::var_os("SCANAI_LOG")
        .filter(|p| !p.is_empty())
        .map_or_else(|| data_dir("scanai.log"), PathBuf::from)
}

/// Logs to stderr as usual, and appends the same lines to [`log_path`].
fn init_logging() {
    // journald adds its own timestamps; the file always gets them.
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let path = log_path();
    let file = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| OpenOptions::new().create(true).append(true).open(&path))
        .map_err(|e| eprintln!("warning: not writing a log file to {}: {e}", path.display()))
        .ok()
        .map(Mutex::new);

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,ort=warn"))
        .format(move |buf, record| {
            let ts = buf.timestamp_seconds();
            let level = record.level();
            let target = record.module_path().unwrap_or_else(|| record.target());
            let msg = record.args();
            if let Some(file) = &file
                && let Ok(mut f) = file.lock()
            {
                // A log line that can't be written to the file shouldn't stop the program.
                let _ = writeln!(f, "[{ts} {level:<5} {target}] {msg}");
            }
            let style = buf.default_level_style(level);
            if journald {
                writeln!(buf, "[{style}{level:<5}{style:#} {target}] {msg}")
            } else {
                writeln!(buf, "[{ts} {style}{level:<5}{style:#} {target}] {msg}")
            }
        })
        .init();
}

fn main() {
    // Before anything reads the environment: .env may also set RUST_LOG and SCANAI_LOG.
    load_env();
    init_logging();

    let mut cli = Cli::parse();
    // A missing subcommand means `watcher watch`; parse again with it spelled out,
    // so `watch` gets its defaults and the options already given still apply.
    let implied: &[&str] = match &cli.cmd {
        None => &["watcher", "watch"],
        Some(Cmd::Watcher(w)) if w.cmd.is_none() => &["watch"],
        _ => &[],
    };
    if !implied.is_empty() {
        cli = Cli::parse_from(std::env::args_os().chain(implied.iter().map(OsString::from)));
    }

    // After parsing, so --help and --version don't log it.
    info!("scanai {} ({})", env!("CARGO_PKG_VERSION"), env!("SCANAI_GIT_HASH"));

    let result = match cli.cmd.expect("set above") {
        Cmd::Watcher(w) => watcher::run(w),
        Cmd::Postprocess(p) => postprocess::run(p),
    };
    if let Err(e) = result {
        error!("{e:#}");
        std::process::exit(1);
    }
}
