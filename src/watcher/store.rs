//! On-disk layout. Everything sits directly in `<out>`, named by the local
//! time the scan started (`<id>` = YYYY-MM-DD_HH-MM-SS):
//!
//!   <out>/<id>_p001.jpg
//!   <out>/<id>_p002.jpg
//!   <out>/<id>.ron          metadata for the whole scan, listing its pages
//!
//! Pages are written to `<out>/.incoming/` while the job runs and moved into
//! place when it ends, with the .ron moved last: a scan is complete once its
//! .ron exists.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::escl::{DeviceInfo, ScanSettings};

pub const INCOMING: &str = ".incoming";

#[derive(Debug, Serialize)]
pub struct ScanRecord {
    pub id: String,
    pub status: ScanStatus,
    pub trigger: Trigger,
    pub scanned_at: DateTime<Local>,
    pub finished_at: DateTime<Local>,
    pub device: DeviceInfo,
    pub settings: ScanSettings,
    pub job: JobRecord,
    pub page_count: usize,
    pub pages: Vec<PageRecord>,
    pub error: Option<String>,
    pub software: String,
}

#[derive(Debug, Serialize)]
pub enum ScanStatus {
    /// Every page the scanner produced was saved.
    Complete,
    /// Some pages were saved, then something went wrong (see `error`).
    Partial,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum Trigger {
    /// Paper was detected in the document feeder.
    FeederLoaded,
    /// Started by hand with `scanai watcher once`.
    Manual,
    /// Started on the printer panel with Scan → Computer (WSD).
    PanelButton,
}

#[derive(Debug, Serialize)]
pub struct JobRecord {
    pub uri: String,
    pub uuid: Option<String>,
    /// WSD job id (WSD jobs have no uuid).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub final_state: Option<String>,
    pub state_reasons: Vec<String>,
    pub images_completed: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct PageRecord {
    pub index: usize,
    pub file: String,
    pub content_type: String,
    pub bytes: usize,
    pub sha256: String,
    pub width_px: Option<usize>,
    pub height_px: Option<usize>,
    pub received_at: DateTime<Local>,
}

pub struct PendingScan {
    pub id: String,
    out: PathBuf,
    incoming: PathBuf,
    files: Vec<String>,
}

impl PendingScan {
    pub fn create(out: &Path, at: DateTime<Local>) -> Result<Self> {
        let incoming = out.join(INCOMING);
        fs::create_dir_all(&incoming)
            .with_context(|| format!("creating {}", incoming.display()))?;
        let mut taken = names(out);
        taken.extend(names(&incoming));
        let base = at.format("%Y-%m-%d_%H-%M-%S").to_string();
        let id = (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base}_{n}") })
            .find(|id| {
                let (ron, page) = (format!("{id}.ron"), format!("{id}_p"));
                !taken.iter().any(|t| *t == ron || t.starts_with(&page))
            })
            .unwrap();
        Ok(Self { id, out: out.to_path_buf(), incoming, files: Vec::new() })
    }

    pub fn write_page(
        &mut self,
        index: usize,
        data: &[u8],
        content_type: &str,
    ) -> Result<PageRecord> {
        let file = format!("{}_p{index:03}.{}", self.id, extension(content_type));
        let path = self.incoming.join(&file);
        fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
        self.files.push(file.clone());
        let size = imagesize::blob_size(data).ok();
        Ok(PageRecord {
            index,
            file,
            content_type: content_type.to_string(),
            bytes: data.len(),
            sha256: Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect(),
            width_px: size.map(|s| s.width),
            height_px: size.map(|s| s.height),
            received_at: Local::now(),
        })
    }

    /// Move the pages into place, then write `<id>.ron` last. Returns the .ron path.
    pub fn finish(self, record: &ScanRecord) -> Result<PathBuf> {
        let ron_name = format!("{}.ron", self.id);
        let ron = ron::ser::to_string_pretty(record, ron::ser::PrettyConfig::default())?;
        fs::write(self.incoming.join(&ron_name), ron + "\n")?;
        for f in self.files.iter().chain([&ron_name]) {
            let (from, to) = (self.incoming.join(f), self.out.join(f));
            fs::rename(&from, &to)
                .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        }
        Ok(self.out.join(ron_name))
    }

    pub fn discard(self) {
        for f in &self.files {
            let _ = fs::remove_file(self.incoming.join(f));
        }
    }
}

fn names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn extension(content_type: &str) -> &'static str {
    match content_type {
        "image/jpeg" => "jpg",
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/tiff" => "tif",
        _ => "bin",
    }
}

/// Folders left in .incoming by a run that was killed mid-scan.
pub fn leftovers(out: &Path) -> Vec<PathBuf> {
    fs::read_dir(out.join(INCOMING))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default()
}
