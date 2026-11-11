//! scanai: scans whatever is put in the scanner and files it.
//!
//! - `watcher`: watches the scanner's feeder and panel and saves every scan with metadata.
//! - `postprocess`: turns the saved pages upright, OCRs them, and has a local vision
//!   model work out what each document is.
//!
//! The watcher starts post-processing after each scan, so `scanai` on its own does both.

mod postprocess;
mod watcher;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use log::error;

#[derive(Parser)]
#[command(version, about = "Scan documents as they are loaded into the scanner, then OCR and classify them")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Watch the scanner's feeder and panel and save every scan (the default)
    Watcher(watcher::Cli),
    /// Turn scans upright, OCR them, and extract what they are with a local vision model
    Postprocess(postprocess::Cli),
}

/// `data/<name>` next to the source.
pub fn data_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("data").join(name)
}

/// Addresses that don't belong in the source come from `.env` (see `.env.example`):
/// the one in or above the current directory, then the one next to the source.
/// Variables already set in the environment win.
fn load_env() {
    let results = [
        dotenvy::dotenv().map(drop),
        dotenvy::from_path(Path::new(env!("CARGO_MANIFEST_DIR")).join(".env")),
    ];
    for e in results.into_iter().filter_map(Result::err).filter(|e| !e.not_found()) {
        eprintln!("warning: ignoring .env: {e}");
    }
}

fn main() {
    // Before anything reads the environment: .env may also set RUST_LOG.
    load_env();
    let mut log = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,ort=warn"));
    log.format_timestamp_secs();
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        // journald adds its own timestamps.
        log.format_timestamp(None);
    }
    log.init();

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

    let result = match cli.cmd.expect("set above") {
        Cmd::Watcher(w) => watcher::run(w),
        Cmd::Postprocess(p) => postprocess::run(p),
    };
    if let Err(e) = result {
        error!("{e:#}");
        std::process::exit(1);
    }
}
