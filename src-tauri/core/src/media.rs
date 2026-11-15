//! Images attached to posts: importing them into the app's media folder and
//! preparing them for each network's size and format limits.
//!
//! Photos are always cleaned of camera and location metadata before upload:
//! JPEG metadata segments are dropped without re-encoding, and images that
//! need rotating, resizing or converting are re-encoded.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, imageops::FilterType};

use crate::error::{AppError, AppResult, PResult, ProviderError};
use crate::model::MediaAttachment;

/// Largest file accepted into the composer.
pub const MAX_IMPORT_BYTES: u64 = 50 * 1024 * 1024;

pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

fn format_for(mime: &str) -> Option<ImageFormat> {
    match mime {
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/png" => Some(ImageFormat::Png),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::WebP),
        _ => None,
    }
}

/// Copies an image file into the media folder so a scheduled post keeps
/// working even if the original is moved or deleted.
pub fn import_file(media_dir: &Path, src: &Path) -> AppResult<MediaAttachment> {
    let meta = std::fs::metadata(src).map_err(|e| AppError::Io(format!("Cannot read {}: {e}", src.display())))?;
    if meta.len() > MAX_IMPORT_BYTES {
        return Err(AppError::invalid(format!(
            "{} is larger than 50 MB.",
            src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
        )));
    }
    let bytes = std::fs::read(src).map_err(|e| AppError::Io(format!("Cannot read {}: {e}", src.display())))?;
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".into());
    import_bytes(media_dir, &bytes, &name)
}

pub fn import_bytes(media_dir: &Path, bytes: &[u8], name: &str) -> AppResult<MediaAttachment> {
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        return Err(AppError::invalid(format!("{name} is larger than 50 MB.")));
    }
    let mime = sniff_mime(bytes).ok_or_else(|| {
        AppError::invalid(format!("{name} is not a supported image. Use JPEG, PNG, GIF or WebP."))
    })?;
    let (width, height) = oriented_dimensions(bytes, mime)
        .map_err(|e| AppError::invalid(format!("{name} could not be read as an image: {e}")))?;
    std::fs::create_dir_all(media_dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    let path = media_dir.join(format!("{id}.{}", extension_for(mime)));
    std::fs::write(&path, bytes)?;
    Ok(MediaAttachment {
        id,
        path: path.to_string_lossy().to_string(),
        file_name: name.to_string(),
        mime: mime.to_string(),
        size: bytes.len() as u64,
        width,
        height,
        alt: String::new(),
    })
}

/// Dimensions as displayed, after applying the EXIF orientation.
fn oriented_dimensions(bytes: &[u8], mime: &str) -> Result<(u32, u32), String> {
    let format = format_for(mime).ok_or("unsupported format")?;
    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader.set_format(format);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    Ok(if swaps_axes(orientation) { (h, w) } else { (w, h) })
}

fn swaps_axes(o: image::metadata::Orientation) -> bool {
    use image::metadata::Orientation::*;
    matches!(o, Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH)
}

/// What a network accepts for an image.
#[derive(Clone, Copy, Debug)]
pub struct ImageLimits {
    pub max_bytes: u64,
    /// Longest side in pixels; 0 means no limit.
    pub max_dimension: u32,
    pub allowed: &'static [&'static str],
}

pub const COMMON_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];
pub const NO_WEBP: &[&str] = &["image/jpeg", "image/png", "image/gif"];
pub const JPEG_PNG: &[&str] = &["image/jpeg", "image/png"];

/// An image ready to upload.
#[derive(Clone, Debug)]
pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub file_name: String,
}

/// Loads an attachment and makes it fit `limits`: metadata stripped,
/// orientation applied, converted and scaled down when needed.
pub fn prepare(att: &MediaAttachment, limits: &ImageLimits, network: &str) -> PResult<PreparedImage> {
    let bytes = std::fs::read(&att.path).map_err(|e| {
        ProviderError::invalid(format!("The image {} is missing from the media folder: {e}", att.file_name))
    })?;
    prepare_bytes(bytes, &att.file_name, limits, network)
}

pub fn prepare_bytes(bytes: Vec<u8>, file_name: &str, limits: &ImageLimits, network: &str) -> PResult<PreparedImage> {
    let mime = sniff_mime(&bytes).ok_or_else(|| ProviderError::invalid(format!("{file_name} is not a supported image.")))?;
    let format = format_for(mime).unwrap();

    let mut reader = ImageReader::new(Cursor::new(&bytes[..]));
    reader.set_format(format);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| ProviderError::invalid(format!("{file_name} could not be read: {e}")))?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let needs_rotation = orientation != image::metadata::Orientation::NoTransforms;
    let too_large_dims = limits.max_dimension > 0 && w.max(h) > limits.max_dimension;
    let allowed = limits.allowed.contains(&mime);

    // Fast path: keep the original pixels.
    if allowed && !needs_rotation && !too_large_dims {
        let out = if mime == "image/jpeg" { strip_jpeg_metadata(&bytes) } else { bytes.clone() };
        if out.len() as u64 <= limits.max_bytes {
            return Ok(PreparedImage {
                bytes: out,
                mime: mime.to_string(),
                width: w,
                height: h,
                file_name: file_name.to_string(),
            });
        }
    }

    if mime == "image/gif" && allowed {
        // Re-encoding would lose the animation.
        return Err(ProviderError::invalid(format!(
            "{file_name} is {} but {network} accepts GIFs up to {}.",
            human_size(bytes.len() as u64),
            human_size(limits.max_bytes)
        )));
    }

    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|e| ProviderError::invalid(format!("{file_name} could not be decoded: {e}")))?;
    img.apply_orientation(orientation);

    let keep_png = mime == "image/png" && limits.allowed.contains(&"image/png") && img.color().has_alpha();
    let mut max_side = if limits.max_dimension > 0 {
        limits.max_dimension.min(img.width().max(img.height()))
    } else {
        img.width().max(img.height())
    };
    let base_name = Path::new(file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".into());

    for _ in 0..10 {
        let scaled = if max_side < img.width().max(img.height()) {
            img.resize(max_side, max_side, FilterType::Lanczos3)
        } else {
            img.clone()
        };
        if keep_png {
            let mut buf = Vec::new();
            scaled
                .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
                .map_err(|e| ProviderError::other(format!("Could not encode {file_name}: {e}")))?;
            if buf.len() as u64 <= limits.max_bytes {
                let (width, height) = scaled.dimensions();
                return Ok(PreparedImage {
                    bytes: buf,
                    mime: "image/png".into(),
                    width,
                    height,
                    file_name: format!("{base_name}.png"),
                });
            }
        }
        let rgb = flatten_to_rgb(&scaled);
        for quality in [90u8, 82, 74] {
            let mut buf = Vec::new();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
            rgb.write_with_encoder(encoder)
                .map_err(|e| ProviderError::other(format!("Could not encode {file_name}: {e}")))?;
            if buf.len() as u64 <= limits.max_bytes {
                let (width, height) = rgb.dimensions();
                return Ok(PreparedImage {
                    bytes: buf,
                    mime: "image/jpeg".into(),
                    width,
                    height,
                    file_name: format!("{base_name}.jpg"),
                });
            }
        }
        max_side = ((max_side as f32) * 0.8) as u32;
        if max_side < 320 {
            break;
        }
    }
    Err(ProviderError::invalid(format!(
        "{file_name} could not be made small enough for {network} (limit {}).",
        human_size(limits.max_bytes)
    )))
}

/// Puts transparent pixels on white so they do not turn black in JPEG.
fn flatten_to_rgb(img: &DynamicImage) -> DynamicImage {
    if !img.color().has_alpha() {
        return DynamicImage::ImageRgb8(img.to_rgb8());
    }
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut rgb = image::RgbImage::new(w, h);
    for (x, y, p) in rgba.enumerate_pixels() {
        let a = p[3] as u32;
        let blend = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
        rgb.put_pixel(x, y, image::Rgb([blend(p[0]), blend(p[1]), blend(p[2])]));
    }
    DynamicImage::ImageRgb8(rgb)
}

/// Removes EXIF, XMP and IPTC segments from a JPEG without touching the
/// image data. Returns the input unchanged if it cannot be parsed.
pub fn strip_jpeg_metadata(bytes: &[u8]) -> Vec<u8> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..2]);
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return bytes.to_vec();
        }
        let marker = bytes[i + 1];
        if marker == 0xFF {
            // Fill byte.
            i += 1;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            // Start of scan (or end): the rest is image data.
            out.extend_from_slice(&bytes[i..]);
            return out;
        }
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            out.extend_from_slice(&bytes[i..i + 2]);
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if len < 2 || i + 2 + len > bytes.len() {
            return bytes.to_vec();
        }
        let segment = &bytes[i..i + 2 + len];
        let payload = &segment[4..];
        let drop = match marker {
            0xE1 => payload.starts_with(b"Exif\0") || payload.starts_with(b"http://ns.adobe.com/"),
            0xED => true, // Photoshop / IPTC
            0xFE => true, // comments
            _ => false,
        };
        if !drop {
            out.extend_from_slice(segment);
        }
        i += 2 + len;
    }
    bytes.to_vec()
}

pub fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// Deletes files in the media folder that no post refers to any more and
/// that are older than a day (abandoned composer attachments).
pub fn cleanup_unreferenced(media_dir: &Path, referenced: &std::collections::HashSet<String>) -> usize {
    let Ok(entries) = std::fs::read_dir(media_dir) else { return 0 };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    let mut removed = 0;
    for entry in entries.flatten() {
        let path: PathBuf = entry.path();
        let key = path.to_string_lossy().to_string();
        if referenced.contains(&key) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(false);
        if old && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let img = if alpha {
            DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| {
                image::Rgba([(x % 255) as u8, (y % 255) as u8, 128, if x < w / 2 { 0 } else { 255 }])
            }))
        } else {
            // Noise-like content so it does not compress to nothing.
            DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
                let v = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) as u8;
                image::Rgb([v, v.wrapping_mul(3), v.wrapping_add(x as u8)])
            }))
        };
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png).unwrap();
        buf
    }

    #[test]
    fn imports_and_sniffs() {
        let dir = tempfile::tempdir().unwrap();
        let png = sample_png(40, 30, false);
        let att = import_bytes(dir.path(), &png, "shot.png").unwrap();
        assert_eq!(att.mime, "image/png");
        assert_eq!((att.width, att.height), (40, 30));
        assert!(Path::new(&att.path).exists());
        assert!(import_bytes(dir.path(), b"not an image", "x.txt").is_err());
    }

    #[test]
    fn keeps_small_images_untouched() {
        let png = sample_png(64, 64, false);
        let limits = ImageLimits { max_bytes: 10_000_000, max_dimension: 0, allowed: COMMON_TYPES };
        let out = prepare_bytes(png.clone(), "a.png", &limits, "Test").unwrap();
        assert_eq!(out.bytes, png);
        assert_eq!(out.mime, "image/png");
    }

    #[test]
    fn shrinks_large_images_under_limit() {
        let png = sample_png(1800, 1200, false);
        assert!(png.len() > 300_000, "sample should be big, was {}", png.len());
        let limits = ImageLimits { max_bytes: 300_000, max_dimension: 1000, allowed: COMMON_TYPES };
        let out = prepare_bytes(png, "big.png", &limits, "Test").unwrap();
        assert!(out.bytes.len() <= 300_000);
        assert!(out.width.max(out.height) <= 1000);
        assert_eq!(out.mime, "image/jpeg");
        assert_eq!(out.file_name, "big.jpg");
        assert_eq!(sniff_mime(&out.bytes), Some("image/jpeg"));
    }

    #[test]
    fn converts_disallowed_types() {
        let png = sample_png(50, 50, true);
        let limits = ImageLimits { max_bytes: 5_000_000, max_dimension: 0, allowed: &["image/jpeg"] };
        let out = prepare_bytes(png, "t.png", &limits, "Test").unwrap();
        assert_eq!(out.mime, "image/jpeg");
        let img = image::load_from_memory(&out.bytes).unwrap().to_rgb8();
        // The transparent half was flattened onto white, not black.
        let p = img.get_pixel(2, 25);
        assert!(p[0] > 200 && p[1] > 200 && p[2] > 200, "{p:?}");
    }

    #[test]
    fn strips_exif_segment() {
        // SOI, APP1 Exif segment, a DQT-like segment, SOS + data, EOI.
        let mut jpeg = vec![0xFF, 0xD8];
        let exif_payload = b"Exif\0\0GPSDATA";
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&((exif_payload.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(exif_payload);
        jpeg.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x04, 0x01, 0x02]);
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x11, 0x22, 0xFF, 0xD9]);
        let out = strip_jpeg_metadata(&jpeg);
        assert!(!out.windows(4).any(|w| w == b"Exif"));
        assert_eq!(out, vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0x01, 0x02, 0xFF, 0xDA, 0x00, 0x02, 0x11, 0x22, 0xFF, 0xD9]);
    }
}
