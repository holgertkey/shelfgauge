//! Images of the books made smaller for other devices: the cover thumbnails of the
//! catalog and the images of publications and book files.
//!
//! Images come from the books, so they are untrusted: every decode keeps to small
//! limits (8192 px a side, 256 MiB), and anything that does not decode within them is
//! kept as it is (it was shown as it is before) or, for a thumbnail, left out.

use std::io::Cursor;

use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageReader};

/// The widest image of a publication: the full width of a phone's column at its
/// device pixel ratio (about 360–430 CSS px at 2.6–3), sharp. The readers limit
/// images by width only, so the height follows freely.
pub const MAX_IMAGE_WIDTH: u32 = 1080;

/// The JPEG quality of a reduced image.
pub const JPEG_QUALITY: u8 = 80;

/// The largest side a book's image may have to be decoded.
const MAX_SIDE: u32 = 8192;

/// The most memory a decode may take.
const MAX_ALLOC: u64 = 256 * 1024 * 1024;

/// The most colors an image may have to stay PNG when reduced (line art, diagrams:
/// a JPEG of those is larger and blurred).
const FEW_COLORS: usize = 256;

/// A reader of `data` that guesses its format and decodes within the limits (at most
/// 8192 px a side, 256 MiB); `None` when the format is not known.
pub(crate) fn reader(data: &[u8]) -> Option<ImageReader<Cursor<&[u8]>>> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    Some(reader)
}

/// Whether `data` is a GIF with more than one frame.
fn animated_gif(data: &[u8]) -> bool {
    use image::AnimationDecoder;
    let Ok(decoder) = image::codecs::gif::GifDecoder::new(Cursor::new(data)) else {
        return false;
    };
    decoder.into_frames().take(2).count() > 1
}

/// Whether `pixels` have at most [`FEW_COLORS`] colors (stops at the first past it).
fn few_colors<P: Eq + std::hash::Hash>(pixels: impl Iterator<Item = P>) -> bool {
    let mut seen = std::collections::HashSet::new();
    for pixel in pixels {
        if seen.insert(pixel) && seen.len() > FEW_COLORS {
            return false;
        }
    }
    true
}

/// Whether `image` has transparent pixels, and whether it has few colors (then it is
/// kept lossless). The common pixel types are read in place; others are converted once.
fn transparent_or_few_colors(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageRgb8(rgb) => few_colors(rgb.pixels().map(|p| p.0)),
        DynamicImage::ImageLuma8(luma) => few_colors(luma.pixels().map(|p| p.0)),
        DynamicImage::ImageRgba8(rgba) => {
            rgba.pixels().any(|p| p.0[3] < 255) || few_colors(rgba.pixels().map(|p| p.0))
        }
        other => {
            let rgba = other.to_rgba8();
            (other.color().has_alpha() && rgba.pixels().any(|p| p.0[3] < 255))
                || few_colors(rgba.pixels().map(|p| p.0))
        }
    }
}

/// Whether `orientation` swaps width and height.
fn turns(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// `data` decoded within the limits and turned as its EXIF orientation says (as a
/// browser shows it; a broken orientation counts as none); `None` when it does not
/// decode. With `max_width`, also `None` when the image as shown is not wider (then
/// nothing is decoded).
pub(crate) fn decode_upright(data: &[u8], max_width: Option<u32>) -> Option<DynamicImage> {
    use image::ImageDecoder;
    let mut decoder = reader(data)?.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (width, height) = decoder.dimensions();
    let shown_width = if turns(orientation) { height } else { width };
    if max_width.is_some_and(|max| shown_width <= max) {
        return None;
    }
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

/// `data` (of type `mime`) made smaller for a publication: an image wider than
/// [`MAX_IMAGE_WIDTH`] scaled down to that width (the height following, Lanczos3) and
/// encoded as JPEG at [`JPEG_QUALITY`], or as PNG when it has transparency or few
/// colors. The width is the one shown: a photo turned by its EXIF orientation is
/// turned upright first. Re-encoding drops EXIF and other metadata. `None` keeps the
/// original: an image not wider than that, one that does not decode within the
/// limits, an animated GIF (its first frame alone would change the book), or a
/// result that is not smaller.
///
/// # Examples
///
/// ```
/// use shelfgauge::images::{MAX_IMAGE_WIDTH, reduce};
///
/// let mut photo = Vec::new();
/// image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(3000, 2000, |x, y| {
///     image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x * y) % 239) as u8])
/// }))
/// .write_to(&mut std::io::Cursor::new(&mut photo), image::ImageFormat::Png)
/// .unwrap();
/// let (smaller, mime) = reduce(&photo, "image/png").unwrap();
/// assert_eq!(mime, "image/jpeg");
/// assert!(smaller.len() < photo.len());
/// let decoded = image::load_from_memory(&smaller).unwrap();
/// assert_eq!((decoded.width(), decoded.height()), (MAX_IMAGE_WIDTH, 720));
/// assert!(reduce(b"not an image", "image/png").is_none());
/// ```
pub fn reduce(data: &[u8], mime: &str) -> Option<(Vec<u8>, &'static str)> {
    use image::ImageEncoder;
    if mime == "image/gif" && animated_gif(data) {
        return None;
    }
    let image = decode_upright(data, Some(MAX_IMAGE_WIDTH))?;
    let (width, height) = (image.width(), image.height());
    let lossless = transparent_or_few_colors(&image);
    let new_height = (u64::from(height) * u64::from(MAX_IMAGE_WIDTH) / u64::from(width)).max(1);
    let small = image.resize_exact(MAX_IMAGE_WIDTH, new_height as u32, FilterType::Lanczos3);
    let mut out = Vec::new();
    let mime = if lossless {
        let rgba = small.to_rgba8();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(
                &rgba,
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .ok()?;
        "image/png"
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .encode_image(&small.to_rgb8())
            .ok()?;
        "image/jpeg"
    };
    (out.len() < data.len()).then_some((out, mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `image` encoded as `format`.
    fn encoded(image: image::DynamicImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = Vec::new();
        image.write_to(&mut Cursor::new(&mut out), format).unwrap();
        out
    }

    /// A photo-like image (many colors) of `width`×`height`.
    fn photo(width: u32, height: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x * y) % 239) as u8])
        }))
    }

    fn size(data: &[u8]) -> (u32, u32) {
        let decoded = image::load_from_memory(data).unwrap();
        (decoded.width(), decoded.height())
    }

    #[test]
    fn a_wide_photo_becomes_a_smaller_jpeg_of_the_full_phone_width() {
        let jpeg = encoded(photo(3000, 2000), image::ImageFormat::Jpeg);
        let (smaller, mime) = reduce(&jpeg, "image/jpeg").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert!(smaller.len() < jpeg.len());
        assert_eq!(size(&smaller), (1080, 720));
    }

    #[test]
    fn the_width_is_capped_not_the_longer_side() {
        let portrait = encoded(photo(1500, 3000), image::ImageFormat::Png);
        let (smaller, _) = reduce(&portrait, "image/png").unwrap();
        assert_eq!(size(&smaller), (1080, 2160));
        // Narrow enough already, however tall: kept as it is.
        let strip = encoded(photo(900, 4000), image::ImageFormat::Png);
        assert!(reduce(&strip, "image/png").is_none());
        let small = encoded(photo(600, 400), image::ImageFormat::Jpeg);
        assert!(reduce(&small, "image/jpeg").is_none());
    }

    #[test]
    fn transparency_and_line_art_stay_png() {
        let mut alpha = image::RgbaImage::from_fn(2000, 1000, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 241) as u8, 9, (x % 200) as u8])
        });
        alpha.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        let png = encoded(
            image::DynamicImage::ImageRgba8(alpha),
            image::ImageFormat::Png,
        );
        let (smaller, mime) = reduce(&png, "image/png").unwrap();
        assert_eq!(mime, "image/png");
        let decoded = image::load_from_memory(&smaller).unwrap();
        assert!(decoded.color().has_alpha());
        assert_eq!(decoded.to_rgba8().get_pixel(0, 0).0[3], 0, "alpha kept");
        // A diagram in two colors.
        let diagram =
            image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(2400, 1200, |x, _| {
                if (x / 40) % 2 == 0 {
                    image::Rgb([0, 0, 0])
                } else {
                    image::Rgb([255, 255, 255])
                }
            }));
        let bmp = encoded(diagram, image::ImageFormat::Bmp);
        let (smaller, mime) = reduce(&bmp, "image/bmp").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(size(&smaller), (1080, 540));
    }

    #[test]
    fn a_result_that_is_not_smaller_keeps_the_original() {
        // A tiny, heavily compressed one-color PNG grows when scaled and re-encoded.
        let flat = encoded(
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                4000,
                10,
                image::Rgb([7, 7, 7]),
            )),
            image::ImageFormat::Png,
        );
        assert!(reduce(&flat, "image/png").is_none_or(|(out, _)| out.len() < flat.len()));
        let jpeg = encoded(photo(1200, 30), image::ImageFormat::Jpeg);
        if let Some((out, _)) = reduce(&jpeg, "image/jpeg") {
            assert!(out.len() < jpeg.len());
        }
    }

    #[test]
    fn animated_gifs_and_garbage_keep_their_bytes() {
        use image::codecs::gif::GifEncoder;
        let mut gif = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut gif);
            for shade in [10u8, 200] {
                let frame = image::RgbaImage::from_pixel(1500, 20, image::Rgba([shade, 0, 0, 255]));
                encoder.encode_frame(image::Frame::new(frame)).unwrap();
            }
        }
        assert!(animated_gif(&gif));
        assert!(reduce(&gif, "image/gif").is_none());
        assert!(reduce(b"not an image at all", "image/jpeg").is_none());
        // Only the start of a JPEG: its size is there, its pixels are not.
        let jpeg = encoded(photo(3000, 2000), image::ImageFormat::Jpeg);
        assert!(reduce(&jpeg[..200], "image/jpeg").is_none());
    }

    /// `jpeg` with an APP1 Exif segment after its SOI marker holding `orientation`.
    fn with_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8, 0xff, 0xe1, 0x00, 0x22];
        out.extend_from_slice(b"Exif\0\0");
        // Little-endian TIFF header, IFD at 8: one entry, Orientation (0x0112), SHORT.
        out.extend_from_slice(&[0x49, 0x49, 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x01, 0x00, 0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[orientation, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn a_turned_photo_comes_out_upright() {
        // Stored landscape, shown portrait (Orientation 6: turn 90° clockwise).
        let turned = with_orientation(&encoded(photo(3000, 2000), image::ImageFormat::Jpeg), 6);
        let (smaller, _) = reduce(&turned, "image/jpeg").unwrap();
        assert_eq!(size(&smaller), (1080, 1620));
        // Shown 1000 px wide: narrow enough, kept with its orientation.
        let narrow = with_orientation(&encoded(photo(1500, 1000), image::ImageFormat::Jpeg), 6);
        assert!(reduce(&narrow, "image/jpeg").is_none());
        // The covers' thumbnails turn too.
        let thumb = crate::hub_client::requests::thumbnail(&turned).unwrap();
        let decoded = image::load_from_memory(&thumb).unwrap();
        assert!(decoded.height() > decoded.width(), "upright");
    }

    #[test]
    fn exif_is_dropped_from_a_re_encoded_jpeg() {
        let jpeg = encoded(photo(3000, 2000), image::ImageFormat::Jpeg);
        // An APP1 Exif segment after the SOI marker.
        let mut exif = vec![0xff, 0xd8, 0xff, 0xe1, 0x00, 0x10];
        exif.extend_from_slice(b"Exif\0\0GPS-here");
        exif.extend_from_slice(&jpeg[2..]);
        let (smaller, _) = reduce(&exif, "image/jpeg").unwrap();
        assert!(!smaller.windows(4).any(|w| w == b"Exif"));
    }
}
