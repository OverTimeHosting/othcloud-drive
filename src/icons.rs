//! Drive icons for Explorer (the letter's DefaultIcon and the sync root's
//! sidebar icon), and the tray icon.
//!
//! * With a cover: the cover, centre-cropped into a rounded tile with a
//!   subtle edge, plus an OTHCloud badge bottom-right.
//! * Without: a rounded tile in the drive's accent colour (fallback: brand
//!   navy) with a soft vertical gradient and the OTH logo (large sizes) or a
//!   simplified white globe mark (small sizes).
//!
//! All shapes are anti-aliased from signed distance fields; only the
//! permissively-licensed `image` and `ico` crates are used.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};
use std::sync::OnceLock;

/// Sizes in the .ico (24 is what Explorer uses at 125–150% DPI).
pub const SIZES: [u32; 6] = [256, 64, 48, 32, 24, 16];

const LOGO_PNG: &[u8] = include_bytes!("../assets/oth-logo.png");

fn logo() -> &'static RgbaImage {
    static L: OnceLock<RgbaImage> = OnceLock::new();
    L.get_or_init(|| {
        image::load_from_memory(LOGO_PNG)
            .expect("bundled logo")
            .to_rgba8()
    })
}

/// The logo's navy, sampled from its dark opaque pixels.
pub fn brand_navy() -> [u8; 3] {
    static N: OnceLock<[u8; 3]> = OnceLock::new();
    *N.get_or_init(|| {
        let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
        for p in logo().pixels() {
            let [pr, pg, pb, pa] = p.0;
            let lum = (pr as u32 * 3 + pg as u32 * 6 + pb as u32) / 10;
            if pa > 240 && lum < 70 && pb > pr {
                r += pr as u64;
                g += pg as u64;
                b += pb as u64;
                n += 1;
            }
        }
        match (r.checked_div(n), g.checked_div(n), b.checked_div(n)) {
            (Some(r), Some(g), Some(b)) => [r as u8, g as u8, b as u8],
            _ => [0x0f, 0x1b, 0x3d],
        }
    })
}

/// "#38bdf8" / "38bdf8" / "#3bf" -> rgb.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    let h: String = if h.len() == 3 {
        h.chars().flat_map(|c| [c, c]).collect()
    } else {
        h.to_string()
    };
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(&h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

// ------------------------------------------------------------- drawing

fn sat(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// Signed distance to a rounded rectangle (negative inside).
fn sd_round_rect(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let cx = (x0 + x1) / 2.0;
    let cy = (y0 + y1) / 2.0;
    let hx = (x1 - x0) / 2.0 - r;
    let hy = (y1 - y0) / 2.0 - r;
    let qx = (px - cx).abs() - hx;
    let qy = (py - cy).abs() - hy;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn sd_circle(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r
}

/// Source-over in straight alpha; `a` is the extra coverage multiplier.
fn over(dst: &mut Rgba<u8>, src: [f32; 4], cov: f32) {
    let sa = sat(src[3] * cov);
    if sa <= 0.0 {
        return;
    }
    let d = dst.0;
    let da = d[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    let mut out = [0u8; 4];
    for i in 0..3 {
        let c = (src[i] * sa + (d[i] as f32 / 255.0) * da * (1.0 - sa)) / oa.max(1e-6);
        out[i] = (sat(c) * 255.0 + 0.5) as u8;
    }
    out[3] = (oa * 255.0 + 0.5) as u8;
    dst.0 = out;
}

fn rgbf(c: [u8; 3], a: f32) -> [f32; 4] {
    [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, a]
}

fn shade(c: [u8; 3], f: f32) -> [u8; 3] {
    let m = |x: u8| -> u8 {
        let x = x as f32;
        (if f >= 1.0 { x + (255.0 - x) * (f - 1.0) } else { x * f }).clamp(0.0, 255.0) as u8
    };
    [m(c[0]), m(c[1]), m(c[2])]
}

struct Tile {
    x0: f32,
    y0: f32,
    x1: f32,
    r: f32,
}

fn tile_geom(s: u32) -> Tile {
    let sf = s as f32;
    // A hair of margin so the edge/badge ring isn't clipped.
    let m = (sf * 0.03).round().max(0.0);
    let x0 = m;
    let x1 = sf - m;
    Tile { x0, y0: x0, x1, r: (x1 - x0) * 0.18 }
}

fn square_crop(img: &DynamicImage) -> DynamicImage {
    let (w, h) = img.dimensions();
    let side = w.min(h).max(1);
    img.crop_imm((w - side) / 2, (h - side) / 2, side, side)
}

/// Draw `src` (already the right pixel size) into `dst` at (ox, oy),
/// multiplied by `mask(x, y)` coverage.
fn blit_masked(dst: &mut RgbaImage, src: &RgbaImage, ox: i32, oy: i32, mask: impl Fn(f32, f32) -> f32) {
    for (x, y, p) in src.enumerate_pixels() {
        let dx = ox + x as i32;
        let dy = oy + y as i32;
        if dx < 0 || dy < 0 || dx >= dst.width() as i32 || dy >= dst.height() as i32 {
            continue;
        }
        let cov = mask(dx as f32 + 0.5, dy as f32 + 0.5);
        if cov <= 0.0 {
            continue;
        }
        let c = rgbf([p[0], p[1], p[2]], p[3] as f32 / 255.0);
        over(dst.get_pixel_mut(dx as u32, dy as u32), c, cov);
    }
}

/// The subtle darker 1px edge just inside the tile outline.
fn tile_edge(img: &mut RgbaImage, t: &Tile, strength: f32) {
    let (w, h) = img.dimensions();
    for y in 0..h {
        for x in 0..w {
            let d = sd_round_rect(x as f32 + 0.5, y as f32 + 0.5, t.x0, t.y0, t.x1, t.x1, t.r);
            // a 1px band straddling the inside of the outline
            let band = sat(0.5 - d) * sat(1.5 + d);
            if band > 0.0 {
                over(img.get_pixel_mut(x, y), [0.0, 0.0, 0.0, strength], band);
            }
        }
    }
}

/// The OTH logo on a navy disc inside a white ring, with a dark hairline.
fn draw_badge(img: &mut RgbaImage, cx: f32, cy: f32, r: f32, detailed: bool) {
    let ring = (r * 0.16).max(1.0);
    let navy = brand_navy();
    let (w, h) = img.dimensions();
    for y in 0..h {
        for x in 0..w {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = sd_circle(px, py, cx, cy, r);
            // dark hairline just outside the white ring
            let hair = sat(0.5 - (d - 0.6)) * sat(0.5 + d + 0.2);
            if hair > 0.0 {
                over(img.get_pixel_mut(x, y), [0.0, 0.0, 0.0, 0.35], hair);
            }
            let cov = sat(0.5 - d);
            if cov > 0.0 {
                over(img.get_pixel_mut(x, y), [1.0, 1.0, 1.0, 1.0], cov);
            }
            let din = sd_circle(px, py, cx, cy, r - ring);
            let covin = sat(0.5 - din);
            if covin > 0.0 {
                over(img.get_pixel_mut(x, y), rgbf(navy, 1.0), covin);
            }
        }
    }
    if !detailed && r >= 4.5 {
        // Small sizes: the shrunk logo is mush; a crisp white globe mark
        // on the navy disc reads as the same brand.
        draw_globe_mark(img, cx, cy, (r - ring) * 0.72);
    }
    if detailed {
        let inner = r - ring;
        // The globe fills ~96% of the logo square.
        let side = ((inner * 2.0) / 0.96).round().max(2.0) as u32;
        let lg = image::imageops::resize(logo(), side, side, FilterType::Lanczos3);
        let ox = (cx - side as f32 / 2.0).round() as i32;
        let oy = (cy - side as f32 / 2.0).round() as i32;
        blit_masked(img, &lg, ox, oy, |px, py| sat(0.5 - sd_circle(px, py, cx, cy, inner)));
    }
}

/// White simplified globe: outline, equator, one meridian.
fn draw_globe_mark(img: &mut RgbaImage, cx: f32, cy: f32, r: f32) {
    let sw = (r * 0.2).max(1.1);
    let (w, h) = img.dimensions();
    for y in 0..h {
        for x in 0..w {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let ring = (sd_circle(px, py, cx, cy, r - sw / 2.0)).abs() - sw / 2.0;
            let inside = sd_circle(px, py, cx, cy, r - sw * 0.9) < 0.0;
            let eq = if inside { (py - cy).abs() - sw * 0.42 } else { 9.0 };
            // meridian: ellipse rx = 0.45 r
            let (ex, ey) = ((px - cx) / (r * 0.45), (py - cy) / r);
            let el = ((ex * ex + ey * ey).sqrt() - 1.0).abs() * (r * 0.45) - sw * 0.42;
            let mer = if inside { el } else { 9.0 };
            let d = ring.min(eq).min(mer);
            let cov = sat(0.5 - d);
            if cov > 0.0 {
                over(img.get_pixel_mut(x, y), [1.0, 1.0, 1.0, 0.96], cov);
            }
        }
    }
}

/// Which small-size badge to use on covers at 16 px.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SmallBadge {
    None,
    Dot,
}

/// Render one drive icon at `s` px.
pub fn render(s: u32, cover: Option<&DynamicImage>, color: Option<[u8; 3]>, small: SmallBadge) -> RgbaImage {
    let mut img = RgbaImage::new(s, s);
    let t = tile_geom(s);
    let tile_px = (t.x1 - t.x0).round() as u32;
    let tile_mask = |px: f32, py: f32| sat(0.5 - sd_round_rect(px, py, t.x0, t.y0, t.x1, t.x1, t.r));
    match cover {
        Some(c) => {
            let sq = square_crop(c).resize_exact(tile_px, tile_px, FilterType::Lanczos3).to_rgba8();
            // Opaque base under transparent covers.
            let mut base = RgbaImage::from_pixel(tile_px, tile_px, Rgba([255, 255, 255, 255]));
            for (x, y, p) in sq.enumerate_pixels() {
                over(base.get_pixel_mut(x, y), rgbf([p[0], p[1], p[2]], p[3] as f32 / 255.0), 1.0);
            }
            blit_masked(&mut img, &base, t.x0 as i32, t.y0 as i32, tile_mask);
            tile_edge(&mut img, &t, if s <= 24 { 0.28 } else { 0.22 });
            if s >= 24 {
                let frac = if s >= 48 { 0.40 } else { 0.46 };
                let r = s as f32 * frac / 2.0;
                let (cx, cy) = (t.x1 - r + s as f32 * 0.01, t.x1 - r + s as f32 * 0.01);
                draw_badge(&mut img, cx, cy, r, s >= 48);
            } else if small == SmallBadge::Dot {
                let r = 3.6;
                draw_badge(&mut img, t.x1 - r, t.x1 - r, r, false);
            }
        }
        None => {
            let c = color.unwrap_or_else(brand_navy);
            let (top, bot) = (shade(c, 1.18), shade(c, 0.82));
            let (w, h) = img.dimensions();
            for y in 0..h {
                let f = y as f32 / (h.max(2) - 1) as f32;
                let col = [
                    top[0] as f32 + (bot[0] as f32 - top[0] as f32) * f,
                    top[1] as f32 + (bot[1] as f32 - top[1] as f32) * f,
                    top[2] as f32 + (bot[2] as f32 - top[2] as f32) * f,
                ];
                for x in 0..w {
                    let cov = tile_mask(x as f32 + 0.5, y as f32 + 0.5);
                    if cov > 0.0 {
                        over(img.get_pixel_mut(x, y), [col[0] / 255.0, col[1] / 255.0, col[2] / 255.0, 1.0], cov);
                    }
                }
            }
            tile_edge(&mut img, &t, 0.18);
            let cx = (t.x0 + t.x1) / 2.0;
            if s >= 48 {
                let side = ((t.x1 - t.x0) * 0.74).round() as u32;
                let lg = image::imageops::resize(logo(), side, side, FilterType::Lanczos3);
                let o = (cx - side as f32 / 2.0).round() as i32;
                blit_masked(&mut img, &lg, o, o, |_, _| 1.0);
            } else {
                draw_globe_mark(&mut img, cx, cx, (t.x1 - t.x0) * 0.34);
            }
        }
    }
    img
}

/// A multi-size .ico for a drive (PNG entry at 256, BMP below).
pub fn drive_ico(cover: Option<&[u8]>, color: Option<&str>) -> Result<Vec<u8>, String> {
    let cover = match cover {
        Some(b) => Some(image::load_from_memory(b).map_err(|e| format!("cover image: {e}"))?),
        None => None,
    };
    if let Some(c) = &cover {
        let (w, h) = c.dimensions();
        if w == 0 || h == 0 {
            return Err("empty cover image".into());
        }
    }
    let color = color.and_then(parse_color);
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for s in SIZES {
        let img = render(s, cover.as_ref(), color, SmallBadge::Dot);
        let icon = ico::IconImage::from_rgba_data(s, s, img.into_raw());
        let entry = if s >= 256 {
            ico::IconDirEntry::encode_as_png(&icon)
        } else {
            ico::IconDirEntry::encode_as_bmp(&icon)
        }
        .map_err(|e| e.to_string())?;
        dir.add_entry(entry);
    }
    let mut out = Vec::new();
    dir.write(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Back-compat helper: cover only.
pub fn cover_to_ico(bytes: &[u8]) -> Result<Vec<u8>, String> {
    drive_ico(Some(bytes), None)
}

/// The tray icon: a white cloud on the OTHCloud blue, 32x32 RGBA.
/// `state`: 0 normal, 1 paused (grey), 2 problem (orange).
pub fn tray_rgba(state: u8) -> (Vec<u8>, u32, u32) {
    let n = 32u32;
    let bg: [u8; 3] = match state {
        1 => [0x80, 0x86, 0x8f],
        2 => [0xe6, 0x7e, 0x22],
        _ => [0x1e, 0x88, 0xe5],
    };
    let mut px = vec![0u8; (n * n * 4) as usize];
    let inside_round_rect = |x: f32, y: f32| {
        let r = 7.0;
        let (cx, cy) = (x.clamp(r, 31.0 - r), y.clamp(r, 31.0 - r));
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    };
    let in_circle = |x: f32, y: f32, cx: f32, cy: f32, r: f32| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let i = ((y * n + x) * 4) as usize;
            if !inside_round_rect(fx, fy) {
                continue;
            }
            let cloud = in_circle(fx, fy, 12.0, 17.0, 5.5)
                || in_circle(fx, fy, 18.0, 14.0, 6.5)
                || in_circle(fx, fy, 23.0, 18.0, 4.5)
                || ((17.0..=22.5).contains(&fy) && (12.0..=23.0).contains(&fx));
            let c = if cloud { [255, 255, 255] } else { bg };
            px[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    (px, n, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(img: RgbaImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn ico_has_all_sizes_for_cover_and_color() {
        let cover = png(RgbaImage::from_fn(300, 200, |x, _| Rgba([x as u8, 0, 200, 255])));
        for (c, col) in [(Some(cover.as_slice()), None), (None, Some("#38bdf8")), (None, None)] {
            let bytes = drive_ico(c, col).unwrap();
            let dir = ico::IconDir::read(std::io::Cursor::new(&bytes)).unwrap();
            let mut sizes: Vec<u32> = dir.entries().iter().map(|e| e.width()).collect();
            sizes.sort();
            assert_eq!(sizes, vec![16, 24, 32, 48, 64, 256]);
            let big = dir.entries().iter().find(|e| e.width() == 256).unwrap();
            assert!(big.is_png(), "256 entry is PNG");
            assert!(!dir.entries().iter().find(|e| e.width() == 16).unwrap().is_png());
        }
        assert!(drive_ico(Some(b"not an image"), None).is_err());
    }

    #[test]
    fn tile_is_rounded_and_badged() {
        let cover = DynamicImage::ImageRgba8(RgbaImage::from_pixel(100, 100, Rgba([255, 0, 0, 255])));
        let img = render(256, Some(&cover), None, SmallBadge::Dot);
        assert_eq!(img.get_pixel(0, 0)[3], 0, "transparent corner");
        let mid = img.get_pixel(80, 80);
        assert!(mid[0] > 200 && mid[1] < 60, "cover shows: {mid:?}");
        // badge region bottom-right is not red
        let b = img.get_pixel(200, 200);
        assert!(!(b[0] > 200 && b[1] < 60), "badge present: {b:?}");
    }

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#38bdf8"), Some([0x38, 0xbd, 0xf8]));
        assert_eq!(parse_color("3bf"), Some([0x33, 0xbb, 0xff]));
        assert_eq!(parse_color("nope"), None);
        let n = brand_navy();
        assert!(n[2] > n[0], "navy is bluish: {n:?}");
    }

    #[test]
    fn tray_icon_bitmap() {
        let (px, w, h) = tray_rgba(0);
        assert_eq!(px.len() as u32, w * h * 4);
        let c = ((16 * w + 18) * 4) as usize;
        assert_eq!(&px[c..c + 3], &[255, 255, 255]);
        assert_eq!(px[3], 0);
    }
}
