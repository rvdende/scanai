//! WS-Scan over WSD: puts this machine in the printer panel's
//! "Scan → Computer (WSD)" list and pulls the scan when Start is pressed there.
//!
//! Flow: unicast WS-Discovery probe → metadata Get (find the scanner service)
//! → WS-Eventing Subscribe to ScanAvailableEvent with a ScanDestination, kept
//! alive with Renew → the printer POSTs ScanAvailableEvent to our listener →
//! CreateScanJob with its ScanIdentifier → RetrieveImage until the printer
//! answers ClientErrorNoImagesAvailable.

use std::net::{IpAddr, UdpSocket};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use log::{debug, info, warn};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use roxmltree::Document;
use uuid::Uuid;

use super::escl::{find, is, text, texts};

const NS: &str = concat!(
    r#"xmlns:soap="http://www.w3.org/2003/05/soap-envelope" "#,
    r#"xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" "#,
    r#"xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing" "#,
    r#"xmlns:wsd="http://schemas.xmlsoap.org/ws/2005/04/discovery" "#,
    r#"xmlns:wscn="http://schemas.microsoft.com/windows/2006/08/wdp/scan""#
);
const ANONYMOUS: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous";
const DISCOVERY_TO: &str = "urn:schemas-xmlsoap-org:ws:2005:04:discovery";
const ACT_PROBE: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe";
const ACT_GET: &str = "http://schemas.xmlsoap.org/ws/2004/09/transfer/Get";
const ACT_EVENTING: &str = "http://schemas.xmlsoap.org/ws/2004/08/eventing/";
const ACT_SCAN: &str = "http://schemas.microsoft.com/windows/2006/08/wdp/scan/";
const SCAN_AVAILABLE: &str = "ScanAvailableEvent";
const NO_IMAGES: &str = "ClientErrorNoImagesAvailable";

/// What one input source accepts over WSD. Sizes are in 1/1000 inch.
#[derive(Debug, Clone)]
pub struct SourceCaps {
    pub resolutions: Vec<u32>,
    pub max_width: u32,
    pub max_height: u32,
}

#[derive(Clone)]
pub struct Wsd {
    http: Client,
    pub scan_url: String,
    pub platen: Option<SourceCaps>,
    pub adf: Option<SourceCaps>,
}

#[derive(Debug, Clone)]
pub struct Subscription {
    manager_url: String,
    id: String,
    expires: Duration,
    destination_token: String,
}

/// Region values are in 1/1000 inch.
pub struct Ticket {
    /// "Platen" or "ADF"
    pub input_source: &'static str,
    pub color_mode: String,
    pub dpi: u32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub width: u32,
    pub height: u32,
}

pub struct Job {
    pub id: String,
    token: String,
}

pub struct Image {
    pub data: Vec<u8>,
    pub content_type: String,
}

struct Reply {
    status: StatusCode,
    content_type: String,
    body: Vec<u8>,
}

impl Reply {
    /// The response XML, or an error describing the SOAP fault.
    fn ok(self, what: &str) -> Result<String> {
        let xml = String::from_utf8_lossy(&self.body).into_owned();
        if let Some((code, reason)) = fault(&xml) {
            bail!("{what} failed: {code}: {reason}");
        }
        if !self.status.is_success() || xml.trim().is_empty() {
            bail!("{what} failed: HTTP {}", self.status);
        }
        Ok(xml)
    }
}

impl Wsd {
    /// Find the printer's WS-Scan service and what it supports.
    pub fn discover(host: &str) -> Result<Self> {
        let (endpoint, xaddr) = probe(host)?;
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("scanai/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let meta = post(&http, &xaddr, envelope(&endpoint, ACT_GET, "", ""), Duration::from_secs(10))?
            .ok("metadata Get")?;
        let doc = Document::parse(&meta).context("parsing device metadata")?;
        let scan_url = doc
            .descendants()
            .filter(|n| is(n, "Hosted"))
            .find(|h| text(*h, "Types").is_some_and(|t| t.contains("ScannerServiceType")))
            .and_then(|h| text(h, "Address"))
            .context("the printer's WSD metadata lists no scanner service")?;
        let mut wsd = Self { http, scan_url, platen: None, adf: None };
        wsd.load_caps()?;
        Ok(wsd)
    }

    fn call(&self, to: &str, action: &str, header: &str, body: &str, timeout: Duration) -> Result<Reply> {
        post(&self.http, to, envelope(to, action, header, body), timeout)
    }

    fn load_caps(&mut self) -> Result<()> {
        let body = "<wscn:GetScannerElementsRequest><wscn:RequestedElements>\
                    <wscn:Name>wscn:ScannerConfiguration</wscn:Name>\
                    </wscn:RequestedElements></wscn:GetScannerElementsRequest>";
        let xml = self
            .call(&self.scan_url, &format!("{ACT_SCAN}GetScannerElements"), "", body, Duration::from_secs(10))?
            .ok("GetScannerElements")?;
        let doc = Document::parse(&xml)?;
        let source = |name: &str, res: &str, max: &str| {
            let n = find(doc.root_element(), name)?;
            let size = find(n, max)?;
            let num = |field| text(size, field).and_then(|s| s.parse().ok()).unwrap_or(0);
            Some(SourceCaps {
                resolutions: find(n, res)
                    .and_then(|r| find(r, "Widths"))
                    .map(|w| texts(w, "Width").iter().filter_map(|s| s.parse().ok()).collect())
                    .unwrap_or_default(),
                max_width: num("Width"),
                max_height: num("Height"),
            })
        };
        self.platen = source("Platen", "PlatenResolutions", "PlatenMaximumSize");
        self.adf = source("ADF", "ADFResolutions", "ADFMaximumSize");
        Ok(())
    }

    fn subscribe(&self, notify_url: &str, display_name: &str, context: &str) -> Result<Subscription> {
        let ident = format!("urn:uuid:{}", Uuid::new_v4());
        let epr = format!(
            "<wsa:Address>{}</wsa:Address><wsa:ReferenceParameters><wse:Identifier>{ident}</wse:Identifier></wsa:ReferenceParameters>",
            esc(notify_url)
        );
        let body = format!(
            "<wse:Subscribe><wse:EndTo>{epr}</wse:EndTo>\
             <wse:Delivery Mode=\"{ACT_EVENTING}DeliveryModes/Push\"><wse:NotifyTo>{epr}</wse:NotifyTo></wse:Delivery>\
             <wse:Expires>PT1H</wse:Expires>\
             <wse:Filter Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/Action\">{ACT_SCAN}{SCAN_AVAILABLE}</wse:Filter>\
             <wscn:ScanDestinations><wscn:ScanDestination>\
             <wscn:ClientDisplayName>{}</wscn:ClientDisplayName><wscn:ClientContext>{}</wscn:ClientContext>\
             </wscn:ScanDestination></wscn:ScanDestinations></wse:Subscribe>",
            esc(display_name),
            esc(context)
        );
        let xml = self
            .call(&self.scan_url, &format!("{ACT_EVENTING}Subscribe"), "", &body, Duration::from_secs(10))?
            .ok("Subscribe")?;
        let doc = Document::parse(&xml)?;
        let root = doc.root_element();
        let manager = find(root, "SubscriptionManager").context("SubscribeResponse has no SubscriptionManager")?;
        Ok(Subscription {
            manager_url: text(manager, "Address").unwrap_or_else(|| self.scan_url.clone()),
            id: text(manager, "Identifier").context("SubscribeResponse has no Identifier")?,
            expires: text(root, "Expires")
                .and_then(|s| parse_duration(&s))
                .unwrap_or(Duration::from_secs(600)),
            destination_token: text(root, "DestinationToken")
                .context("the printer did not accept the scan destination")?,
        })
    }

    fn renew(&self, sub: &Subscription) -> Result<Duration> {
        let xml = self
            .call(
                &sub.manager_url,
                &format!("{ACT_EVENTING}Renew"),
                &id_header(sub),
                "<wse:Renew><wse:Expires>PT1H</wse:Expires></wse:Renew>",
                Duration::from_secs(10),
            )?
            .ok("Renew")?;
        Ok(Document::parse(&xml)
            .ok()
            .and_then(|d| text(d.root_element(), "Expires"))
            .and_then(|s| parse_duration(&s))
            .unwrap_or(sub.expires))
    }

    fn unsubscribe(&self, sub: &Subscription) -> Result<()> {
        self.call(
            &sub.manager_url,
            &format!("{ACT_EVENTING}Unsubscribe"),
            &id_header(sub),
            "<wse:Unsubscribe/>",
            Duration::from_secs(5),
        )?
        .ok("Unsubscribe")?;
        Ok(())
    }

    /// `scan_identifier` and `destination_token` tie the job to a panel request;
    /// without them this is a scan started from this machine.
    pub fn create_scan_job(
        &self,
        scan_identifier: Option<&str>,
        destination_token: Option<&str>,
        t: &Ticket,
    ) -> Result<Job> {
        let mut body = String::from("<wscn:CreateScanJobRequest>");
        if let Some(s) = scan_identifier {
            body += &format!("<wscn:ScanIdentifier>{}</wscn:ScanIdentifier>", esc(s));
        }
        if let Some(d) = destination_token {
            body += &format!("<wscn:DestinationToken>{}</wscn:DestinationToken>", esc(d));
        }
        body += &t.to_xml();
        body += "</wscn:CreateScanJobRequest>";
        let xml = self
            .call(&self.scan_url, &format!("{ACT_SCAN}CreateScanJob"), "", &body, Duration::from_secs(60))?
            .ok("CreateScanJob")?;
        let doc = Document::parse(&xml)?;
        let root = doc.root_element();
        Ok(Job {
            id: text(root, "JobId").context("CreateScanJobResponse has no JobId")?,
            token: text(root, "JobToken").context("CreateScanJobResponse has no JobToken")?,
        })
    }

    /// The next page of the job, or None once the printer has no more.
    pub fn retrieve_image(&self, job: &Job, n: usize) -> Result<Option<Image>> {
        let body = format!(
            "<wscn:RetrieveImageRequest><wscn:JobId>{}</wscn:JobId><wscn:JobToken>{}</wscn:JobToken>\
             <wscn:DocumentDescription><wscn:DocumentName>page{n}</wscn:DocumentName></wscn:DocumentDescription>\
             </wscn:RetrieveImageRequest>",
            esc(&job.id),
            esc(&job.token)
        );
        // The printer holds the request open while the page goes through.
        let reply = self.call(
            &self.scan_url,
            &format!("{ACT_SCAN}RetrieveImage"),
            "",
            &body,
            Duration::from_secs(300),
        )?;
        if reply.content_type.starts_with("multipart/") {
            let image = multipart(&reply.body, &reply.content_type)?
                .into_iter()
                .find(|p| !p.content_type.contains("xop+xml") && !p.content_type.contains("soap"))
                .context("RetrieveImage response has no image part")?;
            return Ok(Some(Image { data: image.body, content_type: image.content_type }));
        }
        let xml = String::from_utf8_lossy(&reply.body);
        match fault(&xml) {
            Some((code, _)) if code.ends_with(NO_IMAGES) => Ok(None),
            Some((code, reason)) => bail!("RetrieveImage failed: {code}: {reason}"),
            None => bail!(
                "unexpected RetrieveImage response (HTTP {}, {})",
                reply.status,
                reply.content_type
            ),
        }
    }
}

impl Ticket {
    fn to_xml(&self) -> String {
        // 0 = keep going until the feeder is empty.
        let images = if self.input_source == "Platen" { 1 } else { 0 };
        format!(
            "<wscn:ScanTicket><wscn:JobDescription><wscn:JobName>scanai</wscn:JobName>\
             <wscn:JobOriginatingUserName>scanai</wscn:JobOriginatingUserName></wscn:JobDescription>\
             <wscn:DocumentParameters><wscn:Format wscn:MustHonor=\"true\">exif</wscn:Format>\
             <wscn:ImagesToTransfer>{images}</wscn:ImagesToTransfer>\
             <wscn:InputSource wscn:MustHonor=\"true\">{src}</wscn:InputSource>\
             <wscn:InputSize><wscn:InputMediaSize><wscn:Width>{w}</wscn:Width><wscn:Height>{h}</wscn:Height></wscn:InputMediaSize></wscn:InputSize>\
             <wscn:MediaSides><wscn:MediaFront><wscn:ScanRegion>\
             <wscn:ScanRegionXOffset>{x}</wscn:ScanRegionXOffset><wscn:ScanRegionYOffset>{y}</wscn:ScanRegionYOffset>\
             <wscn:ScanRegionWidth>{w}</wscn:ScanRegionWidth><wscn:ScanRegionHeight>{h}</wscn:ScanRegionHeight>\
             </wscn:ScanRegion><wscn:ColorProcessing wscn:MustHonor=\"true\">{color}</wscn:ColorProcessing>\
             <wscn:Resolution wscn:MustHonor=\"true\"><wscn:Width>{dpi}</wscn:Width><wscn:Height>{dpi}</wscn:Height></wscn:Resolution>\
             </wscn:MediaFront></wscn:MediaSides></wscn:DocumentParameters></wscn:ScanTicket>",
            src = self.input_source,
            w = self.width,
            h = self.height,
            x = self.x_offset,
            y = self.y_offset,
            color = self.color_mode,
            dpi = self.dpi,
        )
    }
}

pub enum Event {
    /// Someone picked this machine on the panel and pressed Start.
    ScanAvailable { scan_identifier: String },
    SubscriptionEnded,
}

enum Ctl {
    CheckNow,
    Shutdown,
}

type Shared = Arc<Mutex<Option<(Wsd, Subscription)>>>;

/// Keeps this machine registered on the printer panel and delivers panel requests.
pub struct Runtime {
    pub events: Receiver<Event>,
    ctl: Sender<Ctl>,
    shared: Shared,
    keeper: Option<JoinHandle<()>>,
    server: Arc<tiny_http::Server>,
}

impl Runtime {
    pub fn start(host: &str, port: u16, name: &str) -> Result<Self> {
        let ip = local_ip_towards(host)?;
        let server = Arc::new(
            tiny_http::Server::http((ip, port))
                .map_err(|e| anyhow!("can't listen on {ip}:{port}: {e}"))?,
        );
        let notify_url = format!("http://{ip}:{port}/wsd");
        // Unique per run, so a stale panel entry left by a crashed run is ignored.
        let context = format!("scanai-{}", Uuid::new_v4().simple());
        let (tx, events) = channel();
        spawn_listener(server.clone(), context.clone(), tx);
        let (ctl, ctl_rx) = channel();
        let shared = Shared::default();
        let keeper = spawn_keeper(
            host.to_string(),
            notify_url,
            name.to_string(),
            context,
            shared.clone(),
            ctl_rx,
        );
        Ok(Self { events, ctl, shared, keeper: Some(keeper), server })
    }

    /// The scanner service and the destination token for the current registration.
    pub fn current(&self) -> Option<(Wsd, String)> {
        self.shared
            .lock()
            .unwrap()
            .as_ref()
            .map(|(w, s)| (w.clone(), s.destination_token.clone()))
    }

    /// Check the registration is still alive, registering again if not.
    pub fn check_now(&self) {
        let _ = self.ctl.send(Ctl::CheckNow);
    }

    /// Remove this machine from the panel and stop listening.
    pub fn stop(mut self) {
        let _ = self.ctl.send(Ctl::Shutdown);
        if let Some(k) = self.keeper.take() {
            let _ = k.join();
        }
        self.server.unblock();
    }
}

fn spawn_listener(server: Arc<tiny_http::Server>, context: String, tx: Sender<Event>) {
    thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let _ = req.respond(tiny_http::Response::empty(202));
            match parse_event(&body, &context) {
                Some(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                None => debug!("ignored WSD message: {:.300}", body),
            }
        }
    });
}

fn parse_event(body: &str, context: &str) -> Option<Event> {
    let doc = Document::parse(body).ok()?;
    let root = doc.root_element();
    let action = text(root, "Action")?;
    if action.ends_with(SCAN_AVAILABLE) {
        if text(root, "ClientContext").is_some_and(|c| c != context) {
            info!("ignored a panel request for an older registration");
            return None;
        }
        return Some(Event::ScanAvailable { scan_identifier: text(root, "ScanIdentifier")? });
    }
    action.ends_with("SubscriptionEnd").then_some(Event::SubscriptionEnded)
}

fn spawn_keeper(
    host: String,
    notify_url: String,
    name: String,
    context: String,
    shared: Shared,
    ctl: Receiver<Ctl>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut failing = false;
        loop {
            let current = shared.lock().unwrap().clone();
            let wait = match current {
                Some((wsd, sub)) => match wsd.renew(&sub) {
                    Ok(expires) => {
                        debug!("WSD registration renewed for {expires:?}");
                        refresh_after(expires)
                    }
                    Err(e) => {
                        info!("WSD registration lapsed ({e:#}); registering again");
                        *shared.lock().unwrap() = None;
                        Duration::ZERO
                    }
                },
                None => match Wsd::discover(&host)
                    .and_then(|w| w.subscribe(&notify_url, &name, &context).map(|s| (w, s)))
                {
                    Ok((wsd, sub)) => {
                        info!("on the printer panel as \"{name}\" (Scan → Computer (WSD)), events to {notify_url}");
                        failing = false;
                        let wait = refresh_after(sub.expires);
                        *shared.lock().unwrap() = Some((wsd, sub));
                        wait
                    }
                    Err(e) => {
                        if failing {
                            debug!("WSD registration failed: {e:#}");
                        } else {
                            warn!("WSD registration failed: {e:#}; retrying every 30s");
                            failing = true;
                        }
                        Duration::from_secs(30)
                    }
                },
            };
            match ctl.recv_timeout(wait) {
                Ok(Ctl::CheckNow) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Ctl::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    if let Some((wsd, sub)) = shared.lock().unwrap().take() {
                        match wsd.unsubscribe(&sub) {
                            Ok(()) => info!("removed \"{name}\" from the printer panel"),
                            Err(e) => debug!("unsubscribe failed: {e:#}"),
                        }
                    }
                    return;
                }
            }
        }
    })
}

fn refresh_after(expires: Duration) -> Duration {
    (expires * 2 / 3).clamp(Duration::from_secs(30), Duration::from_secs(600))
}

/// Ask the printer directly (unicast WS-Discovery) for its endpoint address and metadata URL.
fn probe(host: &str) -> Result<(String, String)> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = vec![0u8; 65536];
    for _ in 0..3 {
        // A fresh MessageID each time: WSD devices drop repeated ones.
        let msg = envelope(
            DISCOVERY_TO,
            ACT_PROBE,
            "",
            "<wsd:Probe><wsd:Types>wscn:ScanDeviceType</wsd:Types></wsd:Probe>",
        );
        sock.send_to(msg.as_bytes(), (host, 3702))?;
        while let Ok((n, _)) = sock.recv_from(&mut buf) {
            let xml = String::from_utf8_lossy(&buf[..n]);
            let Ok(doc) = Document::parse(&xml) else { continue };
            let Some(m) = find(doc.root_element(), "ProbeMatch") else { continue };
            let endpoint = text(m, "Address");
            let xaddr = text(m, "XAddrs")
                .and_then(|x| x.split_whitespace().find(|a| a.starts_with("http://")).map(str::to_owned));
            if let (Some(e), Some(x)) = (endpoint, xaddr) {
                return Ok((e, x));
            }
        }
    }
    bail!("no WS-Discovery reply from {host}; is WSD enabled in the printer's Protocol settings?")
}

fn local_ip_towards(host: &str) -> Result<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0")?;
    s.connect((host, 3702)).with_context(|| format!("no route to {host}"))?;
    Ok(s.local_addr()?.ip())
}

fn envelope(to: &str, action: &str, extra_header: &str, body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><soap:Envelope {NS}><soap:Header>\
         <wsa:To>{}</wsa:To><wsa:Action>{action}</wsa:Action><wsa:MessageID>urn:uuid:{}</wsa:MessageID>\
         <wsa:ReplyTo><wsa:Address>{ANONYMOUS}</wsa:Address></wsa:ReplyTo>{extra_header}\
         </soap:Header><soap:Body>{body}</soap:Body></soap:Envelope>",
        esc(to),
        Uuid::new_v4()
    )
}

fn id_header(sub: &Subscription) -> String {
    format!("<wse:Identifier>{}</wse:Identifier>", esc(&sub.id))
}

fn post(http: &Client, url: &str, xml: String, timeout: Duration) -> Result<Reply> {
    let resp = http
        .post(url)
        .header(CONTENT_TYPE, "application/soap+xml; charset=utf-8")
        .timeout(timeout)
        .body(xml)
        .send()
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = resp.bytes().with_context(|| format!("reading response from {url}"))?.to_vec();
    Ok(Reply { status, content_type, body })
}

/// (subcode or code, reason) of a SOAP fault, if `xml` is one.
fn fault(xml: &str) -> Option<(String, String)> {
    let doc = Document::parse(xml).ok()?;
    let f = find(doc.root_element(), "Fault")?;
    let code = find(f, "Subcode")
        .and_then(|s| text(s, "Value"))
        .or_else(|| find(f, "Code").and_then(|c| text(c, "Value")))
        .unwrap_or_else(|| "Fault".into());
    let reason = find(f, "Reason").and_then(|r| text(r, "Text")).unwrap_or_default();
    Some((code, reason))
}

struct Part {
    content_type: String,
    body: Vec<u8>,
}

/// Split an MTOM (multipart/related) response into its parts.
fn multipart(body: &[u8], content_type: &str) -> Result<Vec<Part>> {
    let boundary = content_type
        .split(';')
        .find_map(|p| {
            let (k, v) = p.split_once('=')?;
            k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"'))
        })
        .context("multipart response without a boundary")?;
    let delim = format!("--{boundary}").into_bytes();
    let mut starts = Vec::new();
    let mut i = 0;
    while let Some(p) = position(&body[i..], &delim) {
        starts.push(i + p);
        i += p + delim.len();
    }
    let mut parts = Vec::new();
    for w in starts.windows(2) {
        let mut seg = &body[w[0] + delim.len()..w[1]];
        seg = seg.strip_prefix(b"\r\n").unwrap_or(seg);
        // The CRLF before the next delimiter belongs to the delimiter.
        seg = seg.strip_suffix(b"\r\n").unwrap_or(seg);
        let Some(split) = position(seg, b"\r\n\r\n") else { continue };
        let headers = String::from_utf8_lossy(&seg[..split]);
        let content_type = headers
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case("content-type").then(|| v.trim().to_string())
            })
            .unwrap_or_default();
        parts.push(Part { content_type, body: seg[split + 4..].to_vec() });
    }
    Ok(parts)
}

fn position(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// xs:duration such as PT15M or P1DT2H. Years and months never show up here.
fn parse_duration(s: &str) -> Option<Duration> {
    let rest = s.trim().strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut secs = 0f64;
    for (part, units) in [
        (date, &[('W', 604800.0), ('D', 86400.0)][..]),
        (time, &[('H', 3600.0), ('M', 60.0), ('S', 1.0)][..]),
    ] {
        let mut num = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
            } else {
                let mul = units.iter().find(|(u, _)| *u == c)?.1;
                secs += num.parse::<f64>().ok()? * mul;
                num.clear();
            }
        }
    }
    Some(Duration::from_secs_f64(secs))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT15M"), Some(Duration::from_secs(900)));
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("P1DT1S"), Some(Duration::from_secs(86401)));
        assert_eq!(parse_duration("2026-11-09T11:00:00Z"), None);
    }

    #[test]
    fn mtom() {
        let body = b"\r\n--B\r\nContent-Type:application/xop+xml\r\n\r\n<x/>\r\n--B\r\nContent-Type:image/jpeg\r\nContent-ID:<a>\r\n\r\n\xff\xd8--\r\n\xff\xd9\r\n--B--\r\n";
        let parts = multipart(body, "multipart/related; boundary=\"B\"; type=\"application/xop+xml\"").unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].content_type, "image/jpeg");
        assert_eq!(parts[1].body, b"\xff\xd8--\r\n\xff\xd9");
    }

    #[test]
    fn scan_available_event() {
        let body = r#"<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:wscn="http://schemas.microsoft.com/windows/2006/08/wdp/scan"><soap:Header><wsa:Action>http://schemas.microsoft.com/windows/2006/08/wdp/scan/ScanAvailableEvent</wsa:Action></soap:Header><soap:Body><wscn:ScanAvailableEvent><wscn:ClientContext>me</wscn:ClientContext><wscn:ScanIdentifier>abc</wscn:ScanIdentifier></wscn:ScanAvailableEvent></soap:Body></soap:Envelope>"#;
        assert!(matches!(parse_event(body, "me"), Some(Event::ScanAvailable { scan_identifier }) if scan_identifier == "abc"));
        assert!(parse_event(body, "someone-else").is_none());
    }
}
