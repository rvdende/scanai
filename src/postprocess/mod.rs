//! Post-processes scans from the watcher: turns pages upright, OCRs them, and has a
//! local vision model classify the document and extract dates, parties, addresses,
//! amounts and reference numbers.
//!
//! Input:  <raw>/<id>.ron + <id>_pNNN.jpg   (written by the watcher)
//! Output: <out>/<id>_pNNN.jpg  upright page
//!         <out>/<id>_pNNN.txt  OCR text
//!         <out>/<id>.ron       everything else; written last, so a scan is done once it exists

mod extract;
mod pieces;
mod stitch;
mod vision;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use clap::{Args, Subcommand};
use image::codecs::jpeg::JpegEncoder;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};

use extract::{Extraction, Extractor, Known, RunInfo};
use vision::Vision;

#[derive(Args)]
pub struct Cli {
    /// Where the watcher writes scans
    #[arg(long, global = true, default_value_os_t = crate::data_dir("scanned_raw"))]
    raw: PathBuf,
    /// Where processed scans go
    #[arg(long, global = true, default_value_os_t = crate::data_dir("processed"))]
    out: PathBuf,
    /// Ollama server; can be set in .env
    #[arg(long, global = true, env = "SCANAI_OLLAMA", hide_env_values = true, default_value = "http://localhost:11434")]
    ollama: String,
    /// Ollama vision model
    #[arg(long, global = true, default_value = "qwen3-vl:8b")]
    model: String,
    /// Stitch a scan onto the previous one when they overlap and were scanned this close together
    #[arg(long, global = true, default_value_t = 10)]
    stitch_window_mins: i64,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Process new scans as they arrive (the default)
    Watch {
        #[arg(long, default_value_t = 5)]
        interval_secs: u64,
    },
    /// Process the given scan ids (default: everything not processed yet) and exit
    Run {
        ids: Vec<String>,
        /// Redo scans that were already processed
        #[arg(long)]
        force: bool,
    },
    /// Show how scans would be split into separate pieces of paper (no processing)
    Pieces {
        ids: Vec<String>,
        /// Also write each paper mask as a PNG here
        #[arg(long)]
        masks: Option<PathBuf>,
    },
}

/// The parts of the watcher's record we need.
#[derive(Deserialize)]
struct RawScan {
    id: String,
    scanned_at: String,
    device: RawDevice,
    pages: Vec<RawPage>,
}

#[derive(Deserialize)]
struct RawDevice {
    make_and_model: Option<String>,
}

#[derive(Deserialize)]
struct RawPage {
    index: usize,
    file: String,
    content_type: String,
}

#[derive(Serialize)]
struct Processed {
    id: String,
    /// Raw scans this was made from: more than one when scans were stitched together.
    sources: Vec<String>,
    stitch: Option<StitchOut>,
    /// Set when this was one of several pieces of paper scanned together.
    #[serde(skip_serializing_if = "Option::is_none")]
    split: Option<SplitOut>,
    /// The watcher's metadata for each source scan.
    raw_metadata: Vec<String>,
    scanned_at: String,
    device: Option<String>,
    processed_at: DateTime<Local>,
    pages: Vec<PageOut>,
    extraction: Extraction,
    extraction_run: RunInfo,
    software: String,
}

#[derive(Serialize)]
struct PageOut {
    index: usize,
    source_files: Vec<String>,
    file: String,
    text_file: String,
    /// Degrees the page was turned clockwise to make it upright.
    rotated_cw: u32,
    orientation_score: f32,
    /// Degrees the page was then turned counter-clockwise to straighten crooked text.
    #[serde(skip_serializing_if = "Option::is_none")]
    deskewed_deg: Option<f32>,
    /// Part of the upright scan kept (x, y, width, height), when the paper covered
    /// only a small part of the glass.
    #[serde(skip_serializing_if = "Option::is_none")]
    cropped_to: Option<(u32, u32, u32, u32)>,
    width_px: u32,
    height_px: u32,
    ocr_lines: usize,
    ocr_mean_confidence: f32,
}

#[derive(Serialize)]
struct SplitOut {
    /// Which piece of the scan this is (a, b, ...), largest first.
    piece: String,
    of: usize,
    /// Where it lay in the raw scan: (x, y, width, height) pixels.
    region: (u32, u32, u32, u32),
    /// The records made from the other pieces.
    siblings: Vec<String>,
}

#[derive(Serialize)]
struct StitchOut {
    /// How the later scan was turned and moved to line up with the earlier one.
    rotation_deg: f32,
    shift_px: (i64, i64),
    matched_lines: usize,
    overlap_agreement: f32,
    /// The later scan added nothing new, so the earlier image was kept.
    duplicate: bool,
}

/// A page on its way through the pipeline.
struct Page {
    source_files: Vec<String>,
    rotated_cw: u32,
    orientation_score: f32,
    deskewed_deg: Option<f32>,
    /// Save by copying this raw file (unrotated JPEG) instead of re-encoding.
    copy_from: Option<PathBuf>,
    cropped_to: Option<(u32, u32, u32, u32)>,
    upright: image::RgbImage,
    lines: Vec<vision::Line>,
}

/// The parts of a processed record needed for filing and stitching.
#[derive(Deserialize)]
struct Filed {
    #[serde(default)]
    id: String,
    #[serde(default)]
    sources: Vec<String>,
    scanned_at: String,
    pages: Vec<FiledPage>,
    extraction: FiledAs,
}

#[derive(Deserialize)]
struct FiledPage {
    file: String,
    #[serde(default)]
    source_files: Vec<String>,
    #[serde(default)]
    source_file: Option<String>,
}

impl FiledPage {
    fn source_files(&self) -> Vec<String> {
        if self.source_files.is_empty() {
            self.source_file.iter().cloned().collect()
        } else {
            self.source_files.clone()
        }
    }
}

#[derive(Deserialize)]
struct FiledAs {
    doc_type: String,
    doc_type_detail: String,
    #[serde(default)]
    parties: Vec<extract::Party>,
    #[serde(default)]
    tags: Vec<String>,
}

/// Scan ids are local start times: YYYY-MM-DD_HH-MM-SS, maybe with a _N suffix.
fn scan_time(id: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(id.get(..19)?, "%Y-%m-%d_%H-%M-%S").ok()
}

struct Pipeline {
    raw: PathBuf,
    out: PathBuf,
    stitch_window: chrono::TimeDelta,
    vision: Vision,
    extractor: Extractor,
}

impl Pipeline {
    fn pending(&self) -> Result<Vec<String>> {
        let merged: std::collections::HashSet<String> =
            self.records().into_iter().flat_map(|(_, f)| f.sources).collect();
        let mut ids: Vec<String> = fs::read_dir(&self.raw)
            .with_context(|| format!("reading {}", self.raw.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let id = name.strip_suffix(".ron")?;
                (!id.starts_with('.') && !self.out.join(&name).exists() && !merged.contains(id))
                    .then(|| id.to_string())
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Types and tags used on other processed scans, most used first.
    fn known(&self, except_id: &str, except_sources: &[String]) -> Known {
        let mut tags: HashMap<String, usize> = HashMap::new();
        let mut types: HashMap<String, (usize, Vec<String>)> = HashMap::new();
        let mut by_detail = HashMap::new();
        for (id, f) in self.records() {
            if id == except_id || except_sources.contains(&id) {
                continue;
            }
            let f = f.extraction;
            by_detail.insert(
                extract::filing_key(&f.doc_type_detail, &f.parties),
                (f.doc_type.clone(), f.tags.clone()),
            );
            for tag in f.tags {
                *tags.entry(tag).or_default() += 1;
            }
            let t = types.entry(f.doc_type).or_default();
            t.0 += 1;
            if t.1.len() < 3 && !t.1.contains(&f.doc_type_detail) {
                t.1.push(f.doc_type_detail);
            }
        }
        let mut tags: Vec<(String, usize)> = tags.into_iter().collect();
        tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut types: Vec<(String, (usize, Vec<String>))> = types.into_iter().collect();
        types.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));
        Known {
            tags: tags.into_iter().map(|(t, _)| t).collect(),
            types: types.into_iter().map(|(t, (_, ex))| (t, ex)).collect(),
            by_detail,
        }
    }

    fn process(&self, id: &str) -> Result<PathBuf> {
        let started = Instant::now();
        let raw_path = self.raw.join(format!("{id}.ron"));
        let raw: RawScan = ron::from_str(&fs::read_to_string(&raw_path)?)
            .with_context(|| format!("parsing {}", raw_path.display()))?;
        fs::create_dir_all(&self.out)?;

        let mut pages: Vec<Page> = Vec::new();
        for p in &raw.pages {
            if !p.content_type.starts_with("image/") {
                warn!("{id}: skipping {} ({})", p.file, p.content_type);
                continue;
            }
            let src = self.raw.join(&p.file);
            let img = image::open(&src).with_context(|| format!("opening {}", src.display()))?.to_rgb8();
            // Several pieces of paper laid on the glass together: file each on its own.
            if raw.pages.len() == 1 {
                let found = pieces::find(&img);
                if found.len() >= 2 {
                    return self.process_pieces(started, &raw, &p.file, &img, found);
                }
            }
            let copyable = p.content_type == "image/jpeg";
            pages.push(self.prepare(id, p.index, img, &p.file, copyable.then_some(src))?);
        }
        self.remove_pieces(id)?;
        if pages.is_empty() {
            bail!("no image pages");
        }

        // A single-page scan that continues the previous one (same paper, overlapping)
        // is stitched onto it and filed under the earlier scan's id.
        let mut target = (raw.id.clone(), vec![raw.id.clone()], raw.scanned_at.clone());
        let mut stitch_note = None;
        if pages.len() == 1
            && let Some(prev) = self.previous(&raw.id)
        {
            if prev.sources.contains(&raw.id) {
                info!("{id} is already part of {}; reprocess that instead", prev.id);
                return Ok(self.out.join(format!("{}.ron", prev.id)));
            }
            let prev_img = image::open(self.out.join(&prev.pages[0].file))?.to_rgb8();
            let prev_lines = self.vision.ocr(&prev_img)?;
            if let Some(al) = stitch::align(&prev_lines, &pages[0].lines) {
                let mut sources = prev.sources.clone();
                sources.push(raw.id.clone());
                let mut source_files = prev.pages[0].source_files();
                source_files.extend(pages[0].source_files.clone());
                let page = if al.duplicate {
                    info!("{id}: same part of the paper as {}; keeping that scan", prev.id);
                    Page { source_files, copy_from: None, cropped_to: None, upright: prev_img, lines: prev_lines, ..pages.remove(0) }
                } else {
                    let joined = stitch::compose(&prev_img, &pages[0].upright, &al, &prev_lines, &pages[0].lines);
                    let lines = self.vision.ocr(&joined)?;
                    info!(
                        "{id}: continues {} ({} matching lines, {:.0}% of the overlap); stitched to {}x{} px",
                        prev.id,
                        al.matched,
                        al.agreement * 100.0,
                        joined.width(),
                        joined.height()
                    );
                    Page { source_files, copy_from: None, cropped_to: None, upright: joined, lines, ..pages.remove(0) }
                };
                stitch_note = Some(StitchOut {
                    rotation_deg: al.transform.angle.to_degrees(),
                    shift_px: (al.transform.tx.round() as i64, al.transform.ty.round() as i64),
                    matched_lines: al.matched,
                    overlap_agreement: al.agreement,
                    duplicate: al.duplicate,
                });
                pages = vec![page];
                target = (prev.id.clone(), sources, prev.scanned_at.clone());
            }
        }
        let (out_id, sources, scanned_at) = target;
        if out_id != raw.id {
            self.remove_outputs(&raw.id)?;
        }
        let device = raw.device.make_and_model.clone();
        self.write_record(started, &out_id, sources, scanned_at, pages, stitch_note, None, device)
    }

    /// Save the pages, extract from them, and write the record.
    #[allow(clippy::too_many_arguments)]
    fn write_record(
        &self,
        started: Instant,
        out_id: &str,
        sources: Vec<String>,
        scanned_at: String,
        pages: Vec<Page>,
        stitch_note: Option<StitchOut>,
        split: Option<SplitOut>,
        device: Option<String>,
    ) -> Result<PathBuf> {

        let mut outs = Vec::new();
        let mut model_images = Vec::new();
        let mut texts = Vec::new();
        for (i, p) in pages.into_iter().enumerate() {
            let stem = format!("{out_id}_p{:03}", i + 1);
            let file = format!("{stem}.jpg");
            match &p.copy_from {
                Some(src) => {
                    fs::copy(src, self.out.join(&file))?;
                }
                None => {
                    let mut w = std::io::BufWriter::new(fs::File::create(self.out.join(&file))?);
                    JpegEncoder::new_with_quality(&mut w, 95).encode_image(&p.upright)?;
                }
            }
            let text = vision::as_text(&p.lines);
            let text_file = format!("{stem}.txt");
            fs::write(self.out.join(&text_file), &text)?;
            let mean = if p.lines.is_empty() {
                0.0
            } else {
                p.lines.iter().map(|l| l.confidence).sum::<f32>() / p.lines.len() as f32
            };
            outs.push(PageOut {
                index: i + 1,
                source_files: p.source_files,
                file,
                text_file,
                rotated_cw: p.rotated_cw,
                orientation_score: p.orientation_score,
                deskewed_deg: p.deskewed_deg,
                cropped_to: p.cropped_to,
                width_px: p.upright.width(),
                height_px: p.upright.height(),
                ocr_lines: p.lines.len(),
                ocr_mean_confidence: mean,
            });
            model_images.push(stitch::crop_to_text(&p.upright, &p.lines, 60.0));
            texts.push(text);
        }

        let known = self.known(&out_id, &sources);
        let (extraction, run) = self.extractor.extract(&model_images, &texts, &known)?;
        info!(
            "{out_id}: {} — {} ({:.1}s)",
            extraction.doc_type, extraction.doc_type_detail, run.seconds
        );
        let record = Processed {
            id: out_id.to_string(),
            sources: sources.clone(),
            stitch: stitch_note,
            split,
            raw_metadata: sources
                .iter()
                .map(|s| self.raw.join(format!("{s}.ron")).display().to_string())
                .collect(),
            scanned_at,
            device,
            processed_at: Local::now(),
            pages: outs,
            extraction,
            extraction_run: run,
            software: concat!("scanai ", env!("CARGO_PKG_VERSION")).into(),
        };
        let ron = ron::ser::to_string_pretty(&record, ron::ser::PrettyConfig::default())? + "\n";
        let tmp = self.out.join(format!(".{out_id}.ron.tmp"));
        let dst = self.out.join(format!("{out_id}.ron"));
        fs::write(&tmp, ron)?;
        fs::rename(&tmp, &dst)?;
        info!("{out_id}: done in {:.1}s → {}", started.elapsed().as_secs_f32(), dst.display());
        Ok(dst)
    }

    /// The most recent processed scan before `id`, if its last part was scanned within
    /// Turn a page upright, straighten it, read it, and crop away empty glass.
    /// `copy_from`: the raw JPEG, which is reused as is if nothing needed changing.
    fn prepare(&self, id: &str, index: usize, img: image::RgbImage, file: &str, copy_from: Option<PathBuf>) -> Result<Page> {
        let o = self.vision.orient(&img)?;
        let mut upright = vision::rotate_cw(img, o.rotated_cw);
        let mut lines = self.vision.ocr(&upright)?;
        // Paper put down a little crooked: straighten it, then read it again.
        let skew = vision::skew(&lines).filter(|a| a.abs() >= 0.3);
        if let Some(a) = skew {
            upright = vision::deskew(&upright, a);
            lines = self.vision.ocr(&upright)?;
        }
        // A slip or card on a mostly empty page (text on under two thirds of it): keep
        // just the part with text.
        // The crop covers all the ink (drawings, stamps) as well as the text, plus a margin.
        let content = match (vision::text_box(&upright, &lines, 80.0), vision::ink_box(&upright)) {
            (Some(t), Some(i)) => {
                let (w, h) = (upright.width(), upright.height());
                let x0 = t.0.min(i.0.saturating_sub(80));
                let y0 = t.1.min(i.1.saturating_sub(80));
                let x1 = (t.0 + t.2).max((i.0 + i.2 + 80).min(w));
                let y1 = (t.1 + t.3).max((i.1 + i.3 + 80).min(h));
                Some((x0, y0, x1 - x0, y1 - y0))
            }
            (t, _) => t,
        };
        let cropped_to = content.filter(|&(_, _, w, h)| {
            u64::from(w) * u64::from(h) * 3 < u64::from(upright.width()) * u64::from(upright.height()) * 2
        });
        let upright = match cropped_to {
            Some((x, y, w, h)) => {
                let (dx, dy) = (x as f32, y as f32);
                for l in &mut lines {
                    l.bbox = (l.bbox.0 - dx, l.bbox.1 - dy, l.bbox.2 - dx, l.bbox.3 - dy);
                }
                image::imageops::crop_imm(&upright, x, y, w, h).to_image()
            }
            None => upright,
        };
        info!(
            "{id} page {index}: turned {}° (score {:.2}){}, {} OCR lines",
            o.rotated_cw,
            o.score,
            skew.map(|a| format!(", straightened {a:.1}°")).unwrap_or_default(),
            lines.len()
        );
        Ok(Page {
            source_files: vec![file.to_string()],
            rotated_cw: o.rotated_cw,
            orientation_score: o.score,
            deskewed_deg: skew,
            copy_from: copy_from.filter(|_| o.rotated_cw == 0 && skew.is_none() && cropped_to.is_none()),
            cropped_to,
            upright,
            lines,
        })
    }

    /// A scan holding several pieces of paper: each becomes its own record,
    /// `<id>_a`, `<id>_b`, ... (largest piece first).
    fn process_pieces(
        &self,
        started: Instant,
        raw: &RawScan,
        file: &str,
        img: &image::RgbImage,
        found: Vec<pieces::Piece>,
    ) -> Result<PathBuf> {
        let id = &raw.id;
        let ids: Vec<String> = (0..found.len()).map(|k| format!("{id}_{}", piece_letter(k))).collect();
        info!("{id}: {} separate pieces of paper; filing them as {}", found.len(), ids.join(", "));
        // Earlier results for this scan as a whole, or split differently, are replaced.
        self.remove_outputs(id)?;
        self.remove_pieces(id)?;
        let mut last = None;
        for (k, piece) in found.iter().enumerate() {
            let page = self.prepare(&ids[k], 1, piece.cut(img), file, None)?;
            let split = SplitOut {
                piece: piece_letter(k).to_string(),
                of: found.len(),
                region: piece.region,
                siblings: ids.iter().filter(|s| **s != ids[k]).cloned().collect(),
            };
            last = Some(self.write_record(
                started,
                &ids[k],
                vec![id.clone()],
                raw.scanned_at.clone(),
                vec![page],
                None,
                Some(split),
                raw.device.make_and_model.clone(),
            )?);
        }
        last.context("no pieces")
    }

    /// Delete the records made from the pieces of an earlier split of `id`.
    fn remove_pieces(&self, id: &str) -> Result<()> {
        for k in 0..26 {
            let piece = format!("{id}_{}", piece_letter(k));
            if self.out.join(format!("{piece}.ron")).exists() {
                self.remove_outputs(&piece)?;
            }
        }
        Ok(())
    }

    /// the stitch window and it is a single page.
    fn previous(&self, id: &str) -> Option<Filed> {
        let (prev_id, filed) = self
            .records()
            .into_iter()
            .filter(|(pid, _)| pid.as_str() < id)
            .max_by(|a, b| a.0.cmp(&b.0))?;
        let last = filed.sources.last().cloned().unwrap_or(prev_id.clone());
        let gap = scan_time(id)? - scan_time(&last)?;
        (filed.pages.len() == 1 && gap >= chrono::TimeDelta::zero() && gap <= self.stitch_window)
            .then_some(Filed { id: prev_id, ..filed })
    }

    /// Delete a scan's own processed files once it has been stitched into another record.
    fn remove_outputs(&self, id: &str) -> Result<()> {
        let page_prefix = format!("{id}_p");
        for e in fs::read_dir(&self.out)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_page = name
                .strip_prefix(&page_prefix)
                .and_then(|rest| rest.strip_suffix(".jpg").or_else(|| rest.strip_suffix(".txt")))
                .is_some_and(|n| n.len() == 3 && n.chars().all(|c| c.is_ascii_digit()));
            if is_page || name == format!("{id}.ron") {
                info!("{id}: removing old output {name}");
                fs::remove_file(e.path())?;
            }
        }
        Ok(())
    }

    /// Every processed record, keyed by its id.
    fn records(&self) -> Vec<(String, Filed)> {
        fs::read_dir(&self.out)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".ron").filter(|i| !i.starts_with('.'))?.to_string();
                let mut f: Filed = ron::from_str(&fs::read_to_string(e.path()).ok()?).ok()?;
                if f.sources.is_empty() {
                    f.sources = vec![id.clone()];
                }
                Some((id, f))
            })
            .collect()
    }
}

pub fn run(cli: Cli) -> Result<()> {
    if let Some(Cmd::Pieces { ids, masks }) = &cli.cmd {
        let ids = if ids.is_empty() { all_ids(&cli.raw)? } else { ids.clone() };
        for id in ids {
            let raw: RawScan = ron::from_str(&fs::read_to_string(cli.raw.join(format!("{id}.ron")))?)?;
            for p in raw.pages.iter().filter(|p| p.content_type.starts_with("image/")) {
                let img = image::open(cli.raw.join(&p.file))?.to_rgb8();
                let found = pieces::find(&img);
                println!("{}: {} piece(s) {:?}", p.file, found.len(), found.iter().map(|f| (f.region, (f.fill * 100.0).round())).collect::<Vec<_>>());
                if let Some(dir) = masks {
                    fs::create_dir_all(dir)?;
                    pieces::mask_image(&img).save(dir.join(p.file.replace(".jpg", ".png")))?;
                }
            }
        }
        return Ok(());
    }
    info!("loading OCR models");
    let p = Pipeline {
        vision: Vision::load()?,
        extractor: Extractor::new(&cli.ollama, &cli.model)?,
        stitch_window: chrono::TimeDelta::minutes(cli.stitch_window_mins),
        raw: cli.raw,
        out: cli.out,
    };
    match cli.cmd.unwrap_or(Cmd::Watch { interval_secs: 5 }) {
        Cmd::Run { ids, force } => {
            let ids = if ids.is_empty() {
                if force { all_ids(&p.raw)? } else { p.pending()? }
            } else {
                ids
            };
            let mut failed = 0;
            for id in &ids {
                if let Err(e) = p.process(id) {
                    error!("{id}: {e:#}");
                    failed += 1;
                }
            }
            if failed > 0 {
                bail!("{failed} of {} scan(s) failed", ids.len());
            }
            Ok(())
        }
        Cmd::Watch { interval_secs } => watch(&p, Duration::from_secs(interval_secs)),
        Cmd::Pieces { .. } => unreachable!(),
    }
}

fn all_ids(raw: &Path) -> Result<Vec<String>> {
    let mut ids: Vec<String> = fs::read_dir(raw)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".ron").filter(|i| !i.starts_with('.')).map(str::to_owned))
        .collect();
    ids.sort();
    Ok(ids)
}

fn watch(p: &Pipeline, interval: Duration) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || {
            if stop.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
            info!("stopping after the current scan");
        })?;
    }
    info!("watching {} → {}", p.raw.display(), p.out.display());
    // Failed scans are retried after a while (e.g. Ollama was down or the GPU busy).
    let mut failed: HashMap<String, Instant> = HashMap::new();
    while !stop.load(Ordering::SeqCst) {
        for id in p.pending()? {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if failed.get(&id).is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
                continue;
            }
            match p.process(&id) {
                Ok(_) => {
                    failed.remove(&id);
                }
                Err(e) => {
                    error!("{id}: {e:#} (will retry in 10 minutes)");
                    failed.insert(id, Instant::now());
                }
            }
        }
        let until = Instant::now() + interval;
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(200));
        }
    }
    info!("stopped");
    Ok(())
}

fn piece_letter(k: usize) -> char {
    (b'a' + k as u8) as char
}
