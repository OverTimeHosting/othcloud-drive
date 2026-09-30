//! Run the in-memory PROTOCOL.md server for manual / scripted mount tests.
//!
//!   cargo run --example mock_server -- [port]
//!
//! Prints one JSON line with origin, token, org and the seeded drives, then
//! serves until killed. `/__mock/*` endpoints (see tests/common/mock.rs)
//! inspect and change server state.

#[path = "../tests/common/mock.rs"]
mod mock;

fn cover_png(r: u8, g: u8, b: u8) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(128, 128, |x, y| {
        if (x / 16 + y / 16) % 2 == 0 {
            image::Rgba([r, g, b, 255])
        } else {
            image::Rgba([255, 255, 255, 255])
        }
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let server = mock::MockServer::start(port);
    let d1 = server.add_drive("Test Drive", Some(10 * 1024 * 1024 * 1024));
    let d2 = server.add_drive("Photos", None);
    server.put(&d1, "hello.txt", b"Hello from the OTHCloud mock server!\r\n");
    server.put(&d1, "Docs/readme.md", b"# Readme\r\nSeeded file.\r\n");
    server.put(&d1, "Docs/Deep/nested.txt", b"nested");
    let big: Vec<u8> = (0..3_500_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    server.put(&d1, "big.bin", &big);
    server.put(&d2, "cat.jpg", b"not really a jpeg");
    {
        let mut s = server.state.lock().unwrap();
        s.drive_mut(&d1).unwrap().cover = Some(cover_png(0x1e, 0x88, 0xe5));
    }
    println!(
        "{}",
        serde_json::json!({
            "origin": server.origin(),
            "token": "test-token",
            "org": "org_1",
            "drives": [d1, d2],
        })
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
