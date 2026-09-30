//! Render sample drive icons (every size) and a contact sheet, for review.
//!
//!   cargo run --example render_icons -- <out dir>

use image::{imageops, DynamicImage, Rgba, RgbaImage};
use othcloud_drive::icons::{render, SmallBadge, SIZES};

fn photo() -> DynamicImage {
    // Photo-like: sky gradient, sun, two hills, some noise.
    let (w, h) = (640u32, 480u32);
    let mut seed = 12345u32;
    DynamicImage::ImageRgba8(RgbaImage::from_fn(w, h, |x, y| {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let n = ((seed >> 24) as i32 - 128) / 16;
        let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
        let hill1 = 0.62 + 0.08 * (fx * 6.0).sin();
        let hill2 = 0.72 + 0.05 * (fx * 9.0 + 1.0).cos();
        let sun = ((fx - 0.72).powi(2) + (fy - 0.28).powi(2)).sqrt() < 0.09;
        let c: [i32; 3] = if sun {
            [255, 214, 90]
        } else if fy > hill2 {
            [40, 110, 50]
        } else if fy > hill1 {
            [70, 140, 70]
        } else {
            [(90.0 + 90.0 * fy) as i32, (150.0 + 60.0 * fy) as i32, 235]
        };
        Rgba([
            (c[0] + n).clamp(0, 255) as u8,
            (c[1] + n).clamp(0, 255) as u8,
            (c[2] + n).clamp(0, 255) as u8,
            255,
        ])
    }))
}

fn white() -> DynamicImage {
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(400, 400, Rgba([255, 255, 255, 255])))
}

fn wide() -> DynamicImage {
    DynamicImage::ImageRgba8(RgbaImage::from_fn(1600, 200, |x, _| {
        let band = (x / 100) % 3;
        match band {
            0 => Rgba([244, 63, 94, 255]),
            1 => Rgba([56, 189, 248, 255]),
            _ => Rgba([250, 204, 21, 255]),
        }
    }))
}

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("out dir"));
    std::fs::create_dir_all(&out).unwrap();
    let covers = [("cover-photo", photo()), ("cover-white", white()), ("cover-wide", wide())];
    let colors: [(&str, Option<[u8; 3]>); 3] = [
        ("nocover-null", None),
        ("nocover-38bdf8", othcloud_drive::icons::parse_color("#38bdf8")),
        ("nocover-f43f5e", othcloud_drive::icons::parse_color("#f43f5e")),
    ];
    // (name, cover, color, small badge)
    let mut rows: Vec<(String, Vec<RgbaImage>)> = Vec::new();
    for (name, c) in &covers {
        for sb in [SmallBadge::Dot, SmallBadge::None] {
            let label = format!("{name}{}", if sb == SmallBadge::None { "-16nobadge" } else { "" });
            let imgs: Vec<RgbaImage> = SIZES.iter().map(|s| render(*s, Some(c), None, sb)).collect();
            if sb == SmallBadge::Dot {
                for (s, img) in SIZES.iter().zip(&imgs) {
                    img.save(out.join(format!("{name}-{s}.png"))).unwrap();
                }
            }
            rows.push((label, imgs));
        }
    }
    for (name, col) in &colors {
        let imgs: Vec<RgbaImage> = SIZES.iter().map(|s| render(*s, None, *col, SmallBadge::Dot)).collect();
        for (s, img) in SIZES.iter().zip(&imgs) {
            img.save(out.join(format!("{name}-{s}.png"))).unwrap();
        }
        rows.push((name.to_string(), imgs));
    }
    // Contact sheet: per row, 1x on white | 1x on dark | 4x zoom (small sizes).
    let pad = 8u32;
    let one_w: u32 = SIZES.iter().map(|s| s + pad).sum();
    let zoom_sizes = [64u32, 48, 32, 24, 16];
    let zoom_w: u32 = zoom_sizes.iter().map(|s| s * 4 + pad).sum();
    let row_h = 256 + pad;
    let w = one_w * 2 + zoom_w + pad * 4;
    let h = row_h * rows.len() as u32 + pad;
    let mut sheet = RgbaImage::from_pixel(w, h, Rgba([245, 245, 245, 255]));
    for (ri, (_, imgs)) in rows.iter().enumerate() {
        let y0 = pad + ri as u32 * row_h;
        let mut x = pad;
        for bg in [Rgba([255, 255, 255, 255]), Rgba([32, 32, 32, 255])] {
            let panel = RgbaImage::from_pixel(one_w, 256, bg);
            imageops::overlay(&mut sheet, &panel, x as i64, y0 as i64);
            let mut xx = x;
            for img in imgs {
                imageops::overlay(&mut sheet, img, xx as i64, (y0 + 256 - img.height()) as i64);
                xx += img.width() + pad;
            }
            x += one_w + pad;
        }
        let mut xx = x;
        for s in zoom_sizes {
            let img = imgs.iter().find(|i| i.width() == s).unwrap();
            let z = imageops::resize(img, s * 4, s * 4, imageops::FilterType::Nearest);
            let panel = RgbaImage::from_pixel(s * 4, s * 4, Rgba([255, 255, 255, 255]));
            imageops::overlay(&mut sheet, &panel, xx as i64, (y0 + 256 - s * 4) as i64);
            imageops::overlay(&mut sheet, &z, xx as i64, (y0 + 256 - s * 4) as i64);
            xx += s * 4 + pad;
        }
    }
    sheet.save(out.join("contact-sheet.png")).unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    println!("rows: {names:?}");
    println!("wrote {}", out.join("contact-sheet.png").display());
}
