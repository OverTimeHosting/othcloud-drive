//! Drive cover -> .ico for Explorer, and the tray icon bitmap.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};

const SIZES: [u32; 5] = [256, 64, 48, 32, 16];

/// Convert cover image bytes (any format `image` reads) into a multi-size
/// .ico. The cover is centre-cropped to a square first.
pub fn cover_to_ico(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("cover image: {e}"))?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err("empty cover image".into());
    }
    let side = w.min(h);
    let square = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for s in SIZES {
        let scaled: DynamicImage = square.resize_exact(s, s, FilterType::Lanczos3);
        let rgba = scaled.to_rgba8();
        let icon = ico::IconImage::from_rgba_data(s, s, rgba.into_raw());
        // PNG-compressed entries keep the 256px image small.
        dir.add_entry(ico::IconDirEntry::encode(&icon).map_err(|e| e.to_string())?);
    }
    let mut out = Vec::new();
    dir.write(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
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

    #[test]
    fn cover_converts_to_multi_size_ico() {
        let img = image::RgbaImage::from_fn(300, 200, |x, _| image::Rgba([x as u8, 0, 200, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let ico_bytes = cover_to_ico(&png.into_inner()).unwrap();
        let dir = ico::IconDir::read(std::io::Cursor::new(&ico_bytes)).unwrap();
        let mut sizes: Vec<u32> = dir.entries().iter().map(|e| e.width()).collect();
        sizes.sort();
        assert_eq!(sizes, vec![16, 32, 48, 64, 256]);
        assert!(cover_to_ico(b"not an image").is_err());
    }

    #[test]
    fn tray_icon_bitmap() {
        let (px, w, h) = tray_rgba(0);
        assert_eq!(px.len() as u32, w * h * 4);
        // centre is white cloud, corner transparent
        let c = ((16 * w + 18) * 4) as usize;
        assert_eq!(&px[c..c + 3], &[255, 255, 255]);
        assert_eq!(px[3], 0);
    }
}
