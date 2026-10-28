//! Server-side image processing: normalize uploads into a canonical display
//! image, and produce printer-native rasters the spooler consumes unchanged.
//!
//! One renderer owns the truth: what the preview shows and what the spooler
//! prints are *this* module's output — the same bytes, one scale factor apart.
//! Nothing about a back is drawn in the browser, markdown included.
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
//! Where a picture lands on that bitmap is [`Placement`], and it is the picture's
//! own business rather than the card's or the ribbon's. An **uploaded** picture
//! (the front, a second image, a copy of the front) is **scaled to fit inside**
//! the bitmap and centred on the card's white ground: the whole picture is
//! visible, and the display preview shows the same framing one scale factor
//! apart. A **markdown render** is anchored at the panel's top-left corner
//! instead: it is a document laid out at the card's width from its first line,
//! so the card is a window onto the top of it — the text keeps the size the
//! layout gave it, and whatever runs past the panel's bottom edge is clipped.
//! (Centre-*cropping* to fill the bitmap was a bug for an upload: it silently ate
//! the edges of every picture whose aspect differed from the panel's, so a
//! portrait upload printed its middle band.)
//!
//! # Colour depth
//!
//! The ribbon fixes the back's depth ([`crate::models::Ribbon::back_depth`]):
//! YMCKO's back is a colour panel set (24-bit raster), YMCKOKO's is the second
//! black panel (1-bit raster). Both accept every back source — the picture is
//! simply reduced to black or white for the black panel (see
//! [`render_back_raster`]). A 1-bit panel is a poor medium for a photograph,
//! which is the customer's choice to make; nothing here refuses it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use image::{DynamicImage, GrayImage, Rgb, RgbImage};
use uuid::Uuid;

use crate::models::BackDepth;

/// The display image the frontend shows: aspect-correct, small. The CR80 panel
/// at 10 px/mm.
pub const DISPLAY_WIDTH: u32 = 856;
pub const DISPLAY_HEIGHT: u32 = 540;

/// Printer raster size, from config or the 600-dpi Primacy default.
#[derive(Debug, Clone, Copy)]
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
        Self {
            width: w,
            height: h,
        }
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

/// How a picture is placed on the card's panel.
///
/// It is a property of what the picture *is*, so every caller names it rather
/// than inheriting a default: a picture that is a photograph is framed, a
/// picture that is a laid-out document is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Scale the picture to fit inside the panel, preserving aspect, and centre
    /// it on the card's white ground: all of it is visible, letterboxed where
    /// the aspect differs from the panel's. What an uploaded picture gets — the
    /// picture *is* the card side, so none of it may go missing.
    Fit,
    /// Scale the picture to the panel's **width** and anchor it at the panel's
    /// top-left corner (0, 0), clipping whatever runs past the panel's right or
    /// bottom edge. What a markdown render gets: the text is laid out at the
    /// card's own size from its first line, so a long song list is cut at the
    /// card's edge rather than shrunk to fit it.
    TopLeft,
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
    let scaled = img
        .resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
        .to_rgb8();
    let mut canvas = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
    let ox = (w - nw) / 2;
    let oy = (h - nh) / 2;
    image::imageops::overlay(&mut canvas, &scaled, ox as i64, oy as i64);
    canvas
}

/// Scale an image to the width `w`, preserving aspect, and draw it from the
/// panel's top-left corner: the panel is a window onto the top of the picture,
/// and what falls outside it is dropped rather than squeezed in.
fn top_left(img: &DynamicImage, w: u32, h: u32) -> RgbImage {
    let (iw, ih) = (img.width().max(1), img.height().max(1));
    let scale = w as f32 / iw as f32;
    let nw = ((iw as f32 * scale).round() as u32).max(1);
    let nh = ((ih as f32 * scale).round() as u32).max(1);
    let scaled = img
        .resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
        .to_rgb8();
    // The crop is explicit: the window is the panel, and a picture taller or
    // wider than it is cut here rather than fitted to it.
    let visible = image::imageops::crop_imm(&scaled, 0, 0, w.min(nw), h.min(nh)).to_image();
    let mut canvas = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
    image::imageops::replace(&mut canvas, &visible, 0, 0);
    canvas
}

/// Place a picture on a white `w`×`h` panel, the way `how` says. The only
/// difference between the two placements is where the picture lands on the
/// panel; the colour depth is applied afterwards, to the panel, by the caller.
fn place(img: &DynamicImage, w: u32, h: u32, how: Placement) -> RgbImage {
    match how {
        Placement::Fit => fit_to(img, w, h),
        Placement::TopLeft => top_left(img, w, h),
    }
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
    canvas.pixels().map(|p| luma(p) < 128).collect()
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
    out.extend_from_slice(&file_size.to_le_bytes()); // bfSize
    out.extend_from_slice(&[0, 0, 0, 0]); // bfReserved
    out.extend_from_slice(&((14 + 40) as u32).to_le_bytes()); // bfOffBits
                                                              // BITMAPINFOHEADER (40 bytes)
    out.extend_from_slice(&(40u32).to_le_bytes()); // biSize
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&[1, 0]); // biPlanes
    out.extend_from_slice(&[24, 0]); // biBitCount
    out.extend_from_slice(&(0u32).to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(padded * h).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    // Pixel rows, bottom-up, BGR, padded to 4 bytes.
    for y in (0..h).rev() {
        let row = &pixels[(y * row_stride) as usize..((y + 1) * row_stride) as usize];
        for x in 0..w {
            let (r, g, b) = (
                row[(x * 3) as usize],
                row[(x * 3 + 1) as usize],
                row[(x * 3 + 2) as usize],
            );
            out.push(b);
            out.push(g);
            out.push(r);
        }
        out.extend(std::iter::repeat_n(0, (padded - row_stride) as usize));
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
    let row_stride = (w as usize).div_ceil(8);
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
        out.extend(std::iter::repeat_n(0, padded - row_stride));
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

/// A back display image (PNG, at the display panel), at the depth the ribbon
/// prints it in: the colour picture on a colour back, the same picture
/// thresholded to black and white on a black panel.
///
/// The preview is deliberately the depth-accurate render rather than always the
/// colour picture: what the customer sees on the back panel is what comes out
/// of the printer, so changing the ribbon repaints the back previews.
///
/// `placement` is the picture's own ([`Placement`]): the preview frames an
/// uploaded picture exactly as the raster does, so the two are one picture at
/// two sizes.
pub fn render_back_display(
    original: &[u8],
    depth: BackDepth,
    placement: Placement,
) -> RenderResult {
    match decode(original) {
        Ok(img) => {
            let canvas = place(&img, DISPLAY_WIDTH, DISPLAY_HEIGHT, placement);
            let encoded = match depth {
                BackDepth::Color => encode_png(&canvas),
                BackDepth::Black => encode_gray_png(&ink_to_gray(
                    &to_ink(&canvas),
                    DISPLAY_WIDTH,
                    DISPLAY_HEIGHT,
                )),
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
///
/// `placement` decides where the picture lands on the panel — see
/// [`Placement`], and [`render_back_display`] for the same framing at preview
/// size.
pub fn render_back_raster(
    original: &[u8],
    size: &RasterSize,
    depth: BackDepth,
    placement: Placement,
) -> RenderResult {
    match decode(original) {
        Ok(img) => {
            let canvas = place(&img, size.width, size.height, placement);
            RenderResult::Ready(match depth {
                BackDepth::Color => bmp_24(canvas.as_raw(), size.width, size.height),
                BackDepth::Black => bmp_1(&to_ink(&canvas), size.width, size.height),
            })
        }
        Err(e) => RenderResult::Failed(e),
    }
}

/// A markdown back, rendered by the external backend into a picture.
///
/// The backend is `markie` (a native markdown→PNG/SVG renderer, packaged as this
/// flake's `markie`): it is handed the text on stdin and writes one PNG, which
/// this returns as-is. The picture is *not* framed or depth-reduced here — the
/// ordinary back path ([`render_back_display`], [`render_back_raster`]) places
/// it at the panel's top-left and reduces it to the ribbon's depth, at
/// [`Placement::TopLeft`] rather than by fitting it. That is what keeps the
/// preview and the print the same picture, one scale factor apart, for a
/// markdown back exactly as for an uploaded one.
///
/// Markdown may reference pictures, and a **remote** one (`https://…`) is
/// fetched by the backend — 10 s timeout, 10 MiB cap. So a card back can make
/// the server issue one HTTP GET to a host the customer names. A picture that
/// cannot be fetched does *not* fail the render: the backend warns and lays out
/// the alt text in its place, so the card prints without the picture. That
/// warning is relayed to the log rather than swallowed — the customer's artwork
/// silently missing is worth a line somewhere, and it is nowhere on the card.
/// A *local* reference resolves against the document's directory, of which this
/// render has none, so it takes the same alt-text path.
#[derive(Debug, Clone)]
pub struct MarkdownRenderer {
    /// `markie`, or an absolute path to one (`MARKIE_BIN`).
    bin: PathBuf,
    /// Where its output file goes: `<DATA_DIR>/tmp`. The data directory is the
    /// one directory this process is entitled to write to — a container has no
    /// writable `/tmp` and nothing else to lean on.
    scratch: PathBuf,
}

/// The width a markdown back is laid out at, in the backend's pixels: the
/// card's long edge (85.6 mm) at the scale where the backend's fixed 16 px body
/// text lands around 2.7 mm tall — the size a card back is legible at. Laying
/// it out at the printer's raster width instead would print 0.7 mm text.
///
/// The raster is rendered from this at whatever `--png-scale` reaches the
/// raster's width, so the wrapping and the text size stay a property of the
/// *card*: a change of print DPI re-renders sharper, it does not re-flow the
/// text or shrink it.
pub const MARKDOWN_WIDTH: u32 = 508;

/// The theme a markdown back is drawn in: a flat white ground with dark text,
/// which is what a card wants (and what the black panel's own threshold expects).
/// Pinned by name because a backend's default is not a contract.
const MARKDOWN_THEME: &str = "github_light";

impl MarkdownRenderer {
    pub fn new(bin: impl Into<PathBuf>, scratch: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            scratch: scratch.into(),
        }
    }

    /// `MARKIE_BIN`, else `markie` from `PATH`; the scratch directory is
    /// `DATA_DIR/tmp`.
    ///
    /// Resolved once at startup and handed to the renderer as a value, like
    /// [`RasterSize`]: a renderer that read the process environment per card
    /// would make what a card looks like depend on the ambient environment
    /// rather than on its inputs.
    pub fn from_env(data_dir: &Path) -> Self {
        let bin =
            std::env::var_os("MARKIE_BIN").map_or_else(|| PathBuf::from("markie"), PathBuf::from);
        Self::new(bin, data_dir.join("tmp"))
    }

    /// Whether the backend can be run at all — a startup check, so that a
    /// missing one is a line in the log rather than a discovery made one
    /// customer's card at a time.
    pub fn probe(&self) -> Result<(), String> {
        let status = Command::new(&self.bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| self.cannot_run(&e))?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| format!("{} --version exited with {status}", self.bin.display()))
    }

    /// Render `markdown` into a picture as wide as the card is long.
    pub fn render(&self, markdown: &str, size: &RasterSize) -> RenderResult {
        match self.run(markdown, size) {
            Ok(png) => RenderResult::Ready(png),
            Err(e) => RenderResult::Failed(e),
        }
    }

    fn run(&self, markdown: &str, size: &RasterSize) -> Result<Vec<u8>, String> {
        std::fs::create_dir_all(&self.scratch)
            .map_err(|e| format!("markdown scratch directory {}: {e}", self.scratch.display()))?;
        // One render, one file, named for this call: a ribbon switch re-renders
        // every card of a prefix, and two of them must not write over each
        // other's picture.
        let out = self
            .scratch
            .join(format!("markdown-{}.png", Uuid::new_v4()));

        // The layout is the card's; the raster follows it at whatever scale
        // reaches the printer's bitmap. So a test that renders at 400x256 gets
        // the card's own text size rather than a re-flowed, shrunken one.
        let scale = size.width as f32 / MARKDOWN_WIDTH as f32;

        let mut child = Command::new(&self.bin)
            .arg("-")
            .arg("--output")
            .arg(&out)
            .arg("--width")
            .arg(MARKDOWN_WIDTH.to_string())
            .arg("--png-scale")
            .arg(scale.to_string())
            .arg("--theme")
            .arg(MARKDOWN_THEME)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| self.cannot_run(&e))?;

        // Hand the text over and close the pipe. The backend reads all of its
        // input before it renders anything, so a write error means it is already
        // gone — a broken installation, or something that killed it.
        let handed_over = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(markdown.as_bytes()));
        if let Some(Err(e)) = handed_over {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&out);
            return Err(format!(
                "could not hand the markdown to {}: {e}",
                self.bin.display()
            ));
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("{}: {e}", self.bin.display()))?;

        if !output.status.success() {
            let _ = std::fs::remove_file(&out);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let said = stderr.trim();
            // The backend prefixes its one-line reasons with `Error:`.
            let said = said.strip_prefix("Error: ").unwrap_or(said).trim();
            return Err(if said.is_empty() {
                "the markdown renderer failed without saying why".into()
            } else {
                format!("markdown render: {said}")
            });
        }

        let png = std::fs::read(&out)
            .map_err(|e| format!("markdown rendering {}: {e}", out.display()))?;
        let _ = std::fs::remove_file(&out);

        // A render that *succeeded* can still have something to say — a picture
        // it could not fetch, laid out as its alt text instead. The card is
        // printable either way, so this does not fail it; it is a line in the
        // log, because nothing on the card says the artwork went missing.
        //
        // The backend writes its own progress there too ("PNG saved to: …"), so
        // only the lines it prefixes with `Warning:` are relayed — a warning per
        // card about a file the server itself is about to delete is noise.
        let said = String::from_utf8_lossy(&output.stderr);
        for warning in said
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("Warning:"))
        {
            tracing::warn!("markdown render: {warning}");
        }
        Ok(png)
    }

    /// Why the backend could not be started at all. Worth naming precisely: the
    /// fix is configuration, not the customer's text.
    fn cannot_run(&self, e: &std::io::Error) -> String {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "no markdown renderer at {} (set MARKIE_BIN)",
                self.bin.display()
            )
        } else {
            format!("could not run {}: {e}", self.bin.display())
        }
    }
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
        let size = RasterSize {
            width: 200,
            height: 126,
        };
        let bytes = png_of(tiny_rgb(100, 200, 64));
        let res = render_front_raster(&bytes, &size);
        let RenderResult::Ready(bmp) = res else {
            panic!("failed: {res:?}")
        };
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp[28], 24);
        assert_eq!(bmp[18..22], 200u32.to_le_bytes());
        assert_eq!(bmp[22..26], 126u32.to_le_bytes());
        let expected_size = 14 + 40 + ((200 * 3 + 3) & !3) * 126;
        assert_eq!(
            bmp.len(),
            expected_size,
            "raster is exactly the printer bitmap"
        );
    }

    /// The scaling contract: the picture is fitted *inside* the printer bitmap,
    /// letterboxed on white — not cropped to fill it. A tall picture must
    /// therefore keep its full height in the raster.
    #[test]
    fn raster_letterboxes_a_tall_picture_instead_of_cropping_it() {
        let size = RasterSize {
            width: 400,
            height: 200,
        };
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

    /// A markdown render is a *document*, and the card is a window onto its top:
    /// anchored at the panel's top-left, at the width the layout was made for,
    /// with what runs past the panel clipped rather than shrunk to fit.
    ///
    /// The document here is the panel's own width and three times its height,
    /// with black rules at its top, at a row inside the panel and at a row well
    /// past it — so position, width and clipping are each visible in the result.
    #[test]
    fn a_markdown_render_is_anchored_at_the_panel_top_left() {
        let size = RasterSize {
            width: 400,
            height: 200,
        };
        let mut doc = RgbImage::from_pixel(400, 600, Rgb([255, 255, 255]));
        for y in [0, 1, 2, 150, 151, 400, 401] {
            for x in 0..400 {
                doc.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let bytes = png_of(doc);

        let RenderResult::Ready(bmp) =
            render_back_raster(&bytes, &size, BackDepth::Color, Placement::TopLeft)
        else {
            panic!("raster failed")
        };
        let raster = read_bmp(&bmp).to_rgb8();
        assert_eq!(raster.dimensions(), (400, 200), "the panel is the raster");

        // The document's first line is the panel's first line: no letterbox band
        // above it, and the ink runs the panel's full width.
        assert!(luma(raster.get_pixel(0, 0)) < 128, "starts at the top-left");
        assert!(
            luma(raster.get_pixel(399, 1)) < 128,
            "spans the panel's width"
        );
        // The rule at the document's row 150 is at the raster's row 150: the
        // text keeps the size the layout gave it, which is what "fitting" a
        // 400x600 document into 400x200 would have halved.
        assert!(
            luma(raster.get_pixel(200, 151)) < 128,
            "the document is not scaled down"
        );
        assert!(
            luma(raster.get_pixel(200, 100)) > 200,
            "and not repainted where the document is blank"
        );

        // The preview is the same window, one scale factor apart: 856/400 of 600
        // document rows is 1284, of which the 540-row panel shows the top 252.
        let RenderResult::Ready(png) =
            render_back_display(&bytes, BackDepth::Color, Placement::TopLeft)
        else {
            panic!("display failed")
        };
        let shown = image::load_from_memory(&png).expect("png").to_rgb8();
        assert!(
            luma(shown.get_pixel(0, 0)) < 128,
            "the preview is anchored too"
        );
        // Document row 150 lands at display row 321; the rows past the panel
        // (from document row 252 on) are simply not there.
        assert!(
            luma(shown.get_pixel(400, 322)) < 128,
            "the same rule, at preview scale"
        );
        assert!(
            luma(shown.get_pixel(400, 500)) > 200,
            "the bottom of the preview is the document's own blank paper"
        );
    }

    /// A black-panel back is a 1-bit raster built from the same framing, and the
    /// display preview is that same reduction — ink stays ink, paper stays paper.
    #[test]
    fn black_back_raster_and_display_agree() {
        let size = RasterSize {
            width: 100,
            height: 100,
        };
        // A 100x100 canvas: black on the left half, white on the right.
        let mut img = RgbImage::from_pixel(100, 100, Rgb([255, 255, 255]));
        for y in 0..100 {
            for x in 0..50 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let bytes = png_of(img);

        let RenderResult::Ready(bmp) =
            render_back_raster(&bytes, &size, BackDepth::Black, Placement::Fit)
        else {
            panic!("black raster failed")
        };
        assert_eq!(bmp[28], 1, "one bit per pixel on a black back");
        assert!(!bmp.is_empty());

        let color = render_back_raster(&bytes, &size, BackDepth::Color, Placement::Fit);
        let RenderResult::Ready(color) = color else {
            panic!("colour raster failed")
        };
        assert_eq!(color[28], 24, "a colour back stays 24-bit");

        let RenderResult::Ready(display) =
            render_back_display(&bytes, BackDepth::Black, Placement::Fit)
        else {
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
        assert!(matches!(
            render_front_display(b"not an image"),
            RenderResult::Failed(_)
        ));
        let size = RasterSize {
            width: 200,
            height: 126,
        };
        assert!(matches!(
            render_back_raster(b"not an image", &size, BackDepth::Black, Placement::Fit),
            RenderResult::Failed(_)
        ));
    }

    /// A backend that is not there is a *reason* on the card, never a panic and
    /// never a stray file: the fix is configuration, so the message says which
    /// knob, and the scratch directory is left empty.
    #[test]
    fn a_missing_markdown_backend_fails_and_leaves_nothing_behind() {
        let size = RasterSize {
            width: 400,
            height: 256,
        };
        let root = std::env::temp_dir().join(format!("esplp-md-{}", Uuid::new_v4()));
        let renderer = MarkdownRenderer::new("markie-that-is-not-installed", root.join("tmp"));

        let res = renderer.render("# Title", &size);
        let RenderResult::Failed(why) = res else {
            panic!("a backend that is not installed cannot render: {res:?}")
        };
        assert!(why.contains("MARKIE_BIN"), "{why}");
        assert!(renderer.probe().is_err(), "the startup check agrees");

        let left: Vec<_> = std::fs::read_dir(root.join("tmp"))
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        assert!(left.is_empty(), "scratch holds {left:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
