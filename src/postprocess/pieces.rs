//! Finding separate pieces of paper laid on the scanner glass together.
//!
//! The lid behind the paper scans as the brightest white on the page; paper is a
//! shade darker (and often has a faint shadow along its edges). So: shrink the page,
//! mark everything noticeably darker than the lid, close small gaps, and take each
//! large, solidly filled connected region as one piece of paper.

use image::{GrayImage, RgbImage};

/// Downscale factor for the analysis (300 dpi → 75 dpi).
const SCALE: u32 = 4;

#[derive(Clone)]
pub struct Piece {
    /// (x, y, width, height) in full-resolution page pixels.
    pub region: (u32, u32, u32, u32),
    /// Share of the region's box that is paper (1.0 = a solid rectangle).
    pub fill: f32,
    /// Which analysis cells belong to this piece.
    cells: Vec<bool>,
    cells_w: usize,
}

/// Brightness map at 1/SCALE, each cell the mean luma of its block.
fn shrink(img: &RgbImage) -> GrayImage {
    let (w, h) = (img.width() / SCALE, img.height() / SCALE);
    GrayImage::from_fn(w, h, |x, y| {
        let mut sum = 0u32;
        for dy in 0..SCALE {
            for dx in 0..SCALE {
                let p = img.get_pixel(x * SCALE + dx, y * SCALE + dy).0;
                sum += (p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000;
            }
        }
        image::Luma([(sum / (SCALE * SCALE)) as u8])
    })
}

/// The lid's white: a high percentile of the page's brightness.
fn lid_level(g: &GrayImage) -> u8 {
    let mut hist = [0u64; 256];
    for p in g.pixels() {
        hist[p.0[0] as usize] += 1;
    }
    let total: u64 = hist.iter().sum();
    let mut seen = 0;
    for v in (0..256).rev() {
        seen += hist[v];
        if seen * 100 >= total * 2 {
            return v as u8;
        }
    }
    255
}

fn dilate(m: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    // Separable square dilation.
    let mut tmp = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let (a, b) = (x.saturating_sub(r), (x + r).min(w - 1));
            tmp[y * w + x] = (a..=b).any(|i| m[y * w + i]);
        }
    }
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let (a, b) = (y.saturating_sub(r), (y + r).min(h - 1));
            out[y * w + x] = (a..=b).any(|j| tmp[j * w + x]);
        }
    }
    out
}

fn erode(m: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    let inv: Vec<bool> = m.iter().map(|v| !v).collect();
    dilate(&inv, w, h, r).into_iter().map(|v| !v).collect()
}

/// Paper mask at 1/SCALE resolution (true = paper).
pub fn paper_mask(img: &RgbImage) -> (Vec<bool>, usize, usize, u8) {
    let g = shrink(img);
    let (w, h) = (g.width() as usize, g.height() as usize);
    let lid = lid_level(&g);
    let thr = lid.saturating_sub(6);
    let raw: Vec<bool> = g.pixels().map(|p| p.0[0] < thr).collect();
    // Close (fill text and speckle inside paper), then open (drop dust and thin lines).
    let closed = erode(&dilate(&raw, w, h, 3), w, h, 3);
    let opened = dilate(&erode(&closed, w, h, 4), w, h, 4);
    (opened, w, h, lid)
}

/// Connected regions of the mask: (x0, y0, x1, y1, area) in mask cells.
fn components(m: &[bool], w: usize, h: usize) -> Vec<(usize, usize, usize, usize, usize)> {
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !m[start] || seen[start] {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1, mut area) = (w, h, 0, 0, 0);
        seen[start] = true;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            area += 1;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            let mut push = |j: usize| {
                if m[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 {
                push(i - 1);
            }
            if x + 1 < w {
                push(i + 1);
            }
            if y > 0 {
                push(i - w);
            }
            if y + 1 < h {
                push(i + w);
            }
        }
        out.push((x0, y0, x1, y1, area));
    }
    out
}

/// The separate pieces of paper on the page, largest first. Empty unless the page
/// clearly holds two or more pieces, each a solid region, that between them contain
/// all the ink on the page; a single document is never cut up.
pub fn find(img: &RgbImage) -> Vec<Piece> {
    let (m, w, h, lid) = paper_mask(img);
    let ink: Vec<bool> = shrink(img).pixels().map(|p| p.0[0] < lid.saturating_sub(60)).collect();
    // Pieces lying close together can touch in the mask (shadows, the glass edge), so
    // erode the mask to break such bridges, harder each try. The page edge counts as
    // background, so bridges running along it break too.
    for separate in [8, 14, 20] {
        let mut framed = m.clone();
        for x in 0..w {
            framed[x] = false;
            framed[(h - 1) * w + x] = false;
        }
        for y in 0..h {
            framed[y * w] = false;
            framed[y * w + w - 1] = false;
        }
        let sep = erode(&framed, w, h, separate);
        if let Some(p) = pieces_from(&m, &sep, &ink, w, h, separate, lid, img) {
            return p;
        }
    }
    Vec::new()
}

#[allow(clippy::too_many_arguments)]
fn pieces_from(
    m: &[bool],
    sep: &[bool],
    ink: &[bool],
    w: usize,
    h: usize,
    separate: usize,
    lid: u8,
    img: &RgbImage,
) -> Option<Vec<Piece>> {
    let page = (w * h) as f32;
    let mut kept = Vec::new();
    for (x0, y0, x1, y1, area) in components(sep, w, h) {
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
        let fill = area as f32 / (bw * bh) as f32;
        log::debug!(
            "pieces (separate {separate}): region {bw}x{bh} cells at ({x0},{y0}), {:.1}% of page, fill {fill:.2} (lid {lid})",
            100.0 * area as f32 / page
        );
        // Paper: a sizeable, mostly solid region at least ~1" across.
        if area as f32 >= 0.005 * page && fill >= 0.6 && bw.min(bh) + 2 * separate >= 75 {
            kept.push((x0, y0, x1, y1, fill));
        }
    }
    if kept.len() < 2 {
        return None;
    }
    // Grow each piece back by the erosion (plus a margin) over the paper mask, so each
    // gets its whole sheet; contested cells go to the nearer piece's box.
    let grow = separate + 3;
    let mut owner = vec![usize::MAX; w * h];
    let mut best_d = vec![usize::MAX; w * h];
    for (k, &(x0, y0, x1, y1, _)) in kept.iter().enumerate() {
        for y in y0.saturating_sub(grow)..=(y1 + grow).min(h - 1) {
            for x in x0.saturating_sub(grow)..=(x1 + grow).min(w - 1) {
                let i = y * w + x;
                let inside = x >= x0 && x <= x1 && y >= y0 && y <= y1;
                if !(m[i] || ink[i] || inside) {
                    continue;
                }
                let d = x0.saturating_sub(x).max(x.saturating_sub(x1)).max(y0.saturating_sub(y)).max(y.saturating_sub(y1));
                if d < best_d[i] {
                    best_d[i] = d;
                    owner[i] = k;
                }
            }
        }
    }
    // Every bit of ink on the page must belong to some piece.
    let total = ink.iter().filter(|&&v| v).count();
    let covered = ink.iter().zip(&owner).filter(|(v, o)| **v && **o != usize::MAX).count();
    let coverage = covered as f32 / total.max(1) as f32;
    log::debug!("pieces (separate {separate}): {} candidates hold {:.1}% of the ink", kept.len(), coverage * 100.0);
    if coverage < 0.98 {
        return None;
    }
    let s = SCALE as usize;
    let mut pieces: Vec<Piece> = kept
        .iter()
        .enumerate()
        .map(|(k, &(_, _, _, _, fill))| {
            let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
            for y in 0..h {
                for x in 0..w {
                    if owner[y * w + x] == k {
                        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                    }
                }
            }
            let (px0, py0) = (x0 * s, y0 * s);
            let px1 = ((x1 + 1) * s).min(img.width() as usize);
            let py1 = ((y1 + 1) * s).min(img.height() as usize);
            Piece {
                region: (px0 as u32, py0 as u32, (px1 - px0) as u32, (py1 - py0) as u32),
                fill,
                cells: owner.iter().map(|&o| o == k).collect(),
                cells_w: w,
            }
        })
        .collect();
    pieces.sort_by_key(|p| std::cmp::Reverse(u64::from(p.region.2) * u64::from(p.region.3)));
    Some(pieces)
}

impl Piece {
    /// This piece cut out of the page, with anything belonging to other pieces whitened.
    pub fn cut(&self, img: &RgbImage) -> RgbImage {
        let (x0, y0, w, h) = self.region;
        let s = SCALE;
        RgbImage::from_fn(w, h, |x, y| {
            let (px, py) = (x0 + x, y0 + y);
            let cell = (py / s) as usize * self.cells_w + (px / s) as usize;
            if self.cells.get(cell).copied().unwrap_or(false) {
                *img.get_pixel(px, py)
            } else {
                image::Rgb([255, 255, 255])
            }
        })
    }
}

/// The mask as a picture, for checking what was taken as paper.
pub fn mask_image(img: &RgbImage) -> GrayImage {
    let (m, w, h, _) = paper_mask(img);
    GrayImage::from_fn(w as u32, h as u32, |x, y| image::Luma([if m[y as usize * w + x as usize] { 0 } else { 255 }]))
}
