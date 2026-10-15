//! Server-side image processing: normalize uploads into a canonical display
//! image, and produce printer-native rasters the spooler consumes unchanged.
//!
//! One renderer owns the truth. The customer's edit preview stays in the SPA
//! (fast markdown feedback while typing); what the print preview and the
//! spooler both read is *this* module's output — the same bytes.
//!
//! Rendering is deterministic and non-destructive: originals are never
//! touched, and every function is `(size, source) -> bytes` so it can be unit
//! tested without the server.
//!
//! The printer raster is the printer's internal bitmap at the printer DPI, NOT
//! card-mm × dpi (the deliberate overscan is wanted). The default is the Evolis
//! Primacy in 600-dpi mode: 2032×1296, twice the 300-dpi 1016×648 front the
//! SDK's example raster uses. The size is server config
//! (`PRINT_RASTER_WIDTH`/`HEIGHT`), so a printer-specific correction is a
//! config change, not a rebuild.

use image::{DynamicImage, Rgb, RgbImage};

use crate::models::BackDepth;

/// The display image the frontend shows: aspect-correct, small. The CR80 panel
/// at 10 px/mm.
pub const DISPLAY_WIDTH: u32 = 856;
pub const DISPLAY_HEIGHT: u32 = 540;

/// Printer raster size, from config or the 600-dpi Primacy default.
pub struct RasterSize {
    pub width: u32,
    pub height: u32,
}

impl RasterSize {
    pub fn from_env() -> Self {
        let w = std::env::var("PRINT_RASTER_WIDTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2032);
        let h = std::env::var("PRINT_RASTER_HEIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1296);
        Self { width: w, height: h }
    }
}

/// A render is either done (bytes) or failed with a reason. Rendering is
/// synchronous, so the DB status is `ready` or `failed` (never left `pending`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderResult {
    Ready(Vec<u8>),
    Failed(String),
}


fn decode(bytes: &[u8]) -> Result<DynamicImage, String> {
    image::load_from_memory(bytes).map_err(|e| format!("unreadable image: {e}"))
}

/// Scale an image to *fit inside* `w`×`h`, preserving aspect, on a white
/// background (the card is a white panel). Used for the display image: the
/// whole card art is visible, letterboxed if the aspect differs.
fn fit_to(img: &DynamicImage, w: u32, h: u32) -> RgbImage {
    let (iw, ih) = (img.width(), img.height());
    let scale = (w as f32 / iw as f32).min(h as f32 / ih as f32);
    let nw = ((iw as f32 * scale).round() as u32).max(1);
    let nh = ((ih as f32 * scale).round() as u32).max(1);
    let scaled = img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3).to_rgb8();
    let mut canvas = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
    let ox = (w - nw) / 2;
    let oy = (h - nh) / 2;
    image::imageops::overlay(&mut canvas, &scaled, ox as i64, oy as i64);
    canvas
}

/// Scale an image to *cover* `w`×`h`, preserving aspect and centre-cropping
/// the overflow. Used for the print raster: the material fills the printer's
/// native bitmap, bleeding the excess off the panel edge.
fn cover_to(img: &DynamicImage, w: u32, h: u32) -> RgbImage {
    let (iw, ih) = (img.width(), img.height());
    let scale = (w as f32 / iw as f32).max(h as f32 / ih as f32);
    let nw = ((iw as f32 * scale).round() as u32).max(1);
    let nh = ((ih as f32 * scale).round() as u32).max(1);
    let mut scaled = img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3).to_rgb8();
    // Crop the centred `w`×`h` window out of the scaled image.
    let ox = ((nw - w) / 2) as i64;
    let oy = ((nh - h) / 2) as i64;
    image::imageops::crop(&mut scaled, ox.max(0) as u32, oy.max(0) as u32, w, h).to_image()
}

fn encode_png(img: &RgbImage) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img.clone())
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(out.into_inner())
}

/// Encode a 24-bit BGR BMP (rows padded to 4 bytes, bottom-up). `pixels` is
/// row-major RGB, `w`×`h`. Hand-written rather than trusted to the `image`
/// crate's BMP encoder because the exact byte layout (panel size = printer
/// bitmap) is the spooler contract. Pure function: `(pixels, w, h) -> bytes`.
pub fn bmp_24(pixels: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row_stride = w * 3;
    let padded = (row_stride + 3) & !3;
    let file_size = 14 + 40 + padded * h;
    let mut out = Vec::with_capacity(file_size as usize);

    // BITMAPFILEHEADER (14 bytes, little-endian)
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes()); // bfSize
    out.extend_from_slice(&[0, 0, 0, 0]); // bfReserved
    out.extend_from_slice(&((14 + 40) as u32).to_le_bytes()); // bfOffBits
    // BITMAPINFOHEADER (40 bytes)
    out.extend_from_slice(&(40u32).to_le_bytes()); // biSize
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&[1, 0]); // biPlanes
    out.extend_from_slice(&[24, 0]); // biBitCount
    out.extend_from_slice(&(0u32).to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&((padded * h) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    // Pixel rows, bottom-up, BGR, padded to 4 bytes.
    for y in (0..h).rev() {
        let row = &pixels[(y * row_stride) as usize..((y + 1) * row_stride) as usize];
        for x in 0..w {
            let (r, g, b) = (row[(x * 3) as usize], row[(x * 3 + 1) as usize], row[(x * 3 + 2) as usize]);
            out.push(b);
            out.push(g);
            out.push(r);
        }
        out.extend(std::iter::repeat(0).take((padded - row_stride) as usize));
    }
    out
}

// ---------------------------------------------------------------------------
// Public render entry points
// ---------------------------------------------------------------------------

/// Front display image (PNG, fit to the display panel).
pub fn render_front_display(original: &[u8]) -> RenderResult {
    match decode(original).and_then(|img| {
        let canvas = fit_to(&img, DISPLAY_WIDTH, DISPLAY_HEIGHT);
        encode_png(&canvas)
    }) {
        Ok(bytes) => RenderResult::Ready(bytes),
        Err(e) => RenderResult::Failed(e),
    }
}

/// Colour back display image (PNG, fit). A colour back is just another image.
pub fn render_back_display_color(original: &[u8]) -> RenderResult {
    render_front_display(original)
}

/// Front print raster (24-bit BMP at the printer DPI).
pub fn render_front_raster(original: &[u8], size: &RasterSize) -> RenderResult {
    match decode(original) {
        Ok(img) => RenderResult::Ready(bmp_24(
            cover_to(&img, size.width, size.height).as_raw(),
            size.width,
            size.height,
        )),
        Err(e) => RenderResult::Failed(e),
    }
}

/// Colour back print raster (24-bit BMP at the printer DPI).
pub fn render_back_raster_color(original: &[u8], size: &RasterSize) -> RenderResult {
    render_front_raster(original, size)
}

/// A markdown back, rendered to a printer raster and a display PNG.
///
/// This is deliberately a STUB: a real renderer lands in a later pass (the
/// intent is an external markdown→image binary such as Markie, shelled out with
/// a `MARKIE_BIN`). Until then every markdown back reports `failed` with this
/// reason rather than printing a blank side.
///
/// `depth` is the ribbon's ([`Ribbon::back_depth`]): the same markdown is a
/// 24-bit colour raster on YMCKO and a 1-bit one on YMCKOKO, and that is a
/// rendering choice the ribbon makes, never the card. The stub ignores it, but
/// the signature carries it so the real renderer cannot be written without
/// answering the question.
pub fn render_back_markdown_stub(
    _markdown: &str,
    _size: &RasterSize,
    _depth: BackDepth,
) -> RenderResult {
    RenderResult::Failed(
        "the markdown back renderer is not wired up yet; a backend such as Markie is the \
         intended implementation"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_rgb(w: u32, h: u32, v: u8) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb([v, v, v]))
    }

    #[test]
    fn bmp24_header_is_sane() {
        let raw = vec![255u8; 3 * 4 * 4];
        let b = bmp_24(&raw, 4, 4);
        assert_eq!(&b[0..2], b"BM");
        assert_eq!(b[10..14], (54u32).to_le_bytes());
        assert_eq!(b[18..22], 4u32.to_le_bytes());
        assert_eq!(b[22..26], 4u32.to_le_bytes());
        assert_eq!(b[28], 24);
    }

    #[test]
    fn bmp24_rows_are_padded_to_four_bytes() {
        // 5px wide → 15 bytes → padded to 16 per row.
        let raw = vec![0u8; 3 * 5 * 2];
        let b = bmp_24(&raw, 5, 2);
        assert_eq!(&b[0..2], b"BM");
        // size = 54 + 2 rows * 16 = 86.
        assert_eq!(b[2..6], (86u32).to_le_bytes());
    }

    #[test]
    fn display_fit_letterboxes_a_non_panel_input() {
        let img = tiny_rgb(100, 200, 128);
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode");
        let bytes = out.into_inner();
        let res = render_front_display(&bytes);
        assert!(matches!(res, RenderResult::Ready(_)), "{res:?}");
    }

    #[test]
    fn raster_is_a_24_bit_bmp_at_the_requested_size() {
        let size = RasterSize { width: 200, height: 126 };
        let img = tiny_rgb(100, 200, 64);
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode");
        let res = render_front_raster(&out.into_inner(), &size);
        let RenderResult::Ready(bmp) = res else { panic!("failed: {res:?}") };
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp[28], 24);
        assert_eq!(bmp[18..22], 200u32.to_le_bytes());
        assert_eq!(bmp[22..26], 126u32.to_le_bytes());
        let expected_size = 14 + 40 + ((200 * 3 + 3) & !3) * 126;
        assert_eq!(bmp.len(), expected_size, "raster is exactly the printer bitmap");
    }

    #[test]
    fn bad_input_fails_not_panics() {
        assert!(matches!(render_front_display(b"not an image"), RenderResult::Failed(_)));
    }

    #[test]
    fn markdown_back_stub_reports_failed_not_ready() {
        let size = RasterSize { width: 200, height: 126 };
        // Both depths: the stub is a stub on either ribbon.
        for depth in [BackDepth::Color, BackDepth::Black] {
            assert!(matches!(
                render_back_markdown_stub("# Title", &size, depth),
                RenderResult::Failed(_)
            ));
        }
    }
}
