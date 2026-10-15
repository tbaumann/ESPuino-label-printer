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
//! # Scaling
//!
//! The print raster is the printer's *internal bitmap* at the printer DPI — NOT
//! card-mm × dpi. The raster is deliberately a little larger than the card
//! outline: that overscan is what stops an unprinted white rim showing at the
//! card's edge. The default is the Evolis Primacy in 600-dpi mode, 2032×1296
//! against a card outline of 2022×1275; the size is server config
//! (`PRINT_RASTER_WIDTH`/`HEIGHT`), so a printer-specific correction is a config
//! change, not a rebuild.
//!
//! A picture is **scaled to fit inside** that bitmap, centred on the card's
//! white ground — the same framing the display preview shows, one scale factor
//! apart. So what is printed is what the preview shows, and the only difference
//! between the two is the overscan: the outer sliver of the bitmap falls just
//! outside the card edge. (Centre-*cropping* to fill the bitmap instead was a
//! bug: it silently ate the edges of every picture whose aspect differed from
//! the panel's, so a portrait upload printed its middle band.)
//!
//! # Colour depth
//!
//! The ribbon fixes the back's depth ([`crate::models::Ribbon::back_depth`]):
//! YMCKO's back is a colour panel set (24-bit raster), YMCKOKO's is the second
//! black panel (1-bit raster). Both accept every back source — the picture is
//! simply reduced to black or white for the black panel (see
//! [`render_back_raster`]). A 1-bit panel is a poor medium for a photograph,
//! which is the customer's choice to make; nothing here refuses it.

use image::{DynamicImage, GrayImage, Rgb, RgbImage};

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
/// background (the card is a white panel). Used for both the display image and
/// the print raster: the whole picture is visible, letterboxed if the aspect
/// differs from the panel's.
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

/// Rec. 601 luma, the "how dark does this look" figure the 1-bit reduction
/// thresholds on.
fn luma(p: &Rgb<u8>) -> u32 {
    (299 * p[0] as u32 + 587 * p[1] as u32 + 114 * p[2] as u32) / 1000
}

/// Threshold a colour canvas to 1 bit: dark pixels are ink, everything else is
/// the card's white. A plain 50% cut, deliberately — no dithering, no greys; a
/// black panel can make neither.
fn to_ink(canvas: &RgbImage) -> Vec<bool> {
    canvas
        .pixels()
        .map(|p| luma(p) < 128)
        .collect()
}

/// A 1-bit canvas as a greyscale image: pure black where the printer puts ink,
/// pure white everywhere else. This is what an honest preview of a black back
/// paints, and it is the same threshold the raster is built from.
fn ink_to_gray(ink: &[bool], w: u32, h: u32) -> GrayImage {
    let mut out = GrayImage::from_pixel(w, h, image::Luma([255]));
    for (i, &on) in ink.iter().enumerate() {
        if on {
            let (x, y) = (i as u32 % w, i as u32 / w);
            out.put_pixel(x, y, image::Luma([0]));
        }
    }
    out
}

fn encode_png(img: &RgbImage) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img.clone())
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(out.into_inner())
}

fn encode_gray_png(img: &GrayImage) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img.clone())
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

/// Encode a 1-bit BMP (rows padded to 4 bytes, bottom-up, MSB first) with a
/// black/white palette. `ink` is row-major, `w`×`h`; `true` is a black pixel
/// (palette index 0), `false` the card's white (index 1). Same contract as
/// [`bmp_24`]: the bytes *are* the printer bitmap, and the spooler hands them
/// to `set_black_buffer` verbatim.
pub fn bmp_1(ink: &[bool], w: u32, h: u32) -> Vec<u8> {
    assert_eq!(ink.len(), (w as usize) * (h as usize), "ink mask size");
    let row_stride = ((w as usize) + 7) / 8;
    let padded = (row_stride + 3) & !3;
    let palette = 8; // two BGRA quads
    let file_size = 14 + 40 + palette + padded * h as usize;
    let mut out = Vec::with_capacity(file_size);

    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&((14 + 40 + palette) as u32).to_le_bytes()); // bfOffBits
    // BITMAPINFOHEADER
    out.extend_from_slice(&(40u32).to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&[1, 0]); // biPlanes
    out.extend_from_slice(&[1, 0]); // biBitCount
    out.extend_from_slice(&(0u32).to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((padded * h as usize) as u32).to_le_bytes());
    out.extend_from_slice(&(2u32).to_le_bytes()); // biClrUsed
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // biClrImportant..padding
    // Palette: index 0 black (ink), index 1 white (card).
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&[255, 255, 255, 0]);

    for y in (0..h).rev() {
        let row = &ink[(y * w) as usize..((y + 1) * w) as usize];
        for byte in 0..row_stride {
            let mut b = 0u8;
            for bit in 0..8 {
                let x = byte * 8 + bit;
                // Bit set = palette index 1 = white; ink is a cleared bit.
                if x < w as usize && !row[x] {
                    b |= 0x80 >> bit;
                }
            }
            out.push(b);
        }
        out.extend(std::iter::repeat(0).take(padded - row_stride));
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

/// Front print raster (24-bit BMP at the printer DPI).
pub fn render_front_raster(original: &[u8], size: &RasterSize) -> RenderResult {
    match decode(original) {
        Ok(img) => RenderResult::Ready(bmp_24(
            fit_to(&img, size.width, size.height).as_raw(),
            size.width,
            size.height,
        )),
        Err(e) => RenderResult::Failed(e),
    }
}

/// A back display image (PNG, fit to the display panel), at the depth the
/// ribbon prints it in: the colour picture on a colour back, the same picture
/// thresholded to black and white on a black panel.
///
/// The preview is deliberately the depth-accurate render rather than always the
/// colour picture: what the customer sees on the back panel is what comes out
/// of the printer, so changing the ribbon repaints the back previews.
pub fn render_back_display(original: &[u8], depth: BackDepth) -> RenderResult {
    match decode(original) {
        Ok(img) => {
            let canvas = fit_to(&img, DISPLAY_WIDTH, DISPLAY_HEIGHT);
            let encoded = match depth {
                BackDepth::Color => encode_png(&canvas),
                BackDepth::Black => {
                    encode_gray_png(&ink_to_gray(&to_ink(&canvas), DISPLAY_WIDTH, DISPLAY_HEIGHT))
                }
            };
            match encoded {
                Ok(bytes) => RenderResult::Ready(bytes),
                Err(e) => RenderResult::Failed(e),
            }
        }
        Err(e) => RenderResult::Failed(e),
    }
}

/// A back print raster at the printer DPI, at the depth the ribbon prints it
/// in: a 24-bit colour BMP on a colour back, a 1-bit BMP on a black panel.
///
/// Both depths take the same picture; the black one is reduced by
/// [`to_ink`]. A picture is not what a black panel is good at, and it is not
/// this module's business to stop anyone printing one.
pub fn render_back_raster(original: &[u8], size: &RasterSize, depth: BackDepth) -> RenderResult {
    match decode(original) {
        Ok(img) => {
            let canvas = fit_to(&img, size.width, size.height);
            RenderResult::Ready(match depth {
                BackDepth::Color => bmp_24(canvas.as_raw(), size.width, size.height),
                BackDepth::Black => {
                    bmp_1(&to_ink(&canvas), size.width, size.height)
                }
            })
        }
        Err(e) => RenderResult::Failed(e),
    }
}

/// A markdown back, rendered to a printer raster and a display PNG.
///
/// This is deliberately a STUB: a real renderer lands in a later pass (the
/// intent is an external markdown→image binary such as Markie, shelled out with
/// a `MARKIE_BIN`). Until then every markdown back reports `failed` with this
/// reason rather than printing a blank side.
///
/// `depth` is the ribbon's ([`crate::models::Ribbon::back_depth`]): the same
/// markdown is a 24-bit colour raster on YMCKO and a 1-bit one on YMCKOKO, and
/// that is a rendering choice the ribbon makes, never the card. The stub ignores
/// it, but the signature carries it so the real renderer cannot be written
/// without answering the question.
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

    fn png_of(img: RgbImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode");
        out.into_inner()
    }

    fn read_bmp(bytes: &[u8]) -> image::DynamicImage {
        image::load_from_memory_with_format(bytes, image::ImageFormat::Bmp).expect("bmp decodes")
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
    fn bmp1_is_one_bit_with_a_black_white_palette() {
        // A 10x2 mask: left half ink, right half white.
        let mut ink = vec![false; 20];
        for y in 0..2 {
            for x in 0..5 {
                ink[y * 10 + x] = true;
            }
        }
        let b = bmp_1(&ink, 10, 2);
        assert_eq!(&b[0..2], b"BM");
        assert_eq!(b[10..14], (62u32).to_le_bytes(), "14 + 40 + 8 palette");
        assert_eq!(b[28], 1, "1 bit per pixel");
        // 10px → 2 bytes, padded to 4; two rows → 8.
        assert_eq!(b[2..6], ((62 + 8) as u32).to_le_bytes());
        // The palette is black then white, so index 0 (a cleared bit) is ink.
        assert_eq!(&b[54..58], &[0, 0, 0, 0], "palette 0 is black");
        assert_eq!(&b[58..62], &[255, 255, 255, 0], "palette 1 is white");
    }

    #[test]
    fn bmp1_round_trips_through_the_image_crate() {
        let mut ink = vec![false; 8 * 4];
        for x in 0..4 {
            ink[3 * 8 + x] = true;
        }
        let decoded = read_bmp(&bmp_1(&ink, 8, 4)).to_luma8();
        assert_eq!(decoded.dimensions(), (8, 4));
        for y in 0..4 {
            for x in 0..8 {
                let want = if y == 3 && x < 4 { 0 } else { 255 };
                assert_eq!(
                    decoded.get_pixel(x, y)[0],
                    want,
                    "pixel ({x},{y}) of the 1-bit raster"
                );
            }
        }
    }

    #[test]
    fn display_fit_letterboxes_a_non_panel_input() {
        let bytes = png_of(tiny_rgb(100, 200, 128));
        let res = render_front_display(&bytes);
        assert!(matches!(res, RenderResult::Ready(_)), "{res:?}");
    }

    #[test]
    fn raster_is_a_24_bit_bmp_at_the_requested_size() {
        let size = RasterSize { width: 200, height: 126 };
        let bytes = png_of(tiny_rgb(100, 200, 64));
        let res = render_front_raster(&bytes, &size);
        let RenderResult::Ready(bmp) = res else { panic!("failed: {res:?}") };
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp[28], 24);
        assert_eq!(bmp[18..22], 200u32.to_le_bytes());
        assert_eq!(bmp[22..26], 126u32.to_le_bytes());
        let expected_size = 14 + 40 + ((200 * 3 + 3) & !3) * 126;
        assert_eq!(bmp.len(), expected_size, "raster is exactly the printer bitmap");
    }

    /// The scaling contract: the picture is fitted *inside* the printer bitmap,
    /// letterboxed on white — not cropped to fill it. A tall picture must
    /// therefore keep its full height in the raster.
    #[test]
    fn raster_letterboxes_a_tall_picture_instead_of_cropping_it() {
        let size = RasterSize { width: 400, height: 200 };
        // 100x400: taller than the panel, so fitting it leaves white margins
        // left and right and the picture spans the full height.
        let bytes = png_of(RgbImage::from_pixel(100, 400, Rgb([10, 10, 10])));
        let RenderResult::Ready(bmp) = render_front_raster(&bytes, &size) else {
            panic!("raster failed")
        };
        let img = read_bmp(&bmp).to_rgb8();
        // Top edge of the bitmap is picture (dark), not crop-and-zoom band.
        assert!(luma(img.get_pixel(200, 0)) < 128, "top is artwork");
        assert!(luma(img.get_pixel(200, 199)) < 128, "bottom is artwork");
        // Left column is the white ground the picture was letterboxed onto.
        assert!(luma(img.get_pixel(0, 100)) > 200, "left is white ground");
    }

    /// A black-panel back is a 1-bit raster built from the same framing, and the
    /// display preview is that same reduction — ink stays ink, paper stays paper.
    #[test]
    fn black_back_raster_and_display_agree() {
        let size = RasterSize { width: 100, height: 100 };
        // A 100x100 canvas: black on the left half, white on the right.
        let mut img = RgbImage::from_pixel(100, 100, Rgb([255, 255, 255]));
        for y in 0..100 {
            for x in 0..50 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let bytes = png_of(img);

        let RenderResult::Ready(bmp) = render_back_raster(&bytes, &size, BackDepth::Black) else {
            panic!("black raster failed")
        };
        assert_eq!(bmp[28], 1, "one bit per pixel on a black back");
        assert!(!bmp.is_empty());

        let color = render_back_raster(&bytes, &size, BackDepth::Color);
        let RenderResult::Ready(color) = color else { panic!("colour raster failed") };
        assert_eq!(color[28], 24, "a colour back stays 24-bit");

        let RenderResult::Ready(display) = render_back_display(&bytes, BackDepth::Black) else {
            panic!("black display failed")
        };
        let shown = image::load_from_memory(&display).expect("png").to_luma8();
        // The square picture is fitted into the 856x540 display panel, so it
        // spans x 158..698, centred; sample inside each half of it.
        assert_eq!(shown.get_pixel(200, 270)[0], 0, "ink previews as black");
        assert_eq!(shown.get_pixel(680, 270)[0], 255, "paper previews as white");
    }

    #[test]
    fn bad_input_fails_not_panics() {
        assert!(matches!(render_front_display(b"not an image"), RenderResult::Failed(_)));
        let size = RasterSize { width: 200, height: 126 };
        assert!(matches!(
            render_back_raster(b"not an image", &size, BackDepth::Black),
            RenderResult::Failed(_)
        ));
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
