//! Drive icons for Explorer (the letter's DefaultIcon and the sync root's
//! sidebar icon), and the tray icon.
//!
//! * A photo cover: centre-cropped to fill a rounded tile.
//! * A logo cover (a picture with transparency, e.g. a PNG logo): trimmed to
//!   what is drawn, and fitted inside the tile on the drive's accent colour
//!   (or a light neutral) — never cropped, never on a hard white square.
//! * No cover (or one that can't be decoded): a tile in the drive's accent
//!   colour (fallback: brand navy) with the OTH logo (64 px and up) or a
//!   crisp globe mark (smaller).
//!
//! A cover gets a small OTHCloud badge from 48 px up; below that the badge
//! would hide most of the picture, so the small sizes Explorer uses in its
//! navigation pane and lists show the cover alone. Small sizes are lightly
//! sharpened after downscaling.
//!
//! All shapes are anti-aliased from signed distance fields; only the
//! permissively-licensed `image` and `ico` crates are used.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};
use std::sync::OnceLock;

/// Sizes in the .ico: 16/20/24/32/40/48 are what Explorer picks at
/// 100/125/150/200/250/300% DPI for small and list icons; 64 and 256 for
/// tiles and large icons.
pub const SIZES: [u32; 8] = [256, 64, 48, 40, 32, 24, 20, 16];

/// Bumped whenever the drawing changes, and part of the icon's file name, so
/// a new look reaches Explorer (which caches icons by path) on upgrade.
pub const STYLE: u32 = 2;

/// The size a client asks the panel to scale a cover to (`cover?w=`): enough
/// for the 256 px frame after a square crop of a 4:3 picture.
pub const COVER_FETCH_WIDTH: u32 = 512;

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

/// Relative luminance 0..1 (sRGB weights, no gamma: good enough to pick a
/// contrasting mark).
fn luma(c: [u8; 3]) -> f32 {
    (0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32) / 255.0
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

/// Source-over in straight alpha; `cov` is the extra coverage multiplier.
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
    x1: f32,
    r: f32,
}

impl Tile {
    fn side(&self) -> f32 {
        self.x1 - self.x0
    }
    fn sd(&self, px: f32, py: f32) -> f32 {
        sd_round_rect(px, py, self.x0, self.x0, self.x1, self.x1, self.r)
    }
}

fn tile_geom(s: u32) -> Tile {
    let sf = s as f32;
    // A hair of margin so the edge/badge ring isn't clipped (none at 16-20:
    // every pixel counts there).
    let m = (sf * 0.03).round().max(0.0);
    let x0 = m;
    let x1 = sf - m;
    Tile { x0, x1, r: (x1 - x0) * 0.18 }
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

/// Fill the tile with a soft vertical gradient around `c`.
fn fill_tile(img: &mut RgbaImage, t: &Tile, c: [u8; 3], spread: f32) {
    let (top, bot) = (shade(c, 1.0 + spread), shade(c, 1.0 - spread));
    let (w, h) = img.dimensions();
    for y in 0..h {
        let f = y as f32 / (h.max(2) - 1) as f32;
        let col = [
            (top[0] as f32 + (bot[0] as f32 - top[0] as f32) * f) / 255.0,
            (top[1] as f32 + (bot[1] as f32 - top[1] as f32) * f) / 255.0,
            (top[2] as f32 + (bot[2] as f32 - top[2] as f32) * f) / 255.0,
        ];
        for x in 0..w {
            let cov = sat(0.5 - t.sd(x as f32 + 0.5, y as f32 + 0.5));
            if cov > 0.0 {
                over(img.get_pixel_mut(x, y), [col[0], col[1], col[2], 1.0], cov);
            }
        }
    }
}

/// The subtle darker 1px edge just inside the tile outline.
fn tile_edge(img: &mut RgbaImage, t: &Tile, strength: f32) {
    let (w, h) = img.dimensions();
    for y in 0..h {
        for x in 0..w {
            let d = t.sd(x as f32 + 0.5, y as f32 + 0.5);
            // a 1px band straddling the inside of the outline
            let band = sat(0.5 - d) * sat(1.5 + d);
            if band > 0.0 {
                over(img.get_pixel_mut(x, y), [0.0, 0.0, 0.0, strength], band);
            }
        }
    }
}

/// The OTHCloud badge: a navy disc inside a white ring with a dark hairline,
/// carrying the full logo (large) or the crisp globe mark.
fn draw_badge(img: &mut RgbaImage, cx: f32, cy: f32, r: f32) {
    let ring = (r * 0.14).max(1.0);
    let navy = brand_navy();
    let (w, h) = img.dimensions();
    for y in 0..h {
        for x in 0..w {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = sd_circle(px, py, cx, cy, r);
            // dark hairline just outside the white ring
            let hair = sat(0.5 - (d - 0.6)) * sat(0.5 + d + 0.2);
            if hair > 0.0 {
                over(img.get_pixel_mut(x, y), [0.0, 0.0, 0.0, 0.30], hair);
            }
            let cov = sat(0.5 - d);
            if cov > 0.0 {
                over(img.get_pixel_mut(x, y), [1.0, 1.0, 1.0, 1.0], cov);
            }
            let covin = sat(0.5 - sd_circle(px, py, cx, cy, r - ring));
            if covin > 0.0 {
                over(img.get_pixel_mut(x, y), rgbf(navy, 1.0), covin);
            }
        }
    }
    let inner = r - ring;
    if inner >= 30.0 {
        // The globe fills ~96% of the logo square.
        let side = ((inner * 2.0) / 0.96).round().max(2.0) as u32;
        let lg = image::imageops::resize(logo(), side, side, FilterType::Lanczos3);
        let ox = (cx - side as f32 / 2.0).round() as i32;
        let oy = (cy - side as f32 / 2.0).round() as i32;
        blit_masked(img, &lg, ox, oy, |px, py| sat(0.5 - sd_circle(px, py, cx, cy, inner)));
    } else {
        // The shrunk logo is mush; the globe mark reads as the same brand.
        draw_globe_mark(img, cx, cy, inner * 0.74, [255, 255, 255]);
    }
}

/// A simplified globe: outline, equator, one meridian, in `ink`.
fn draw_globe_mark(img: &mut RgbaImage, cx: f32, cy: f32, r: f32, ink: [u8; 3]) {
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
                over(img.get_pixel_mut(x, y), rgbf(ink, 0.96), cov);
            }
        }
    }
}

// ------------------------------------------------------------- covers

/// A cover ready to draw: square, and classified.
pub struct Cover {
    /// Square. For a logo: trimmed to its drawn pixels and centred on
    /// transparency.
    img: RgbaImage,
    /// Has real transparency: fit it inside the tile instead of filling it.
    pub logo: bool,
}

/// Longest side kept from a cover before drawing (the largest frame is 256).
const COVER_WORK: u32 = 768;

/// Decide how a cover is drawn: a picture with transparency in a noticeable
/// share of its pixels is a logo (trim and fit); anything else is a photo
/// (centre crop to fill).
pub fn prepare_cover(src: &DynamicImage) -> Cover {
    let (w, h) = src.dimensions();
    let src = if w.max(h) > COVER_WORK {
        src.resize(COVER_WORK, COVER_WORK, FilterType::Triangle)
    } else {
        src.clone()
    };
    let rgba = src.to_rgba8();
    let (w, h) = rgba.dimensions();
    let total = (w as u64 * h as u64).max(1);
    let clear = rgba.pixels().filter(|p| p[3] < 250).count() as u64;
    if clear * 50 > total {
        // Trim to what is drawn (alpha > 24).
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
        for (x, y, p) in rgba.enumerate_pixels() {
            if p[3] > 24 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
        if x1 > x0 && y1 > y0 {
            let (cw, ch) = (x1 - x0, y1 - y0);
            let side = cw.max(ch);
            let mut sq = RgbaImage::new(side, side);
            let part = image::imageops::crop_imm(&rgba, x0, y0, cw, ch).to_image();
            image::imageops::replace(&mut sq, &part, ((side - cw) / 2) as i64, ((side - ch) / 2) as i64);
            return Cover { img: sq, logo: true };
        }
        // Nothing drawn at all: show it as a (blank) photo.
    }
    let side = w.min(h).max(1);
    let sq = image::imageops::crop_imm(&rgba, (w - side) / 2, (h - side) / 2, side, side).to_image();
    Cover { img: sq, logo: false }
}

/// Downscale to `px` and, at small sizes, sharpen a little: Lanczos alone
/// leaves a 16-32 px cover soft.
fn scaled(src: &RgbaImage, px: u32, s: u32) -> RgbaImage {
    let px = px.max(1);
    let r = image::imageops::resize(src, px, px, FilterType::Lanczos3);
    match s {
        0..=24 => image::imageops::unsharpen(&r, 0.6, 1),
        25..=48 => image::imageops::unsharpen(&r, 0.5, 2),
        _ => r,
    }
}

/// Render one drive icon at `s` px.
pub fn render(s: u32, cover: Option<&Cover>, color: Option<[u8; 3]>) -> RgbaImage {
    let mut img = RgbaImage::new(s, s);
    let t = tile_geom(s);
    let tile_px = t.side().round() as u32;
    let tile_mask = |px: f32, py: f32| sat(0.5 - t.sd(px, py));
    match cover {
        Some(c) if !c.logo => {
            let sq = scaled(&c.img, tile_px, s);
            // Opaque base under any stray transparency.
            let mut base = RgbaImage::from_pixel(tile_px, tile_px, Rgba([255, 255, 255, 255]));
            for (x, y, p) in sq.enumerate_pixels() {
                over(base.get_pixel_mut(x, y), rgbf([p[0], p[1], p[2]], p[3] as f32 / 255.0), 1.0);
            }
            blit_masked(&mut img, &base, t.x0 as i32, t.x0 as i32, tile_mask);
            tile_edge(&mut img, &t, if s <= 24 { 0.26 } else { 0.20 });
        }
        Some(c) => {
            // Logo: on the drive's colour, or a light neutral that reads on
            // both the light and the dark Explorer theme.
            let bg = color.unwrap_or([0xee, 0xf1, 0xf6]);
            fill_tile(&mut img, &t, bg, 0.06);
            tile_edge(&mut img, &t, 0.16);
            // Inset so the logo doesn't touch the rounded corners.
            let inset = if s <= 20 { 0.08 } else { 0.12 };
            let side = (t.side() * (1.0 - 2.0 * inset)).round().max(1.0) as u32;
            let lg = scaled(&c.img, side, s);
            let o = ((t.x0 + t.x1) / 2.0 - side as f32 / 2.0).round() as i32;
            blit_masked(&mut img, &lg, o, o, tile_mask);
        }
        None => {
            let c = color.unwrap_or_else(brand_navy);
            fill_tile(&mut img, &t, c, 0.18);
            tile_edge(&mut img, &t, 0.18);
            let cx = (t.x0 + t.x1) / 2.0;
            if s >= 64 {
                let side = (t.side() * 0.74).round() as u32;
                let lg = image::imageops::resize(logo(), side, side, FilterType::Lanczos3);
                let o = (cx - side as f32 / 2.0).round() as i32;
                blit_masked(&mut img, &lg, o, o, |_, _| 1.0);
            } else {
                let ink = if luma(c) > 0.62 { brand_navy() } else { [255, 255, 255] };
                draw_globe_mark(&mut img, cx, cx, t.side() * 0.34, ink);
            }
        }
    }
    if cover.is_some() && s >= 48 {
        let r = s as f32 * 0.34 / 2.0;
        let c = (s as f32 - r - 0.5).min(t.x1 - r + s as f32 * 0.015);
        draw_badge(&mut img, c, c, r);
    }
    img
}

/// A multi-size .ico (PNG entry at 256, 32-bit BMP below) from a decoded
/// cover, or the colour fallback when `cover` is None.
pub fn drive_ico_from(cover: Option<&DynamicImage>, color: Option<&str>) -> Result<Vec<u8>, String> {
    let cover = cover.map(prepare_cover);
    let color = color.and_then(parse_color);
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for s in SIZES {
        let img = render(s, cover.as_ref(), color);
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

/// Decode cover bytes (any format the `image` crate reads: PNG, JPEG, WebP,
/// GIF, BMP, ICO). HEIC/AVIF/TIFF are not among them — ask the panel for a
/// scaled copy instead (`cover?w=`, `thumbnail`).
pub fn decode_cover(bytes: &[u8]) -> Result<DynamicImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("cover image: {e}"))?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err("empty cover image".into());
    }
    Ok(img)
}

/// A multi-size .ico for a drive from cover bytes (or None: the colour tile).
pub fn drive_ico(cover: Option<&[u8]>, color: Option<&str>) -> Result<Vec<u8>, String> {
    let cover = cover.map(decode_cover).transpose()?;
    drive_ico_from(cover.as_ref(), color)
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

    fn frames(bytes: &[u8]) -> Vec<(u32, bool, RgbaImage)> {
        let dir = ico::IconDir::read(std::io::Cursor::new(bytes)).unwrap();
        dir.entries()
            .iter()
            .map(|e| {
                let i = e.decode().unwrap();
                assert_eq!(e.width(), e.height());
                (e.width(), e.is_png(), RgbaImage::from_raw(i.width(), i.height(), i.rgba_data().to_vec()).unwrap())
            })
            .collect()
    }

    /// A transparent logo with a wide empty margin (like a 500x500 PNG logo).
    fn logo_cover() -> Vec<u8> {
        png(RgbaImage::from_fn(500, 500, |x, y| {
            let d = ((x as f32 - 250.0).powi(2) + (y as f32 - 250.0).powi(2)).sqrt();
            if d < 150.0 { Rgba([200, 30, 30, 255]) } else { Rgba([0, 0, 0, 0]) }
        }))
    }

    #[test]
    fn ico_has_every_size_for_cover_logo_and_colour() {
        let photo = png(RgbaImage::from_fn(300, 200, |x, _| Rgba([x as u8, 0, 200, 255])));
        let logo = logo_cover();
        for (c, col) in [
            (Some(photo.as_slice()), None),
            (Some(logo.as_slice()), Some("#38bdf8")),
            (None, Some("#38bdf8")),
            (None, None),
        ] {
            let f = frames(&drive_ico(c, col).unwrap());
            let mut sizes: Vec<u32> = f.iter().map(|x| x.0).collect();
            sizes.sort();
            assert_eq!(sizes, vec![16, 20, 24, 32, 40, 48, 64, 256]);
            for (s, is_png, img) in &f {
                assert_eq!(*is_png, *s == 256, "only 256 is PNG ({s})");
                assert_eq!(img.dimensions(), (*s, *s));
                // Every frame is drawn: an opaque centre, transparent corner.
                assert_eq!(img.get_pixel(s / 2, s / 2)[3], 255, "{s}: centre opaque");
                if *s >= 24 {
                    assert_eq!(img.get_pixel(0, 0)[3], 0, "{s}: rounded corner");
                }
            }
        }
        assert!(drive_ico(Some(b"not an image"), None).is_err());
    }

    #[test]
    fn photo_fills_the_tile_and_badge_only_from_48() {
        let cover = prepare_cover(&DynamicImage::ImageRgba8(RgbaImage::from_pixel(100, 60, Rgba([255, 0, 0, 255]))));
        assert!(!cover.logo);
        let red = |p: &Rgba<u8>| p[0] > 200 && p[1] < 60 && p[2] < 60;
        let big = render(256, Some(&cover), None);
        assert!(red(big.get_pixel(80, 80)), "cover shows: {:?}", big.get_pixel(80, 80));
        assert!(!red(big.get_pixel(214, 214)), "badge present: {:?}", big.get_pixel(214, 214));
        let b48 = render(48, Some(&cover), None);
        assert!(!red(b48.get_pixel(40, 40)), "badge at 48");
        // 16-32: the cover alone, right to the bottom-right corner.
        for s in [16u32, 20, 24, 32] {
            let img = render(s, Some(&cover), None);
            let p = img.get_pixel(s - s / 4, s - s / 4);
            assert!(red(p), "{s}: no badge over the cover: {p:?}");
        }
    }

    #[test]
    fn logo_is_trimmed_and_fitted_on_the_drive_colour() {
        let img = decode_cover(&logo_cover()).unwrap();
        let cover = prepare_cover(&img);
        assert!(cover.logo, "transparent picture is a logo");
        // Trimmed: the 300 px disc, not the 500 px canvas.
        assert!((299..=301).contains(&cover.img.width()), "{}", cover.img.width());
        let out = render(64, Some(&cover), parse_color("#38bdf8"));
        // Centre is the logo, the inset around it the colour, not white.
        let c = out.get_pixel(32, 32);
        assert!(c[0] > 150 && c[1] < 80, "logo at centre: {c:?}");
        let edge = out.get_pixel(32, 5);
        assert!(edge[2] > 200 && edge[0] < 120, "drive colour around the logo: {edge:?}");
        // Without a colour: a light neutral, still not pure white.
        let n = render(64, Some(&cover), None);
        let e = n.get_pixel(32, 5);
        assert!(e[0] > 200 && e[0] < 255 && e[3] == 255, "light neutral: {e:?}");
    }

    #[test]
    fn fallback_uses_colour_with_a_contrasting_mark() {
        let dark = render(32, None, parse_color("#1e293b"));
        let light = render(32, None, parse_color("#facc15"));
        let navy = render(32, None, None);
        let n = brand_navy();
        let p = navy.get_pixel(16, 3);
        assert!((p[2] as i32 - n[2] as i32).abs() < 60, "navy tile: {p:?}");
        // The globe mark's outline crosses (16, 16 - 0.34*side + 1).
        let count = |img: &RgbaImage, f: &dyn Fn(&Rgba<u8>) -> bool| img.pixels().filter(|p| f(p)).count();
        assert!(count(&dark, &|p| p[0] > 230 && p[1] > 230 && p[2] > 230) > 10, "white mark on dark");
        assert!(count(&light, &|p| p[0] < 90 && p[1] < 90 && p[3] > 200) > 10, "navy mark on yellow");
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
