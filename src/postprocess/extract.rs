//! Classify a document and pull out its fields with a local vision model through Ollama.

use std::io::Cursor;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use log::warn;
use base64::Engine;
use chrono::NaiveDate;
use image::RgbImage;
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DOC_TYPES: &[&str] = &[
    "invoice", "receipt", "quote", "delivery_note", "purchase_order", "account_statement",
    "bank_statement", "payslip", "tax_document", "utility_bill", "vehicle_document",
    "insurance", "contract", "letter", "medical", "identity_document", "certificate",
    "warranty", "manual", "product_packaging", "form", "other",
];

#[derive(Debug, Serialize, Deserialize)]
pub struct Extraction {
    pub doc_type: String,
    pub doc_type_detail: String,
    pub title: Option<String>,
    pub languages: Vec<String>,
    pub summary: String,
    /// Main date of the document (YYYY-MM-DD), if it has one.
    pub document_date: Option<String>,
    /// What the model gave when it wasn't a valid YYYY-MM-DD date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_date_raw: Option<String>,
    pub dates: Vec<LabeledDate>,
    pub parties: Vec<Party>,
    pub addresses: Vec<LabeledAddress>,
    pub amounts: Vec<Amount>,
    pub reference_numbers: Vec<LabeledValue>,
    /// Purchased items, one per line on an invoice, receipt, quote or slip.
    #[serde(default)]
    pub line_items: Vec<LineItem>,
    #[serde(default)]
    pub line_items_check: Option<ItemsCheck>,
    /// Lowercase, hyphenated topic tags shared across documents (always includes doc_type).
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LabeledDate {
    pub label: String,
    pub date: String,
    /// False when `date` isn't a real YYYY-MM-DD date.
    #[serde(default = "yes")]
    pub valid: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Party {
    pub role: String,
    pub name: Option<String>,
    pub address: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub tax_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LabeledAddress {
    pub label: String,
    pub address: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Amount {
    pub label: String,
    pub value: f64,
    pub currency: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LineItem {
    pub description: String,
    /// Stock code / SKU / product code as printed.
    pub code: Option<String>,
    pub quantity: Option<f64>,
    /// Unit of measure as printed (EA, kg, L, ...).
    pub unit: Option<String>,
    pub unit_price: Option<f64>,
    /// Discount on this line as an amount (not a percentage).
    pub discount: Option<f64>,
    /// What this line costs in total, after discount.
    pub total: Option<f64>,
    pub currency: Option<String>,
    /// Serial numbers, notes and other extra details printed with the item.
    pub details: Option<String>,
    /// Whether quantity × unit_price − discount equals total (None: not enough numbers to tell).
    #[serde(default)]
    pub adds_up: Option<bool>,
}

/// Do the line totals add up to one of the document's printed amounts?
#[derive(Debug, Serialize, Deserialize)]
pub struct ItemsCheck {
    pub sum: f64,
    /// Label of the printed amount equal to the sum (e.g. "SUB TOTAL"), if any.
    pub matches: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LabeledValue {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct RunInfo {
    pub model: String,
    pub seconds: f32,
    pub prompt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub images_sent: usize,
}

const SYSTEM: &str = "You read scanned documents and return structured data as JSON.
Rules:
- Use only what is visible in the document. Never invent values; use null or an empty list when something is absent.
- doc_type is the closest category; doc_type_detail says precisely what it is (e.g. \"South African motor vehicle licence (MVL1)\").
- Dates are YYYY-MM-DD. South African documents usually write dates day-first.
- document_date is the main date of the document (issue/invoice date), if any.
- parties: who issued it, who it is addressed to, shipper/consignee, etc. role is a short word such as issuer, recipient, shipper, consignee, owner, supplier, customer.
- addresses: every postal or street address, each with a short label saying whose it is.
- amounts: monetary amounts with their label as printed; value is a plain number without currency symbols or thousands separators; currency is an ISO code such as ZAR when known.
- Number formats: South African slips often write a space between thousands and a comma before the cents, so \"1 145,00\" is 1145.00 and \"R 12 198,00\" is 12198.00. Never drop the digits before the space.
- reference_numbers: invoice/account/order/waybill/registration/VIN/licence numbers etc., value exactly as printed.
- summary: one or two sentences saying what the document is and what it is about.
- languages: ISO 639-1 codes of the languages used.
- line_items: every purchased product or service line on an invoice, receipt, till slip, quote or order, in printed order; empty for other documents. description is the item name as printed; code is its stock code/SKU; prices are plain numbers; total is what the line costs after any discount; details holds serial numbers or notes printed under the item. Do not list subtotals, VAT, totals or payments as items.
";
const TAG_SYSTEM: &str = "You assign a document type and filing tags to documents so that documents of the same kind always get the same type and the same tags.
doc_type: you get the type proposed by whoever read the document plus the types already used for other documents, with examples of what they were. If an already-used type covers documents like this one, answer with that type; otherwise keep the proposed type.
tags: return 3 to 6 tags. Each tag is lowercase words joined by hyphens.
Cover: the specific kind of document, its subject area, and the organisation or authority that issued it.
Never use names of private people, dates, amounts, quantities or reference numbers.
Reuse an existing tag exactly whenever it means the same thing; only create a new tag when no existing tag fits.
Only tag what the document is; don't add tags for things it is not.";

fn schema() -> Value {
    let s = json!({"type": "string"});
    let ns = json!({"type": ["string", "null"]});
    let nn = json!({"type": ["number", "null"]});
    let obj = |props: Value| {
        let required: Vec<&String> = props.as_object().unwrap().keys().collect();
        json!({"type": "object", "properties": props, "required": required})
    };
    let arr = |item: Value| json!({"type": "array", "items": item});
    obj(json!({
        "doc_type": {"type": "string", "enum": DOC_TYPES},
        "doc_type_detail": s,
        "title": ns,
        "languages": arr(s.clone()),
        "summary": s,
        "document_date": ns,
        "dates": arr(obj(json!({"label": s, "date": s}))),
        "parties": arr(obj(json!({"role": s, "name": ns, "address": ns, "phone": ns, "email": ns, "tax_id": ns}))),
        "addresses": arr(obj(json!({"label": s, "address": s}))),
        "amounts": arr(obj(json!({"label": s, "value": {"type": "number"}, "currency": ns}))),
        "reference_numbers": arr(obj(json!({"label": s, "value": s}))),
        "line_items": arr(obj(json!({
            "description": s, "code": ns, "quantity": nn, "unit": ns, "unit_price": nn,
            "discount": nn, "total": nn, "currency": ns, "details": ns,
        }))),
    }))
}

/// What earlier scans were filed under, so new scans can be filed consistently.
#[derive(Default)]
pub struct Known {
    /// Tags in use, most used first.
    pub tags: Vec<String>,
    /// Document types in use, most used first, with a few example doc_type_details.
    pub types: Vec<(String, Vec<String>)>,
    /// filing_key() of earlier scans → (doc_type, tags), so the same kind of document
    /// from the same issuer is filed identically without asking the model again.
    pub by_detail: std::collections::HashMap<String, (String, Vec<String>)>,
}

struct ChatError {
    error: anyhow::Error,
    transient: bool,
}

pub struct Extractor {
    http: reqwest::blocking::Client,
    pub url: String,
    pub model: String,
    pub max_images: usize,
    pub max_side: u32,
}

impl Extractor {
    pub fn new(url: &str, model: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(900))
                .build()?,
            url: url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            max_images: 4,
            max_side: 1600,
        })
    }

    /// Reads the pages, then tags the result in a separate text-only step so the
    /// tagging instructions can't bias the extraction. `known` is what earlier
    /// scans were filed under; the tagger reuses those types and tags where they fit.
    pub fn extract(
        &self,
        pages: &[RgbImage],
        ocr_texts: &[String],
        known: &Known,
    ) -> Result<(Extraction, RunInfo)> {
        let mut text = String::new();
        for (i, t) in ocr_texts.iter().enumerate() {
            text += &format!("--- page {} ---\n{t}\n", i + 1);
        }
        let text: String = text.chars().take(12000).collect();
        let shown = pages.len().min(self.max_images);
        let prompt = format!(
            "This scan has {} page(s); {shown} page image(s) are attached in order. Machine OCR of \
             every page follows; it may contain recognition errors, so trust the images where they \
             disagree.\n\n{text}",
            pages.len()
        );
        let images = pages[..shown]
            .iter()
            .map(|p| self.encode(p))
            .collect::<Result<Vec<_>>>()?;
        let started = Instant::now();
        let (content, resp) = self.chat(json!([
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": prompt, "images": images},
        ]), schema())?;
        let mut ex: Extraction = serde_json::from_str(&content)
            .with_context(|| format!("model output didn't match the schema: {content:.500}"))?;
        clean(&mut ex);
        let mut prompt_tokens = resp["prompt_eval_count"].as_u64();
        let mut output_tokens = resp["eval_count"].as_u64();

        if let Some((doc_type, tags)) = known.by_detail.get(&filing_key(&ex.doc_type_detail, &ex.parties)) {
            ex.doc_type = doc_type.clone();
            ex.tags = tags.clone();
        } else {
            let (doc_type, tags, resp) = self.tag(&ex, known)?;
            ex.doc_type = doc_type;
            ex.tags = tags;
            prompt_tokens = prompt_tokens.zip(resp["prompt_eval_count"].as_u64()).map(|(a, b)| a + b);
            output_tokens = output_tokens.zip(resp["eval_count"].as_u64()).map(|(a, b)| a + b);
        }
        Ok((
            ex,
            RunInfo {
                model: self.model.clone(),
                seconds: started.elapsed().as_secs_f32(),
                prompt_tokens,
                output_tokens,
                images_sent: shown,
            },
        ))
    }

    fn tag(&self, ex: &Extraction, known: &Known) -> Result<(String, Vec<String>, Value)> {
        let orgs: Vec<&str> = ex
            .parties
            .iter()
            .filter(|p| matches!(p.role.as_str(), "issuer" | "supplier" | "shipper"))
            .filter_map(|p| p.name.as_deref())
            .collect();
        let vocabulary = if known.tags.is_empty() {
            "(none yet)".to_string()
        } else {
            known.tags.join(", ")
        };
        let types = if known.types.is_empty() {
            "(none yet)".to_string()
        } else {
            known
                .types
                .iter()
                .map(|(t, ex)| format!("- {t}: e.g. {}", ex.join("; ")))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let prompt = format!(
            "Proposed document type: {}\nWhat it is: {}\nTitle: {}\nSummary: {}\nIssued by: {}\n\n\
             Document types already used:\n{types}\n\nExisting tags: {vocabulary}",
            ex.doc_type,
            ex.doc_type_detail,
            ex.title.as_deref().unwrap_or("-"),
            ex.summary,
            if orgs.is_empty() { "-".into() } else { orgs.join("; ") },
        );
        let (content, resp) = self.chat(json!([
            {"role": "system", "content": TAG_SYSTEM},
            {"role": "user", "content": prompt},
        ]), json!({
            "type": "object",
            "properties": {
                "doc_type": {"type": "string", "enum": DOC_TYPES},
                "tags": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["doc_type", "tags"],
        }))?;
        #[derive(Deserialize)]
        struct Tags {
            doc_type: String,
            tags: Vec<String>,
        }
        let t: Tags = serde_json::from_str(&content)
            .with_context(|| format!("tagger output didn't match the schema: {content:.300}"))?;
        // The document type is always a tag, so same-type documents share at least one.
        let mut seen = std::collections::HashSet::new();
        let doc_type = t.doc_type.clone();
        let tags = std::iter::once(doc_type.as_str())
            .chain(t.tags.iter().map(String::as_str))
            .map(normalize_tag)
            .filter(|t| !t.is_empty() && seen.insert(t.clone()))
            .take(9)
            .collect();
        Ok((doc_type, tags, resp))
    }

    /// One non-streaming chat call; returns the answer text and the raw response.
    fn chat(&self, mut messages: Value, format: Value) -> Result<(String, Value)> {
        // qwen3-vl always opens its answer with a thinking block, whatever `think` says.
        // Ollama 0.32 applied the format schema from the first token, which cut the
        // thinking short; 0.40 applies it only after the thinking, which can run for
        // tens of thousands of tokens. Starting the answer with an already closed
        // thinking block gives the JSON straight away on both.
        if let Some(m) = messages.as_array_mut() {
            m.push(json!({"role": "assistant", "content": "<think>\n\n</think>\n\n"}));
        }
        let body = json!({
            "model": self.model,
            "stream": false,
            "think": false,
            "format": format,
            "options": {"temperature": 0, "num_ctx": 32768},
            "messages": messages,
        });
        // Ollama answers 500 when its model runner dies, which llama-server now and
        // then does while loading the model; the next request starts a fresh runner.
        // The same goes for Ollama restarting. So try again before giving up.
        const WAITS: [u64; 3] = [5, 20, 60];
        let mut attempt = 0;
        let resp: Value = loop {
            let err = match self.post(&body) {
                Ok(resp) => break resp,
                Err(e) => e,
            };
            let Some(wait) = WAITS.get(attempt).filter(|_| err.transient) else {
                return Err(err.error);
            };
            attempt += 1;
            warn!("{:#}; trying again in {wait} s ({attempt} of {})", err.error, WAITS.len());
            std::thread::sleep(Duration::from_secs(*wait));
        };
        // Without the closed thinking block above, Ollama 0.32 put the answer in
        // `thinking` and left `content` empty; keep reading both.
        let content = [&resp["message"]["content"], &resp["message"]["thinking"]]
            .into_iter()
            .filter_map(|v| v.as_str())
            .find(|s| !s.trim().is_empty())
            .context("Ollama returned an empty message")?
            .to_string();
        Ok((content, resp))
    }

    /// Sends one chat request. A failure is `transient` when the same request may
    /// well succeed a little later: Ollama unreachable, or a 5xx answer.
    fn post(&self, body: &Value) -> Result<Value, ChatError> {
        let url = format!("{}/api/chat", self.url);
        let resp = self.http.post(&url).json(body).send().map_err(|e| ChatError {
            transient: e.is_connect(),
            error: anyhow::Error::new(e).context(format!("is Ollama running at {}?", self.url)),
        })?;
        let status = resp.status();
        if !status.is_success() {
            // Ollama says what went wrong in the body, as {"error": "..."}.
            let text = resp.text().unwrap_or_default();
            let reason = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or(text);
            return Err(ChatError {
                transient: status.is_server_error(),
                error: anyhow!("Ollama answered {status} for {url}: {reason:.500}"),
            });
        }
        resp.json().map_err(|e| ChatError { transient: false, error: e.into() })
    }

    fn encode(&self, page: &RgbImage) -> Result<String> {
        let (w, h) = page.dimensions();
        let scale = (self.max_side as f32 / w.max(h) as f32).min(1.0);
        let small = image::imageops::resize(
            page,
            (w as f32 * scale) as u32,
            (h as f32 * scale) as u32,
            image::imageops::FilterType::Triangle,
        );
        let mut buf = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut buf, 90).encode_image(&small)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(buf.into_inner()))
    }
}

fn is_date(s: &str) -> bool {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

fn clean(ex: &mut Extraction) {
    if let Some(d) = ex.document_date.take() {
        if is_date(&d) {
            ex.document_date = Some(d);
        } else if !d.trim().is_empty() {
            ex.document_date_raw = Some(d);
        }
    }
    for d in &mut ex.dates {
        d.valid = is_date(&d.date);
    }
    ex.dates.retain(|d| !d.date.trim().is_empty());
    ex.addresses.retain(|a| !a.address.trim().is_empty());
    ex.reference_numbers.retain(|r| !r.value.trim().is_empty());
    ex.line_items.retain(|i| !i.description.trim().is_empty());
    for i in &mut ex.line_items {
        i.details = i.details.take().filter(|d| !d.trim().is_empty());
        i.adds_up = match (i.unit_price, i.total) {
            (Some(price), Some(total)) => {
                let expected = i.quantity.unwrap_or(1.0) * price - i.discount.unwrap_or(0.0);
                Some(close(expected, total))
            }
            _ => None,
        };
    }
    let totals: Vec<f64> = ex.line_items.iter().filter_map(|i| i.total).collect();
    ex.line_items_check = (!totals.is_empty() && totals.len() == ex.line_items.len()).then(|| {
        let sum = (totals.iter().sum::<f64>() * 100.0).round() / 100.0;
        ItemsCheck {
            sum,
            matches: ex.amounts.iter().find(|a| close(a.value, sum)).map(|a| a.label.clone()),
        }
    });
}

/// What kind of document this is and who issued it, e.g.
/// "south-african-tax-invoice|istore": a generic description alone would lump
/// different shops' invoices together.
pub fn filing_key(doc_type_detail: &str, parties: &[Party]) -> String {
    let issuer = parties
        .iter()
        .find(|p| matches!(p.role.as_str(), "issuer" | "supplier" | "shipper"))
        .and_then(|p| p.name.as_deref())
        .map(normalize_tag)
        .unwrap_or_default();
    format!("{}|{issuer}", normalize_tag(doc_type_detail))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.015
}

/// "Motor Vehicle_Licence " -> "motor-vehicle-licence"
pub fn normalize_tag(t: &str) -> String {
    t.trim()
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
