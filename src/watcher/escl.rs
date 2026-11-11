//! Minimal eSCL (AirScan / Mopria) client: just what is needed to watch the
//! document feeder and pull scans off the device.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::{StatusCode, Url};
use roxmltree::{Document, Node};
use serde::Serialize;

const NS_SCAN: &str = "http://schemas.hp.com/imaging/escl/2011/05/03";
const NS_PWG: &str = "http://www.pwg.org/schemas/2010/12/sm";

/// eSCL `AdfState` value meaning "paper is sitting in the feeder".
pub const ADF_LOADED: &str = "ScannerAdfLoaded";

pub struct Escl {
    client: Client,
    base: Url,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub host: String,
    pub protocol: String,
    pub manufacturer: Option<String>,
    pub make_and_model: Option<String>,
    pub serial_number: Option<String>,
    pub uuid: Option<String>,
    pub escl_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Capabilities {
    pub device: DeviceInfo,
    pub platen: Option<InputCaps>,
    pub adf: Option<InputCaps>,
    /// Where a page narrower than the feeder sits across its width (Left/Right/Center).
    pub adf_justification_x: Option<String>,
}

/// Limits for one input source. Sizes are in 1/300 inch.
#[derive(Debug, Clone)]
pub struct InputCaps {
    pub max_width: u32,
    pub max_height: u32,
    pub color_modes: Vec<String>,
    pub formats: Vec<String>,
    pub resolutions: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub state: String,
    pub adf_state: Option<String>,
    pub jobs: Vec<JobInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobInfo {
    pub uuid: Option<String>,
    pub uri: Option<String>,
    pub state: Option<String>,
    pub state_reasons: Vec<String>,
    pub images_completed: Option<u32>,
}

pub enum CreateJob {
    Created(Url),
    Rejected(StatusCode, String),
}

pub enum NextDocument {
    Document { data: Vec<u8>, content_type: Option<String> },
    NoMore,
    NotReady,
}

/// A scan region in 1/300 inch, as eSCL expects.
#[derive(Debug, Clone, Serialize)]
pub struct Region {
    pub paper: String,
    pub units: String,
    pub x_offset: u32,
    pub y_offset: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanSettings {
    pub input_source: String,
    pub intent: String,
    pub color_mode: String,
    pub document_format: String,
    pub resolution_dpi: u32,
    pub region: Option<Region>,
    pub compression_factor: Option<u32>,
}

impl ScanSettings {
    pub fn to_xml(&self, version: &str) -> String {
        let mut x = String::new();
        x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        x.push_str(&format!(
            "<scan:ScanSettings xmlns:scan=\"{NS_SCAN}\" xmlns:pwg=\"{NS_PWG}\">\n"
        ));
        x.push_str(&format!("  <pwg:Version>{version}</pwg:Version>\n"));
        x.push_str(&format!("  <scan:Intent>{}</scan:Intent>\n", self.intent));
        if let Some(r) = &self.region {
            x.push_str("  <pwg:ScanRegions>\n    <pwg:ScanRegion>\n");
            x.push_str(&format!(
                "      <pwg:ContentRegionUnits>{}</pwg:ContentRegionUnits>\n",
                r.units
            ));
            x.push_str(&format!("      <pwg:XOffset>{}</pwg:XOffset>\n", r.x_offset));
            x.push_str(&format!("      <pwg:YOffset>{}</pwg:YOffset>\n", r.y_offset));
            x.push_str(&format!("      <pwg:Width>{}</pwg:Width>\n", r.width));
            x.push_str(&format!("      <pwg:Height>{}</pwg:Height>\n", r.height));
            x.push_str("    </pwg:ScanRegion>\n  </pwg:ScanRegions>\n");
        }
        x.push_str(&format!(
            "  <pwg:InputSource>{}</pwg:InputSource>\n",
            self.input_source
        ));
        x.push_str(&format!("  <scan:ColorMode>{}</scan:ColorMode>\n", self.color_mode));
        x.push_str(&format!(
            "  <pwg:DocumentFormat>{0}</pwg:DocumentFormat>\n  <scan:DocumentFormatExt>{0}</scan:DocumentFormatExt>\n",
            self.document_format
        ));
        x.push_str(&format!(
            "  <scan:XResolution>{0}</scan:XResolution>\n  <scan:YResolution>{0}</scan:YResolution>\n",
            self.resolution_dpi
        ));
        if let Some(c) = self.compression_factor {
            x.push_str(&format!("  <scan:CompressionFactor>{c}</scan:CompressionFactor>\n"));
        }
        x.push_str("</scan:ScanSettings>\n");
        x
    }
}

impl Escl {
    /// `host` is an IP/hostname (HTTPS is assumed) or a full base URL.
    pub fn new(host: &str) -> Result<Self> {
        let base = if host.contains("://") {
            Url::parse(host)
        } else {
            Url::parse(&format!("https://{host}/"))
        }
        .with_context(|| format!("invalid scanner host {host:?}"))?;
        let client = Client::builder()
            // Printers ship self-signed certificates.
            .tls_danger_accept_invalid_certs(true)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("scanai/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client, base })
    }

    pub fn host(&self) -> String {
        self.base.host_str().unwrap_or_default().to_string()
    }

    fn get_xml(&self, path: &str) -> Result<String> {
        let url = self.base.join(path)?;
        let resp = self
            .client
            .get(url.clone())
            .timeout(Duration::from_secs(10))
            .send()
            .with_context(|| format!("GET {url}"))?;
        let code = resp.status();
        let body = resp.text()?;
        if !code.is_success() {
            bail!("GET {url} returned {code}: {}", body.trim());
        }
        Ok(body)
    }

    pub fn capabilities(&self) -> Result<Capabilities> {
        let xml = self.get_xml("/eSCL/ScannerCapabilities")?;
        let doc = Document::parse(&xml).context("parsing ScannerCapabilities")?;
        let root = doc.root_element();
        let device = DeviceInfo {
            host: self.host(),
            protocol: "eSCL".into(),
            manufacturer: child_text(root, "Manufacturer"),
            make_and_model: child_text(root, "MakeAndModel"),
            serial_number: child_text(root, "SerialNumber"),
            uuid: child_text(root, "UUID"),
            escl_version: child_text(root, "Version"),
        };
        let adf_node = find(root, "Adf");
        Ok(Capabilities {
            device,
            platen: find(root, "PlatenInputCaps").map(parse_input_caps),
            adf: adf_node
                .and_then(|n| find(n, "AdfSimplexInputCaps"))
                .map(parse_input_caps),
            adf_justification_x: adf_node
                .and_then(|n| find(n, "Justification"))
                .and_then(|n| text(n, "XImagePosition")),
        })
    }

    pub fn status(&self) -> Result<Status> {
        let xml = self.get_xml("/eSCL/ScannerStatus")?;
        let doc = Document::parse(&xml).context("parsing ScannerStatus")?;
        let root = doc.root_element();
        let jobs = root
            .descendants()
            .filter(|n| is(n, "JobInfo"))
            .map(|j| JobInfo {
                uuid: text(j, "JobUuid"),
                uri: text(j, "JobUri"),
                state: text(j, "JobState"),
                state_reasons: texts(j, "JobStateReason"),
                images_completed: text(j, "ImagesCompleted").and_then(|s| s.parse().ok()),
            })
            .collect();
        Ok(Status {
            state: child_text(root, "State").unwrap_or_else(|| "Unknown".into()),
            adf_state: child_text(root, "AdfState"),
            jobs,
        })
    }

    pub fn create_job(&self, settings_xml: String) -> Result<CreateJob> {
        let url = self.base.join("/eSCL/ScanJobs")?;
        let resp = self
            .client
            .post(url.clone())
            .header(CONTENT_TYPE, "text/xml")
            .body(settings_xml)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let code = resp.status();
        let location = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp.text().unwrap_or_default();
        match (code.is_success(), location) {
            (true, Some(loc)) => Ok(CreateJob::Created(self.job_url(&loc)?)),
            (true, None) => bail!("scanner accepted the job ({code}) but sent no Location"),
            (false, _) => Ok(CreateJob::Rejected(code, body.trim().to_string())),
        }
    }

    /// Devices sometimes put the wrong scheme or port in Location, so keep only its path.
    fn job_url(&self, location: &str) -> Result<Url> {
        let path = match Url::parse(location) {
            Ok(u) => u.path().to_string(),
            Err(_) => location.to_string(),
        };
        Ok(self.base.join(path.trim_end_matches('/'))?)
    }

    /// The device holds the request open while the page goes through, so
    /// `timeout` must cover a whole page.
    pub fn next_document(&self, job: &Url, timeout: Duration) -> Result<NextDocument> {
        let url = Url::parse(&format!("{job}/NextDocument"))?;
        let resp = self
            .client
            .get(url.clone())
            .timeout(timeout)
            .send()
            .with_context(|| format!("GET {url}"))?;
        let code = resp.status();
        match code {
            StatusCode::OK => {
                let content_type = resp
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
                let data = resp.bytes().context("reading page data")?.to_vec();
                if data.is_empty() {
                    return Ok(NextDocument::NotReady);
                }
                Ok(NextDocument::Document { data, content_type })
            }
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(NextDocument::NoMore),
            StatusCode::SERVICE_UNAVAILABLE => Ok(NextDocument::NotReady),
            _ => {
                let body = resp.text().unwrap_or_default();
                bail!("GET {url} returned {code}: {}", body.trim())
            }
        }
    }

    pub fn delete_job(&self, job: &Url) -> Result<()> {
        self.client
            .delete(job.clone())
            .timeout(Duration::from_secs(10))
            .send()?;
        Ok(())
    }
}

fn parse_input_caps(n: Node) -> InputCaps {
    let num = |name| text(n, name).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut formats = texts(n, "DocumentFormat");
    formats.dedup();
    InputCaps {
        max_width: num("MaxWidth"),
        max_height: num("MaxHeight"),
        color_modes: texts(n, "ColorMode"),
        formats,
        resolutions: n
            .descendants()
            .filter(|d| is(d, "DiscreteResolution"))
            .filter_map(|d| text(d, "XResolution")?.parse().ok())
            .collect(),
    }
}

pub(crate) fn is(n: &Node, name: &str) -> bool {
    n.is_element() && n.tag_name().name() == name
}

pub(crate) fn find<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.descendants().find(|d| is(d, name))
}

pub(crate) fn text(n: Node, name: &str) -> Option<String> {
    find(n, name).and_then(|d| d.text()).map(|s| s.trim().to_string())
}

fn child_text(n: Node, name: &str) -> Option<String> {
    n.children()
        .find(|d| is(d, name))
        .and_then(|d| d.text())
        .map(|s| s.trim().to_string())
}

pub(crate) fn texts(n: Node, name: &str) -> Vec<String> {
    n.descendants()
        .filter(|d| is(d, name))
        .filter_map(|d| d.text())
        .map(|s| s.trim().to_string())
        .collect()
}
