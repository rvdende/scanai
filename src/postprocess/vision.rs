//! Page orientation and OCR (PP-LCNet orientation classifier + PP-OCRv6, via oar-ocr).

use anyhow::{Context, Result};
use image::{RgbImage, imageops};
use oar_ocr::predictors::DocumentOrientationPredictor;
use oar_ocr::prelude::*;

pub struct Vision {
    orientation: DocumentOrientationPredictor,
    ocr: OAROCR,
}

pub struct Orientation {
    /// Degrees the page was turned clockwise to make it upright.
    pub rotated_cw: u32,
    pub score: f32,
}

pub struct Line {
    pub text: String,
    pub confidence: f32,
    /// Axis-aligned box in the upright page: (left, top, right, bottom) pixels.
    pub bbox: (f32, f32, f32, f32),
    /// Slope of the text line in degrees (positive = falling to the right).
    pub angle: f32,
}

impl Vision {
    /// Model files are fetched into ~/.oar on first use.
    pub fn load() -> Result<Self> {
        let orientation = DocumentOrientationPredictor::builder()
            .topk(4)
            .build("pp-lcnet_x1_0_doc_ori.onnx")
            .context("loading the orientation model")?;
        let ocr = OAROCRBuilder::pp_ocrv6(PpOcrV6Size::Small)
            .build()
            .context("loading the OCR models")?;
        Ok(Self { orientation, ocr })
    }

    pub fn orient(&self, page: &RgbImage) -> Result<Orientation> {
        // A small slip on a mostly empty page is unreadable once the classifier shrinks
        // the page to a thumbnail, so look only at the part with ink on it.
        let (x, y, w, h) = content_box(page);
        let content = imageops::crop_imm(page, x, y, w, h).to_image();
        let on_content = self.classify(&content)?;
        if (w, h) == page.dimensions() {
            return Ok(on_content);
        }
        // A long thin slip can read worse cropped than with the page around it, so
        // ask about both and trust whichever answer is more certain.
        let on_page = self.classify(page)?;
        log::debug!(
            "orientation: content says {}° ({:.2}), whole page says {}° ({:.2})",
            on_content.rotated_cw,
            on_content.score,
            on_page.rotated_cw,
            on_page.score
        );
        Ok(if on_content.score >= on_page.score { on_content } else { on_page })
    }

    fn classify(&self, page: &RgbImage) -> Result<Orientation> {
        let out = self.orientation.predict(vec![page.clone()])?;
        let best = out
            .orientations
            .first()
            .and_then(|c| c.iter().max_by(|a, b| a.score.total_cmp(&b.score)))
            .context("orientation model returned nothing")?;
        // The label is how far the page is turned clockwise, so undo it by turning
        // counter-clockwise. (oar-ocr's own apply_document_orientation turns the
        // other way, which left a 90° page upside down in testing.)
        let label: u32 = best.label.parse().unwrap_or(0);
        Ok(Orientation {
            rotated_cw: (360 - label) % 360,
            score: best.score,
        })
    }

    pub fn ocr(&self, upright: &RgbImage) -> Result<Vec<Line>> {
        let out = self.ocr.predict(vec![upright.clone()])?;
        let mut lines: Vec<Line> = out
            .first()
            .map(|r| {
                r.text_regions
                    .iter()
                    .filter_map(|reg| {
                        let (text, confidence) = reg.text_with_confidence()?;
                        let pts = &reg.bounding_box.points;
                        let xs = pts.iter().map(|p| p.x);
                        let ys = pts.iter().map(|p| p.y);
                        Some(Line {
                            text: text.to_string(),
                            confidence,
                            angle: text_angle(pts.iter().map(|p| (p.x, p.y)).collect()),
                            bbox: (
                                xs.clone().fold(f32::MAX, f32::min),
                                ys.clone().fold(f32::MAX, f32::min),
                                xs.fold(f32::MIN, f32::max),
                                ys.fold(f32::MIN, f32::max),
                            ),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        reading_order(&mut lines);
        Ok(lines)
    }
}

pub fn rotate_cw(img: RgbImage, degrees: u32) -> RgbImage {
    match degrees {
        90 => imageops::rotate90(&img),
        180 => imageops::rotate180(&img),
        270 => imageops::rotate270(&img),
        _ => img,
    }
}

/// The region of the page holding most of the ink, ignoring specks of dust:
/// (x, y, width, height), with a margin. The whole page if it's blank.
pub fn content_box(img: &RgbImage) -> (u32, u32, u32, u32) {
    let (w, h) = img.dimensions();
    let dark = |x: u32, y: u32| {
        let p = img.get_pixel(x, y).0;
        (p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000 < 160
    };
    let mut rows = vec![0u32; h as usize];
    let mut cols = vec![0u32; w as usize];
    for y in 0..h {
        for x in 0..w {
            if dark(x, y) {
                rows[y as usize] += 1;
                cols[x as usize] += 1;
            }
        }
    }
    // The band with the most ink, where gaps shorter than `gap` don't split a band.
    fn band(counts: &[u32], gap: usize) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize, u64)> = None;
        let mut cur: Option<(usize, usize, u64)> = None;
        for (i, &c) in counts.iter().enumerate() {
            if c < 2 {
                continue;
            }
            cur = match cur {
                Some((s, e, n)) if i - e <= gap => Some((s, i, n + c as u64)),
                _ => {
                    if let Some(c0) = cur && best.is_none_or(|b| c0.2 > b.2) {
                        best = Some(c0);
                    }
                    Some((i, i, c as u64))
                }
            };
        }
        if let Some(c0) = cur && best.is_none_or(|b| c0.2 > b.2) {
            best = Some(c0);
        }
        best.map(|(s, e, _)| (s, e))
    }
    let (Some((top, bot)), Some((left, right))) = (band(&rows, 150), band(&cols, 150)) else {
        return (0, 0, w, h);
    };
    let m = 40;
    let (x0, y0) = ((left as u32).saturating_sub(m), (top as u32).saturating_sub(m));
    let (x1, y1) = ((right as u32 + m).min(w - 1), (bot as u32 + m).min(h - 1));
    if x1 - x0 < 100 || y1 - y0 < 100 {
        return (0, 0, w, h);
    }
    (x0, y0, x1 - x0 + 1, y1 - y0 + 1)
}

/// The box around all confidently read text, plus `margin`: (x, y, width, height).
pub fn text_box(img: &RgbImage, lines: &[Line], margin: f32) -> Option<(u32, u32, u32, u32)> {
    let read: Vec<&Line> = lines
        .iter()
        .filter(|l| l.confidence > 0.6 && l.text.chars().filter(|c| c.is_alphanumeric()).count() >= 2)
        .collect();
    if read.is_empty() {
        return None;
    }
    let (w, h) = (img.width() as f32, img.height() as f32);
    for l in &read {
        log::debug!("text_box: {:?} at {:?} ({:.2})", l.text, l.bbox, l.confidence);
    }
    let l = (read.iter().map(|l| l.bbox.0).fold(f32::MAX, f32::min) - margin).max(0.0);
    let t = (read.iter().map(|l| l.bbox.1).fold(f32::MAX, f32::min) - margin).max(0.0);
    let r = (read.iter().map(|l| l.bbox.2).fold(f32::MIN, f32::max) + margin).min(w);
    let b = (read.iter().map(|l| l.bbox.3).fold(f32::MIN, f32::max) + margin).min(h);
    (r - l >= 50.0 && b - t >= 50.0).then(|| (l as u32, t as u32, (r - l) as u32, (b - t) as u32))
}

/// Group lines into rows (top to bottom), left to right within a row.
fn reading_order(lines: &mut [Line]) {
    lines.sort_by(|a, b| a.bbox.1.total_cmp(&b.bbox.1));
    let mut row = 0usize;
    let mut row_top = f32::MIN;
    let mut row_h = 0f32;
    let mut keys = Vec::with_capacity(lines.len());
    for l in lines.iter() {
        let h = (l.bbox.3 - l.bbox.1).max(1.0);
        if l.bbox.1 - row_top > 0.5 * row_h.max(h) {
            row += 1;
            row_top = l.bbox.1;
            row_h = h;
        }
        keys.push(row);
    }
    let mut idx: Vec<usize> = (0..lines.len()).collect();
    idx.sort_by(|&a, &b| keys[a].cmp(&keys[b]).then(lines[a].bbox.0.total_cmp(&lines[b].bbox.0)));
    let sorted: Vec<Line> = idx
        .into_iter()
        .map(|i| std::mem::replace(&mut lines[i], Line { text: String::new(), confidence: 0.0, angle: 0.0, bbox: (0.0, 0.0, 0.0, 0.0) }))
        .collect();
    for (slot, l) in lines.iter_mut().zip(sorted) {
        *slot = l;
    }
}

/// Plain text, one visual row per line.
pub fn as_text(lines: &[Line]) -> String {
    let mut out = String::new();
    let mut row_top: Option<f32> = None;
    for l in lines {
        let h = (l.bbox.3 - l.bbox.1).max(1.0);
        match row_top {
            Some(t) if (l.bbox.1 - t).abs() < 0.5 * h => out.push_str("  "),
            _ => {
                if row_top.is_some() {
                    out.push('\n');
                }
                row_top = Some(l.bbox.1);
            }
        }
        out.push_str(&l.text);
    }
    out.push('\n');
    out
}

/// Direction of the longest edge of a text box, folded into (-45°, 45°].
fn text_angle(pts: Vec<(f32, f32)>) -> f32 {
    let n = pts.len();
    let Some((dx, dy)) = (0..n)
        .map(|i| (pts[(i + 1) % n].0 - pts[i].0, pts[(i + 1) % n].1 - pts[i].1))
        .max_by(|a, b| (a.0 * a.0 + a.1 * a.1).total_cmp(&(b.0 * b.0 + b.1 * b.1)))
    else {
        return 0.0;
    };
    let mut a = dy.atan2(dx).to_degrees();
    while a > 45.0 {
        a -= 90.0;
    }
    while a <= -45.0 {
        a += 90.0;
    }
    a
}

/// How crooked the text is, in degrees (positive = falling to the right): the
/// median slope of the longer lines. None when there's too little text to tell.
pub fn skew(lines: &[Line]) -> Option<f32> {
    let mut angles: Vec<f32> = lines
        .iter()
        .filter(|l| l.confidence > 0.8 && l.bbox.2 - l.bbox.0 > 150.0 && l.text.chars().count() >= 5)
        .map(|l| l.angle)
        .collect();
    if angles.len() < 5 {
        return None;
    }
    angles.sort_by(f32::total_cmp);
    Some(angles[angles.len() / 2])
}

/// Turn the page `degrees` counter-clockwise (undoing a clockwise skew), on a canvas
/// grown to fit, filling the new corners with white.
pub fn deskew(img: &RgbImage, degrees: f32) -> RgbImage {
    let a = degrees.to_radians();
    let (s, c) = a.sin_cos();
    let (w, h) = (img.width() as f32, img.height() as f32);
    let (ow, oh) = ((w * c.abs() + h * s.abs()).ceil(), (w * s.abs() + h * c.abs()).ceil());
    let (icx, icy, ocx, ocy) = (w / 2.0, h / 2.0, ow / 2.0, oh / 2.0);
    RgbImage::from_fn(ow as u32, oh as u32, |x, y| {
        // Output pixel → source pixel: rotate by the skew around the centres.
        let (u, v) = (x as f32 + 0.5 - ocx, y as f32 + 0.5 - ocy);
        let (sx, sy) = (icx + c * u - s * v - 0.5, icy + s * u + c * v - 0.5);
        let (x0, y0) = (sx.floor(), sy.floor());
        if x0 < 0.0 || y0 < 0.0 || x0 + 1.0 >= w || y0 + 1.0 >= h {
            return image::Rgb([255, 255, 255]);
        }
        let (fx, fy) = (sx - x0, sy - y0);
        let (x0, y0) = (x0 as u32, y0 as u32);
        let p = |dx, dy| img.get_pixel(x0 + dx, y0 + dy).0;
        let (p00, p10, p01, p11) = (p(0, 0), p(1, 0), p(0, 1), p(1, 1));
        let mut out = [0u8; 3];
        for k in 0..3 {
            let top = p00[k] as f32 * (1.0 - fx) + p10[k] as f32 * fx;
            let bot = p01[k] as f32 * (1.0 - fx) + p11[k] as f32 * fx;
            out[k] = (top * (1.0 - fy) + bot * fy).round() as u8;
        }
        image::Rgb(out)
    })
}

/// The box around all the ink on the page — drawings, stamps and handwriting as well
/// as text — ignoring isolated specks of dust: (x, y, width, height).
pub fn ink_box(img: &RgbImage) -> Option<(u32, u32, u32, u32)> {
    const CELL: u32 = 8;
    let (cw, ch) = ((img.width() / CELL) as usize, (img.height() / CELL) as usize);
    let mut ink = vec![false; cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            'cell: for y in 0..CELL {
                for x in 0..CELL {
                    let p = img.get_pixel(cx as u32 * CELL + x, cy as u32 * CELL + y).0;
                    if (p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000 < 170 {
                        ink[cy * cw + cx] = true;
                        break 'cell;
                    }
                }
            }
        }
    }
    // Only sizeable connected marks count (a drawing's strokes, a stamp); a stray
    // mark or speck of dust doesn't. Cells within one of each other join up.
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    let mut seen = vec![false; cw * ch];
    let mut stack = Vec::new();
    for start in 0..cw * ch {
        if !ink[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        let mut members = Vec::new();
        while let Some(i) = stack.pop() {
            members.push(i);
            let (cx, cy) = (i % cw, i / cw);
            for ny in cy.saturating_sub(2)..=(cy + 2).min(ch - 1) {
                for nx in cx.saturating_sub(2)..=(cx + 2).min(cw - 1) {
                    let j = ny * cw + nx;
                    if ink[j] && !seen[j] {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        if members.len() >= 60 {
            for i in members {
                let (cx, cy) = (i % cw, i / cw);
                (x0, y0, x1, y1) = (x0.min(cx), y0.min(cy), x1.max(cx), y1.max(cy));
            }
        }
    }
    (x0 != usize::MAX).then(|| {
        let (x0, y0) = (x0 as u32 * CELL, y0 as u32 * CELL);
        (x0, y0, (x1 as u32 + 1) * CELL - x0, (y1 as u32 + 1) * CELL - y0)
    })
}
