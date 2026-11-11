//! Join scans of the same piece of paper (e.g. a long till slip scanned in two
//! overlapping halves) by aligning the OCR lines they have in common.
//!
//! The paper is rarely put down at exactly the same angle twice, so the second
//! scan is fitted with a rotation plus a shift (RANSAC over matched line centres,
//! then a least-squares refit), and resampled into the first scan's frame.

use std::collections::HashMap;

use image::{Rgb, RgbImage};

use super::vision::Line;

/// Maps a point in `b` to `a`: a = R(angle)·b + (tx, ty).
#[derive(Debug, Clone, Copy)]
pub struct Transform {
    pub angle: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Transform {
    fn apply(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let (s, c) = self.angle.sin_cos();
        (c * x - s * y + self.tx, s * x + c * y + self.ty)
    }

    fn invert(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let (s, c) = self.angle.sin_cos();
        let (x, y) = (x - self.tx, y - self.ty);
        (c * x + s * y, -s * x + c * y)
    }
}

pub struct Alignment {
    pub transform: Transform,
    /// Distinctive lines found in both that agree with the transform.
    pub matched: usize,
    /// Share of `a`'s lines with numbers in them, inside the overlap, that `b` also
    /// has (1.0 = all). Numbers (amounts, dates, document numbers) are what tell two
    /// copies of the same printed form apart, so only they count.
    pub agreement: f32,
    /// True when `b` shows nothing `a` doesn't already have.
    pub duplicate: bool,
}

const MIN_MATCHES: usize = 4;
/// Matching lines that contain digits and agree with the transform.
const MIN_NUMBER_MATCHES: usize = 2;
const MIN_AGREEMENT: f32 = 0.6;
/// Numbers reading differently at the same spot that OCR slips can explain.
const MAX_CONFLICTS: usize = 2;
/// Pixels (at 300 dpi) a matched line may be off from where the transform puts it.
const TOLERANCE: f32 = 25.0;
/// Paper put down more crooked than this is not considered.
const MAX_ANGLE_DEG: f32 = 20.0;

fn key(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn has_digit(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit())
}

fn center(l: &Line) -> (f32, f32) {
    ((l.bbox.0 + l.bbox.2) / 2.0, (l.bbox.1 + l.bbox.3) / 2.0)
}

/// Lines that are long enough to be telling and appear only once on the page.
fn distinctive(lines: &[Line]) -> HashMap<String, &Line> {
    let mut seen: HashMap<String, Option<&Line>> = HashMap::new();
    for l in lines {
        let k = key(&l.text);
        if k.chars().count() < 6 {
            continue;
        }
        seen.entry(k).and_modify(|e| *e = None).or_insert(Some(l));
    }
    seen.into_iter().filter_map(|(k, v)| Some((k, v?))).collect()
}

/// Least-squares rotation + shift taking the `b` points onto the `a` points.
fn fit(pairs: &[((f32, f32), (f32, f32))]) -> Transform {
    let n = pairs.len() as f32;
    let (mut ax, mut ay, mut bx, mut by) = (0.0, 0.0, 0.0, 0.0);
    for ((pax, pay), (pbx, pby)) in pairs {
        ax += pax;
        ay += pay;
        bx += pbx;
        by += pby;
    }
    let (ax, ay, bx, by) = (ax / n, ay / n, bx / n, by / n);
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for ((pax, pay), (pbx, pby)) in pairs {
        let (u, v) = (pbx - bx, pby - by);
        let (p, q) = (pax - ax, pay - ay);
        sxx += u * p + v * q;
        sxy += u * q - v * p;
    }
    let angle = sxy.atan2(sxx);
    let (s, c) = angle.sin_cos();
    Transform { angle, tx: ax - (c * bx - s * by), ty: ay - (s * bx + c * by) }
}

/// Numbers both pages print at the same spot (after mapping `b` into `a`'s frame):
/// how many read the same digits, and how many differ.
fn digits_at_same_place(a: &[Line], b: &[Line], t: &Transform) -> (usize, usize) {
    let digits = |s: &str| -> String { s.chars().filter(|c| c.is_ascii_digit()).collect() };
    let a_nums: Vec<(&Line, String)> =
        a.iter().map(|l| (l, digits(&l.text))).filter(|(_, d)| d.len() >= 3).collect();
    let (mut same, mut differ) = (0, 0);
    for lb in b {
        let db = digits(&lb.text);
        if db.len() < 3 {
            continue;
        }
        let (cx, cy) = t.apply(center(lb));
        let (hw, hh) = ((lb.bbox.2 - lb.bbox.0) / 2.0, (lb.bbox.3 - lb.bbox.1) / 2.0);
        let (l, top, r, bot) = (cx - hw, cy - hh, cx + hw, cy + hh);
        // The a line overlapping this one the most, needing half the height in common.
        let hit = a_nums
            .iter()
            .filter_map(|(la, da)| {
                let w = r.min(la.bbox.2) - l.max(la.bbox.0);
                let h = bot.min(la.bbox.3) - top.max(la.bbox.1);
                let min_h = (bot - top).min(la.bbox.3 - la.bbox.1);
                (w > 0.0 && h > 0.5 * min_h).then_some((w * h, da))
            })
            .max_by(|x, y| x.0.total_cmp(&y.0));
        match hit {
            Some((_, da)) if *da == db => same += 1,
            Some(_) => differ += 1,
            None => {}
        }
    }
    (same, differ)
}

fn dist(p: (f32, f32), q: (f32, f32)) -> f32 {
    ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt()
}

fn span(points: impl Iterator<Item = (f32, f32)>) -> (f32, f32) {
    points.fold((f32::MAX, f32::MIN), |(t, b), (_, y)| (t.min(y), b.max(y)))
}

/// Do these two upright pages show overlapping parts of the same paper?
pub fn align(a: &[Line], b: &[Line]) -> Option<Alignment> {
    let (da, db) = (distinctive(a), distinctive(b));
    let pairs: Vec<(&Line, &Line)> = da
        .iter()
        .filter_map(|(k, la)| Some((*la, *db.get(k)?)))
        .collect();
    log::debug!("align: {} distinctive lines in common", pairs.len());
    if pairs.len() < MIN_MATCHES {
        return None;
    }
    let pts: Vec<((f32, f32), (f32, f32))> = pairs.iter().map(|(la, lb)| (center(la), center(lb))).collect();
    let inliers_of = |t: &Transform| -> Vec<usize> {
        (0..pts.len()).filter(|&i| dist(t.apply(pts[i].1), pts[i].0) < TOLERANCE).collect()
    };

    // RANSAC: every pair of matches proposes a transform; keep the best supported one.
    let mut best: Vec<usize> = Vec::new();
    for i in 0..pts.len() {
        for j in i + 1..pts.len() {
            // Two points too close together give an unreliable angle.
            if dist(pts[i].1, pts[j].1) < 200.0 {
                continue;
            }
            let t = fit(&[pts[i], pts[j]]);
            if t.angle.abs().to_degrees() > MAX_ANGLE_DEG {
                continue;
            }
            let inl = inliers_of(&t);
            if inl.len() > best.len() {
                best = inl;
            }
        }
    }
    if best.len() < 2 {
        return None;
    }
    let transform = fit(&best.iter().map(|&i| pts[i]).collect::<Vec<_>>());
    let inliers = inliers_of(&transform);
    let number_matches = inliers.iter().filter(|&&i| has_digit(&pairs[i].0.text)).count();
    log::debug!(
        "align: rotation {:.2}°, shift ({:.0}, {:.0}); {} lines agree, {number_matches} with numbers",
        transform.angle.to_degrees(),
        transform.tx,
        transform.ty,
        inliers.len()
    );
    if inliers.len() < MIN_MATCHES || number_matches < MIN_NUMBER_MATCHES {
        return None;
    }

    // Overlap: the vertical span both pages' text covers, in a's frame.
    let (a_top, a_bot) = span(a.iter().map(center));
    let (b_top, b_bot) = span(b.iter().map(|l| transform.apply(center(l))));
    let (lo, hi) = (a_top.max(b_top), a_bot.min(b_bot));
    let in_overlap = da
        .values()
        .filter(|l| {
            let y = center(l).1;
            has_digit(&l.text) && y > lo && y < hi
        })
        .count();
    let agreement = number_matches as f32 / in_overlap.max(1) as f32;
    log::debug!("align: {number_matches} of {in_overlap} number lines in the overlap match");
    if agreement < MIN_AGREEMENT {
        return None;
    }
    // Two documents printed from the same template share their header numbers (tax
    // and phone numbers, account, date) but differ in the invoice number and amounts,
    // printed in the same places. A rescan of the same paper reads the same digits there.
    let (same, conflicts) = digits_at_same_place(a, b, &transform);
    log::debug!("align: numbers in the same place: {same} agree, {conflicts} differ");
    if conflicts > MAX_CONFLICTS && conflicts * 3 > same {
        log::info!(
            "not stitching: {conflicts} numbers differ at the same spot ({same} agree), so it's a different document on the same form"
        );
        return None;
    }
    Some(Alignment {
        transform,
        matched: inliers.len(),
        agreement: agreement.min(1.0),
        duplicate: b_top >= a_top - 20.0 && b_bot <= a_bot + 20.0,
    })
}

/// Join `b` onto `a`, resampling `b` into `a`'s frame and cutting over from one
/// to the other in the middle of the overlap.
pub fn compose(a: &RgbImage, b: &RgbImage, al: &Alignment, a_lines: &[Line], b_lines: &[Line]) -> RgbImage {
    let t = al.transform;
    let (bw, bh) = (b.width() as f32, b.height() as f32);
    let corners = [(0.0, 0.0), (bw, 0.0), (0.0, bh), (bw, bh)].map(|p| t.apply(p));
    let x0 = corners.iter().map(|p| p.0).fold(0.0f32, f32::min).floor() as i64;
    let y0 = corners.iter().map(|p| p.1).fold(0.0f32, f32::min).floor() as i64;
    let x1 = corners.iter().map(|p| p.0).fold(a.width() as f32, f32::max).ceil() as i64;
    let y1 = corners.iter().map(|p| p.1).fold(a.height() as f32, f32::max).ceil() as i64;

    let (a_top, a_bot) = span(a_lines.iter().map(center));
    let (b_top, b_bot) = span(b_lines.iter().map(|l| t.apply(center(l))));
    let seam = ((a_top.max(b_top) + a_bot.min(b_bot)) / 2.0) as i64;
    let a_on_top = a_top <= b_top;

    let from_a = |x: i64, y: i64| -> Option<Rgb<u8>> {
        (x >= 0 && y >= 0 && x < a.width() as i64 && y < a.height() as i64)
            .then(|| *a.get_pixel(x as u32, y as u32))
    };
    let from_b = |x: i64, y: i64| -> Option<Rgb<u8>> {
        let (bx, by) = t.invert((x as f32 + 0.5, y as f32 + 0.5));
        (bx >= 0.0 && by >= 0.0 && bx < bw && by < bh).then(|| *b.get_pixel(bx as u32, by as u32))
    };
    let mut out = RgbImage::from_pixel((x1 - x0) as u32, (y1 - y0) as u32, Rgb([255, 255, 255]));
    for y in y0..y1 {
        let a_first = (y < seam) == a_on_top;
        for x in x0..x1 {
            let p = if a_first {
                from_a(x, y).or_else(|| from_b(x, y))
            } else {
                from_b(x, y).or_else(|| from_a(x, y))
            };
            if let Some(p) = p {
                out.put_pixel((x - x0) as u32, (y - y0) as u32, p);
            }
        }
    }
    out
}

/// Crop to the area with text (plus a margin), for sending to the vision model.
pub fn crop_to_text(img: &RgbImage, lines: &[Line], margin: f32) -> RgbImage {
    if lines.is_empty() {
        return img.clone();
    }
    let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for ln in lines {
        l = l.min(ln.bbox.0);
        t = t.min(ln.bbox.1);
        r = r.max(ln.bbox.2);
        b = b.max(ln.bbox.3);
    }
    let x = (l - margin).max(0.0) as u32;
    let y = (t - margin).max(0.0) as u32;
    let w = ((r + margin) as u32).min(img.width()).saturating_sub(x).max(1);
    let h = ((b + margin) as u32).min(img.height()).saturating_sub(y).max(1);
    image::imageops::crop_imm(img, x, y, w, h).to_image()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: Transform = Transform { angle: 0.0, tx: 0.0, ty: 0.0 };

    /// Lines 120 px apart, laid out in page space and then placed with `t`.
    fn slip(texts: &[&str], t: Transform) -> Vec<Line> {
        texts
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (cx, cy) = t.apply((500.0, 100.0 + 120.0 * i as f32));
                Line { text: (*s).into(), confidence: 0.9, angle: 0.0, bbox: (cx - 150.0, cy - 15.0, cx + 150.0, cy + 15.0) }
            })
            .collect()
    }

    const SLIP: &[&str] = &[
        "Builders a division of", "Reg No: 2008/011666/07", "TAX INVOICE", "Document Number: B00001",
        "SCAN TO RATE US", "Item/GTIN 6009553200008", "BUILDERS EURO CYLINDER", "1145,00 C",
        "CARRIER BAG", "TOTAL [6] ZAR 1146,20", "Terminal Payment", "RRN:000000000001",
        "VAT AMT R 149,50", "Thank you for shopping",
    ];

    #[test]
    fn rotated_overlapping_halves_align() {
        // a: the top 11 lines, square on the glass.
        let a = slip(&SLIP[..11], ID);
        // b: from "TAX INVOICE" down, put down 3° crooked and further down the glass.
        // In a's frame, b's line k sits where a's line k+2 is; b's own frame maps there by `truth`.
        let truth = Transform { angle: 3f32.to_radians(), tx: 40.0, ty: 240.0 };
        let b: Vec<Line> = SLIP[2..]
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let (bx, by) = truth.invert((500.0, 100.0 + 120.0 * (k + 2) as f32));
                Line { text: (*s).into(), confidence: 0.9, angle: 0.0, bbox: (bx - 150.0, by - 15.0, bx + 150.0, by + 15.0) }
            })
            .collect();
        let al = align(&a, &b).expect("should align");
        assert!((al.transform.angle.to_degrees() - 3.0).abs() < 0.2, "{:?}", al.transform);
        assert!((al.transform.tx - 40.0).abs() < 2.0 && (al.transform.ty - 240.0).abs() < 2.0, "{:?}", al.transform);
        assert!(!al.duplicate);
    }

    #[test]
    fn two_copies_of_the_same_form_dont() {
        let form = |reg: &str, vin: &str, date: &str| {
            slip(&["Republic of South Africa", "MOTOR VEHICLE LICENCE", "(National Road Traffic Act, 1996)",
                "Vehicle register number", reg, "Vehicle identification number", vin,
                "Date of expiry", date, "Engine number"], ID)
        };
        let a = form("TST123W", "XXXFD0000000001", "2023-10-31");
        let b = form("CA555001", "XXXZZZ0RZXX00002", "2022-10-31");
        assert!(align(&a, &b).is_none());
    }

    #[test]
    fn invoices_on_the_same_template_same_day_dont() {
        let inv = |no: &str, item: &str, qty: &str, price: &str, tax: &str, total: &str, excl: &str| {
            slip(&["EXAMPLE TYRES (PTY) Ltd", "CASH SALES SEPTEMBER 2021", "Tax Registration 4000000000",
                "Telephone 000 5550100", "Fax 0005550101", "Company Reg. no. 2000/000000/07",
                "CASH0921", "2021/09/27", no, item, qty, price, tax, total,
                "REDISA FEE IS INCLUDED", excl, tax, total, "Discount 0.00", total,
                "ACC.NO.0000000000", "FAX PROOF OF PAYMENT TO: 0005550101"], ID)
        };
        let a = inv("INV000101", "CON 245/70R16 CONTI TRAC", "2.00", "2050.00", "534.78", "4 100.00", "3 565.22");
        let b = inv("INV000102", "WHEELALIGMENT/WIELSPORIN", "1.00", "300.00", "39.13", "300.00", "260.87");
        assert!(align(&a, &b).is_none());
    }

    #[test]
    fn rescan_of_the_same_page_is_a_duplicate() {
        let a = slip(SLIP, ID);
        let b = slip(SLIP, Transform { angle: -0.02, tx: 30.0, ty: 40.0 });
        let al = align(&a, &b).expect("same paper");
        assert!(al.duplicate);
    }

    #[test]
    fn different_slips_from_same_shop_dont() {
        let a = slip(&["Builders a division of", "Reg No: 2008/011666/07", "TAX INVOICE", "Document Number: B111",
            "GARDEN HOSE 20M", "TOTAL [2] ZAR 300,00", "Terminal Payment", "RRN:5550001"], ID);
        let b = slip(&["Builders a division of", "Reg No: 2008/011666/07", "TAX INVOICE", "Document Number: B222",
            "PAINT ROLLER", "TOTAL [1] ZAR 89,00", "Terminal Payment", "RRN:5550002"], ID);
        assert!(align(&a, &b).is_none());
    }
}
