//! Watches an eSCL scanner's document feeder (and its panel, via WSD) and saves
//! every scan with metadata.

mod escl;
mod firewall;
mod store;
mod wsd;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use clap::{Args, Subcommand, ValueEnum};
use log::{debug, error, info, warn};
use reqwest::StatusCode;

use escl::{ADF_LOADED, Capabilities, CreateJob, DeviceInfo, Escl, NextDocument, Region, ScanSettings};
use store::{JobRecord, PageRecord, PendingScan, ScanRecord, ScanStatus, Trigger};

#[derive(Args)]
pub struct Cli {
    #[command(flatten)]
    opts: Opts,
    /// `None` means `watch`; main fills it in before calling `run`.
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Args)]
struct Opts {
    /// Scanner IP/hostname (HTTPS) or full base URL; normally set in .env
    #[arg(long, global = true, env = "SCANAI_HOST", hide_env_values = true)]
    host: Option<String>,
    /// Where scans are written
    #[arg(long, global = true, default_value_os_t = default_out())]
    out: PathBuf,
    /// Resolution in DPI (eSCL feeder: 100/200/300/600; panel scans over WSD: 100/300)
    #[arg(long, global = true, default_value_t = 300)]
    dpi: u32,
    #[arg(long, global = true, value_enum, default_value_t = Color::Rgb)]
    color: Color,
    /// File format for feeder scans (panel scans are always JPEG)
    #[arg(long, global = true, value_enum, default_value_t = Format::Jpeg)]
    format: Format,
    /// Scan area. `auto` lets the scanner decide.
    #[arg(long, global = true, value_enum, default_value_t = Paper::A4)]
    paper: Paper,
    #[arg(long, global = true, value_enum, default_value_t = Intent::Document)]
    intent: Intent,
    /// eSCL CompressionFactor (device range 1-3); omitted = device default
    #[arg(long, global = true)]
    compression: Option<u32>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Scan whenever paper is loaded in the top feeder or Start is pressed on the panel (the default)
    Watch(WatchOpts),
    /// Scan once right now and exit
    Once {
        #[arg(long, value_enum, default_value_t = Source::Feeder)]
        source: Source,
        /// Protocol to scan with
        #[arg(long, value_enum, default_value_t = Via::Escl)]
        via: Via,
    },
    /// Print scanner details and current status
    Status,
}

#[derive(Args, Clone)]
pub struct WatchOpts {
    /// How often to ask the scanner whether the feeder has paper (ms)
    #[arg(long, default_value_t = 1000)]
    poll_ms: u64,
    /// Wait this long after paper appears before scanning, so you can finish loading the stack (ms)
    #[arg(long, default_value_t = 2000)]
    settle_ms: u64,
    /// Name shown for this computer under Scan → Computer (WSD) on the printer panel
    /// [default: this computer's hostname]
    #[arg(long, default_value_t = hostname(), hide_default_value = true)]
    wsd_name: String,
    /// Port the printer sends panel scan requests to
    #[arg(long, default_value_t = 5309)]
    wsd_port: u16,
    /// Don't register on the printer panel; only watch the feeder
    #[arg(long)]
    no_wsd: bool,
    /// Don't post-process saved scans
    #[arg(long)]
    no_postprocess: bool,
}

/// Runs the post-processor (this program, as `postprocess --raw <out> run`) in the
/// background, one instance at a time, so the OCR models are only loaded while
/// there is work. A request that arrives while it's running makes it run once more
/// afterwards, and each run processes everything still pending, so no scan is missed.
struct PostProcess {
    program: PathBuf,
    raw: PathBuf,
    child: Option<std::process::Child>,
    again: bool,
}

impl PostProcess {
    fn request(&mut self) {
        self.reap();
        if self.child.is_some() {
            self.again = true;
            return;
        }
        match std::process::Command::new(&self.program)
            .arg("postprocess")
            .arg("--raw")
            .arg(&self.raw)
            .arg("run")
            .stdin(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => {
                debug!("started post-processing (pid {})", c.id());
                self.child = Some(c);
            }
            Err(e) => error!("can't start post-processing ({}): {e}", self.program.display()),
        }
    }

    /// Notice a finished run, and start the follow-up run if one was requested.
    fn reap(&mut self) {
        if let Some(c) = &mut self.child {
            match c.try_wait() {
                Ok(None) => return,
                Ok(Some(status)) if !status.success() => warn!("post-processing exited with {status}"),
                _ => {}
            }
            self.child = None;
            if std::mem::take(&mut self.again) {
                self.request();
            }
        }
    }
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Feeder,
    Platen,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Escl,
    Wsd,
}

#[derive(ValueEnum, Clone, Copy)]
enum Color {
    Rgb,
    Gray,
    Bw,
}

#[derive(ValueEnum, Clone, Copy)]
enum Format {
    Jpeg,
    Pdf,
}

#[derive(ValueEnum, Clone, Copy)]
enum Paper {
    A4,
    Letter,
    Legal,
    Max,
    Auto,
}

#[derive(ValueEnum, Clone, Copy)]
enum Intent {
    Document,
    TextAndGraphic,
    Photo,
}

impl Color {
    fn mode(self) -> &'static str {
        match self {
            Color::Rgb => "RGB24",
            Color::Gray => "Grayscale8",
            Color::Bw => "BlackAndWhite1",
        }
    }
}

/// This computer's name without any domain part ("beige", not "beige.local").
fn hostname() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    match name.split('.').next().map(str::trim) {
        Some(short) if !short.is_empty() => short.to_string(),
        _ => "scanai".into(),
    }
}

fn default_out() -> PathBuf {
    crate::data_dir("scanned_raw")
}

pub fn run(cli: Cli) -> Result<()> {
    let host = cli
        .opts
        .host
        .as_deref()
        .filter(|h| !h.trim().is_empty())
        .context("no scanner address: set SCANAI_HOST in .env (see .env.example) or pass --host")?;
    let mut app = App {
        escl: Escl::new(host)?,
        opts: cli.opts,
        caps: None,
        wsd: None,
        post: None,
    };
    match cli.cmd.expect("set in main") {
        Cmd::Watch(w) => app.watch(&w),
        Cmd::Once { source, via } => {
            let st = app.escl.status()?;
            let adf = st.adf_state.as_deref().unwrap_or("(not reported)");
            if st.state != "Idle" {
                bail!("scanner is busy (state {}); try again when it is Idle", st.state);
            }
            if source == Source::Feeder && adf != ADF_LOADED {
                bail!("the top feeder is empty ({adf}); load paper, or use --source platen for the glass");
            }
            let outcome = match via {
                Via::Escl => app.scan(source, Trigger::Manual)?,
                Via::Wsd => app.scan_wsd(None, Some(source), Trigger::Manual)?,
            };
            match outcome {
                Outcome::Saved { .. } => Ok(()),
                Outcome::Rejected(code, body) => {
                    let state = app.escl.status().map(|s| s.state).unwrap_or_default();
                    bail!("scanner refused the job ({code}, scanner state {state}): {body}")
                }
                Outcome::NothingScanned(why) => bail!("nothing was scanned: {why}"),
            }
        }
        Cmd::Status => app.print_status(),
    }
}

struct App {
    escl: Escl,
    opts: Opts,
    caps: Option<Capabilities>,
    wsd: Option<wsd::Runtime>,
    post: Option<PostProcess>,
}

enum Outcome {
    Saved { complete: bool },
    Rejected(StatusCode, String),
    NothingScanned(String),
}

impl App {
    fn caps(&mut self) -> Result<&Capabilities> {
        if self.caps.is_none() {
            self.caps = Some(self.escl.capabilities()?);
        }
        Ok(self.caps.as_ref().unwrap())
    }

    fn print_status(&mut self) -> Result<()> {
        let caps = self.caps()?.clone();
        let d = &caps.device;
        println!("device:   {}", d.make_and_model.as_deref().unwrap_or("?"));
        println!("serial:   {}", d.serial_number.as_deref().unwrap_or("?"));
        println!("uuid:     {}", d.uuid.as_deref().unwrap_or("?"));
        println!("eSCL:     {}", d.escl_version.as_deref().unwrap_or("?"));
        if let Some(a) = &caps.adf {
            println!(
                "feeder:   {}x{} (1/300 in), dpi {:?}, {:?}, justification {}",
                a.max_width,
                a.max_height,
                a.resolutions,
                a.color_modes,
                caps.adf_justification_x.as_deref().unwrap_or("?")
            );
        }
        let st = self.escl.status()?;
        println!("state:    {}", st.state);
        println!("feeder:   {}", st.adf_state.as_deref().unwrap_or("(not reported)"));
        match wsd::Wsd::discover(&self.escl.host()) {
            Ok(w) => println!("WSD:      {} (panel scans: dpi {:?})", w.scan_url, w.platen.map(|p| p.resolutions).unwrap_or_default()),
            Err(e) => println!("WSD:      unavailable ({e:#})"),
        }
        println!("output:   {}", self.opts.out.display());
        Ok(())
    }

    fn settings(&mut self, source: Source) -> Result<(ScanSettings, String)> {
        let (dpi, color, format, paper, intent, compression) = (
            self.opts.dpi,
            self.opts.color,
            self.opts.format,
            self.opts.paper,
            self.opts.intent,
            self.opts.compression,
        );
        let caps = self.caps()?;
        let (input, justification) = match source {
            Source::Feeder => (
                caps.adf.as_ref().context("this scanner has no document feeder")?,
                caps.adf_justification_x.as_deref(),
            ),
            Source::Platen => (caps.platen.as_ref().context("this scanner has no flatbed")?, None),
        };
        let color_mode = color.mode();
        let document_format = match format {
            Format::Jpeg => "image/jpeg",
            Format::Pdf => "application/pdf",
        };
        if !input.resolutions.is_empty() && !input.resolutions.contains(&dpi) {
            bail!("{dpi} dpi not supported here; choose one of {:?}", input.resolutions);
        }
        if !input.color_modes.is_empty() && !input.color_modes.iter().any(|m| m == color_mode) {
            bail!("color mode {color_mode} not supported; device offers {:?}", input.color_modes);
        }
        if !input.formats.is_empty() && !input.formats.iter().any(|f| f == document_format) {
            bail!("format {document_format} not supported; device offers {:?}", input.formats);
        }
        let settings = ScanSettings {
            input_source: source_name(source).into(),
            intent: match intent {
                Intent::Document => "Document",
                Intent::TextAndGraphic => "TextAndGraphic",
                Intent::Photo => "Photo",
            }
            .into(),
            color_mode: color_mode.into(),
            document_format: document_format.into(),
            resolution_dpi: dpi,
            region: (!matches!(paper, Paper::Auto)).then(|| {
                region(
                    paper,
                    300,
                    "escl:ThreeHundredthsOfInches",
                    input.max_width,
                    input.max_height,
                    justification,
                )
            }),
            compression_factor: compression,
        };
        let version = caps.device.escl_version.clone().unwrap_or_else(|| "2.0".into());
        Ok((settings, version))
    }

    /// Scan over eSCL (the feeder watcher and `once`).
    fn scan(&mut self, source: Source, trigger: Trigger) -> Result<Outcome> {
        let started = Local::now();
        let (settings, version) = self.settings(source)?;
        let device = self.caps()?.device.clone();
        let xml = settings.to_xml(&version);
        debug!("ScanSettings:\n{xml}");

        let job = match self.escl.create_job(xml)? {
            CreateJob::Created(url) => url,
            CreateJob::Rejected(code, body) => return Ok(Outcome::Rejected(code, body)),
        };
        info!("scan job started: {}", job.path());

        let mut pending = PendingScan::create(&self.opts.out, started)?;
        let mut pages = Vec::new();
        let mut error: Option<String> = None;
        let mut waiting_since: Option<Instant> = None;
        let mut closed = false;
        loop {
            match self.escl.next_document(&job, Duration::from_secs(300)) {
                Ok(NextDocument::Document { data, content_type }) => {
                    waiting_since = None;
                    let ct = content_type.unwrap_or_else(|| settings.document_format.clone());
                    match pending.write_page(pages.len() + 1, &data, &ct) {
                        Ok(p) => {
                            log_page(&p);
                            pages.push(p);
                        }
                        Err(e) => {
                            error = Some(format!("{e:#}"));
                            break;
                        }
                    }
                    // The flatbed only ever has one page.
                    if source == Source::Platen {
                        break;
                    }
                }
                Ok(NextDocument::NoMore) => {
                    closed = true;
                    break;
                }
                Ok(NextDocument::NotReady) => {
                    if waiting_since.get_or_insert_with(Instant::now).elapsed()
                        > Duration::from_secs(120)
                    {
                        error = Some("timed out waiting for the next page".into());
                        break;
                    }
                    thread::sleep(Duration::from_millis(500));
                }
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    break;
                }
            }
        }

        // A job stays open (and the scanner refuses new ones) until the device has
        // answered "no more pages", so after a flatbed page ask once more to close it.
        if error.is_none() && !closed {
            closed = matches!(
                self.escl.next_document(&job, Duration::from_secs(30)),
                Ok(NextDocument::NoMore)
            );
        }
        if !closed {
            let _ = self.escl.delete_job(&job);
        }
        let job_path = job.path().trim_end_matches('/').to_string();
        let info = self.escl.status().ok().and_then(|s| {
            s.jobs
                .into_iter()
                .find(|j| j.uri.as_deref().map(|u| u.trim_end_matches('/')) == Some(&job_path))
        });
        if error.is_none()
            && let Some(state) = info.as_ref().and_then(|j| j.state.as_deref())
            && matches!(state, "Aborted" | "Canceled")
        {
            error = Some(format!(
                "scanner reports job {state} ({})",
                info.as_ref().unwrap().state_reasons.join(", ")
            ));
        }
        let job_record = JobRecord {
            uri: job_path.clone(),
            uuid: info
                .as_ref()
                .and_then(|j| j.uuid.clone())
                .or_else(|| job_path.rsplit('/').next().map(str::to_owned)),
            id: None,
            final_state: info.as_ref().and_then(|j| j.state.clone()),
            state_reasons: info.as_ref().map(|j| j.state_reasons.clone()).unwrap_or_default(),
            images_completed: info.as_ref().and_then(|j| j.images_completed),
        };
        save(pending, pages, error, started, trigger, device, settings, job_record)
    }

    /// Scan over WSD. With a `scan_identifier` this answers a panel request;
    /// without one it is a scan started from here (`once --via wsd`).
    fn scan_wsd(
        &mut self,
        scan_identifier: Option<&str>,
        source: Option<Source>,
        trigger: Trigger,
    ) -> Result<Outcome> {
        let started = Local::now();
        let (wsd, token) = match (&self.wsd, scan_identifier) {
            (Some(rt), Some(_)) => {
                let (w, t) = rt.current().context("not registered with the printer")?;
                (w, Some(t))
            }
            _ => (wsd::Wsd::discover(&self.escl.host())?, None),
        };
        // A panel request doesn't say which input to use: feed the stack if there is one.
        let source = source.unwrap_or_else(|| match self.escl.status() {
            Ok(s) if s.adf_state.as_deref() == Some(ADF_LOADED) => Source::Feeder,
            _ => Source::Platen,
        });
        let caps = match source {
            Source::Feeder => wsd.adf.clone().context("no feeder over WSD")?,
            Source::Platen => wsd.platen.clone().context("no flatbed over WSD")?,
        };
        let dpi = nearest_dpi(&caps.resolutions, self.opts.dpi);
        let color_mode = self.opts.color.mode();
        let paper = self.opts.paper;
        let device_caps = self.caps()?;
        let justification = match source {
            Source::Feeder => device_caps.adf_justification_x.clone(),
            Source::Platen => None,
        };
        let mut device = device_caps.device.clone();
        device.protocol = "WSD".into();
        let r = region(
            paper,
            1000,
            "ThousandthsOfInches",
            caps.max_width,
            caps.max_height,
            justification.as_deref(),
        );
        let ticket = wsd::Ticket {
            input_source: match source {
                Source::Feeder => "ADF",
                Source::Platen => "Platen",
            },
            color_mode: color_mode.into(),
            dpi,
            x_offset: r.x_offset,
            y_offset: r.y_offset,
            width: r.width,
            height: r.height,
        };
        let settings = ScanSettings {
            input_source: source_name(source).into(),
            intent: "Text".into(),
            color_mode: color_mode.into(),
            document_format: "image/jpeg".into(),
            resolution_dpi: dpi,
            region: Some(r),
            compression_factor: None,
        };

        let job = wsd.create_scan_job(scan_identifier, token.as_deref(), &ticket)?;
        info!("WSD scan job {} started ({}, {dpi} dpi)", job.id, settings.input_source);
        let mut pending = PendingScan::create(&self.opts.out, started)?;
        let mut pages = Vec::new();
        let mut error = None;
        for n in 1.. {
            match wsd.retrieve_image(&job, n) {
                Ok(Some(img)) => match pending.write_page(n, &img.data, &img.content_type) {
                    Ok(p) => {
                        log_page(&p);
                        pages.push(p);
                    }
                    Err(e) => {
                        error = Some(format!("{e:#}"));
                        break;
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    break;
                }
            }
        }
        let job_record = JobRecord {
            uri: wsd.scan_url.clone(),
            uuid: None,
            id: Some(job.id.clone()),
            final_state: None,
            state_reasons: Vec::new(),
            images_completed: Some(pages.len() as u32),
        };
        save(pending, pages, error, started, trigger, device, settings, job_record)
    }

    fn watch(&mut self, w: &WatchOpts) -> Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            ctrlc::set_handler(move || {
                if stop.swap(true, Ordering::SeqCst) {
                    std::process::exit(130);
                }
                info!("stopping (a scan in progress will finish first; press again to force)");
            })?;
        }
        std::fs::create_dir_all(&self.opts.out)
            .with_context(|| format!("creating {}", self.opts.out.display()))?;
        for p in store::leftovers(&self.opts.out) {
            warn!("unfinished scan from an earlier run: {}", p.display());
        }
        info!("saving scans to {}", self.opts.out.display());

        // Wait for the scanner to answer before doing anything else.
        loop {
            match self.caps() {
                Ok(c) => {
                    let d = &c.device;
                    info!(
                        "scanner: {} (serial {}) at {}",
                        d.make_and_model.as_deref().unwrap_or("?"),
                        d.serial_number.as_deref().unwrap_or("?"),
                        d.host
                    );
                    if c.adf.is_none() {
                        bail!("this scanner has no document feeder");
                    }
                    break;
                }
                Err(e) => warn!("scanner not reachable yet: {e:#}"),
            }
            if sleep(&stop, Duration::from_secs(10)) {
                return Ok(());
            }
        }

        if !w.no_wsd {
            match wsd::Runtime::start(&self.escl.host(), w.wsd_port, &w.wsd_name) {
                Ok(rt) => {
                    self.wsd = Some(rt);
                    firewall::check(&self.escl.host(), w.wsd_port);
                }
                Err(e) => warn!("panel scanning (WSD) is off: {e:#}"),
            }
        }

        let poll = Duration::from_millis(w.poll_ms);
        let settle = Duration::from_millis(w.settle_ms);
        // Scan when paper is loaded, then wait for the feeder to empty before scanning again,
        // so a jammed or refused stack isn't rescanned in a loop.
        let mut armed = true;
        let mut last_adf: Option<String> = None;
        let mut failures = 0u32;
        if w.no_postprocess {
            info!("post-processing is off");
        } else {
            match std::env::current_exe() {
                Ok(program) => {
                    info!("post-processing new scans");
                    self.post = Some(PostProcess {
                        program,
                        raw: self.opts.out.clone(),
                        child: None,
                        again: false,
                    });
                    // Catch up on anything saved while we weren't running.
                    self.postprocess();
                }
                Err(e) => warn!("post-processing is off: can't find this program's own path: {e}"),
            }
        }
        info!("watching the document feeder (poll {poll:?}, settle {settle:?})");

        while !stop.load(Ordering::SeqCst) {
            let st = match self.escl.status() {
                Ok(st) => {
                    if failures >= 3 {
                        info!("scanner reachable again");
                    }
                    failures = 0;
                    st
                }
                Err(e) => {
                    failures += 1;
                    if failures == 3 {
                        warn!("scanner not responding: {e:#}");
                    } else {
                        debug!("status poll failed: {e:#}");
                    }
                    let backoff = if failures >= 3 { Duration::from_secs(5) } else { poll };
                    if self.idle(&stop, backoff) {
                        break;
                    }
                    continue;
                }
            };

            if st.adf_state != last_adf {
                info!("feeder: {}", st.adf_state.as_deref().unwrap_or("(not reported)"));
                last_adf = st.adf_state.clone();
            }
            let loaded = st.adf_state.as_deref() == Some(ADF_LOADED);
            if !loaded {
                armed = true;
            } else if armed && st.state == "Idle" {
                info!("paper detected in the feeder; scanning in {settle:?}");
                if self.idle(&stop, settle) {
                    break;
                }
                match self.escl.status() {
                    Ok(s) if s.adf_state.as_deref() == Some(ADF_LOADED) && s.state == "Idle" => {}
                    Ok(_) => {
                        info!("feeder changed before the scan started; waiting");
                        continue;
                    }
                    Err(e) => {
                        debug!("status poll failed: {e:#}");
                        continue;
                    }
                }
                match self.scan(Source::Feeder, Trigger::FeederLoaded) {
                    // A clean run empties the feeder, so paper seen afterwards is a new stack.
                    Ok(Outcome::Saved { complete }) => {
                        self.postprocess();
                        if !complete {
                            armed = false;
                        }
                    }
                    Ok(Outcome::Rejected(StatusCode::SERVICE_UNAVAILABLE, _)) => {
                        info!("scanner busy; will retry");
                        if self.idle(&stop, Duration::from_secs(5)) {
                            break;
                        }
                        continue;
                    }
                    Ok(Outcome::Rejected(code, body)) => {
                        error!("scanner refused the job ({code}): {body}");
                        armed = false;
                    }
                    Ok(Outcome::NothingScanned(why)) => {
                        warn!("nothing was scanned: {why}");
                        armed = false;
                    }
                    Err(e) => {
                        error!("scan failed: {e:#}");
                        armed = false;
                    }
                }
                if !armed {
                    info!("remove and reload the paper to scan again");
                }
                continue;
            }
            if self.idle(&stop, poll) {
                break;
            }
        }
        if let Some(rt) = self.wsd.take() {
            rt.stop();
        }
        info!("stopped");
        Ok(())
    }

    /// Wait up to `d`, handling panel scan requests as they arrive.
    /// Returns true if a stop was requested.
    fn idle(&mut self, stop: &AtomicBool, d: Duration) -> bool {
        let end = Instant::now() + d;
        loop {
            if stop.load(Ordering::SeqCst) {
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            let slice = (end - now).min(Duration::from_millis(100));
            if let Some(p) = &mut self.post {
                p.reap();
            }
            let event = match &self.wsd {
                Some(rt) => rt.events.recv_timeout(slice).ok(),
                None => {
                    thread::sleep(slice);
                    None
                }
            };
            if let Some(ev) = event {
                self.on_wsd_event(ev);
            }
        }
    }

    fn postprocess(&mut self) {
        if let Some(p) = &mut self.post {
            p.request();
        }
    }

    fn on_wsd_event(&mut self, ev: wsd::Event) {
        match ev {
            wsd::Event::ScanAvailable { scan_identifier } => {
                info!("scan requested from the printer panel");
                match self.scan_wsd(Some(&scan_identifier), None, Trigger::PanelButton) {
                    Ok(Outcome::Saved { .. }) => self.postprocess(),
                    Ok(Outcome::NothingScanned(why)) => warn!("nothing was scanned: {why}"),
                    Ok(Outcome::Rejected(code, body)) => error!("scanner refused the job ({code}): {body}"),
                    Err(e) => error!("panel scan failed: {e:#}"),
                }
            }
            wsd::Event::SubscriptionEnded => info!("the printer ended the WSD registration"),
        }
        // The printer may drop the registration after a request; make sure we're still listed.
        if let Some(rt) = &self.wsd {
            rt.check_now();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn save(
    pending: PendingScan,
    pages: Vec<PageRecord>,
    error: Option<String>,
    started: DateTime<Local>,
    trigger: Trigger,
    device: DeviceInfo,
    settings: ScanSettings,
    job: JobRecord,
) -> Result<Outcome> {
    if pages.is_empty() {
        pending.discard();
        return Ok(Outcome::NothingScanned(
            error.unwrap_or_else(|| "the scanner returned no pages".into()),
        ));
    }
    let complete = error.is_none();
    let record = ScanRecord {
        id: pending.id.clone(),
        status: if complete { ScanStatus::Complete } else { ScanStatus::Partial },
        trigger,
        scanned_at: started,
        finished_at: Local::now(),
        device,
        settings,
        job,
        page_count: pages.len(),
        pages,
        error: error.clone(),
        software: concat!("scanai ", env!("CARGO_PKG_VERSION")).into(),
    };
    let ron = pending.finish(&record)?;
    if let Some(e) = error {
        warn!(
            "saved {} page(s), metadata {}, but the scan did not finish cleanly: {e}",
            record.page_count,
            ron.display()
        );
    } else {
        info!("saved {} page(s), metadata {}", record.page_count, ron.display());
    }
    Ok(Outcome::Saved { complete })
}

fn log_page(p: &PageRecord) {
    info!(
        "  page {}: {} ({} KiB{})",
        p.index,
        p.file,
        p.bytes / 1024,
        match (p.width_px, p.height_px) {
            (Some(w), Some(h)) => format!(", {w}x{h} px"),
            _ => String::new(),
        }
    );
}

fn source_name(s: Source) -> &'static str {
    match s {
        Source::Feeder => "Feeder",
        Source::Platen => "Platen",
    }
}

/// The highest supported resolution not above `want`, else the lowest supported.
fn nearest_dpi(supported: &[u32], want: u32) -> u32 {
    let below = supported.iter().copied().filter(|&d| d <= want).max();
    below.or_else(|| supported.iter().copied().min()).unwrap_or(want)
}

/// Paper sizes in 1/1000 inch.
fn paper_mils(paper: Paper) -> Option<(&'static str, u32, u32)> {
    match paper {
        Paper::A4 => Some(("A4", 8268, 11693)),
        Paper::Letter => Some(("Letter", 8500, 11000)),
        Paper::Legal => Some(("Legal", 8500, 14000)),
        Paper::Max | Paper::Auto => None,
    }
}

/// Scan region in units of 1/`per_inch` inch (eSCL: 300, WSD: 1000). A page narrower
/// than the feeder sits against the side the device reports in `Justification`, so
/// offset the region to match.
fn region(
    paper: Paper,
    per_inch: u32,
    units: &str,
    max_w: u32,
    max_h: u32,
    justification_x: Option<&str>,
) -> Region {
    let (name, w, h) = match paper_mils(paper) {
        Some((name, w, h)) => (name, (w * per_inch + 500) / 1000, (h * per_inch + 500) / 1000),
        None => ("Max", max_w, max_h),
    };
    let (w, h) = (w.min(max_w), h.min(max_h));
    let x_offset = match justification_x {
        Some("Right") => max_w - w,
        Some("Center") => (max_w - w) / 2,
        _ => 0,
    };
    Region {
        paper: name.into(),
        units: units.into(),
        x_offset,
        y_offset: 0,
        width: w,
        height: h,
    }
}

/// Sleep in short steps; returns true if a stop was requested.
fn sleep(stop: &AtomicBool, d: Duration) -> bool {
    let end = Instant::now() + d;
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= end {
            return false;
        }
        thread::sleep((end - now).min(Duration::from_millis(100)));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a4_regions() {
        let r = region(Paper::A4, 300, "u", 2550, 4200, Some("Right"));
        assert_eq!((r.x_offset, r.width, r.height), (70, 2480, 3508));
        let r = region(Paper::A4, 1000, "u", 8500, 11700, None);
        assert_eq!((r.x_offset, r.width, r.height), (0, 8268, 11693));
    }

    #[test]
    fn dpi_choice() {
        assert_eq!(nearest_dpi(&[100, 300], 300), 300);
        assert_eq!(nearest_dpi(&[100, 300], 600), 300);
        assert_eq!(nearest_dpi(&[100, 300], 50), 100);
        assert_eq!(nearest_dpi(&[], 200), 200);
    }
}
