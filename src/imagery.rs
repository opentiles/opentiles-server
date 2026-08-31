//! Imagery handling: JPEG pass-through, PNG → JPEG re-encode, and deriving a
//! tile's imagery from an ancestor by crop-and-upscale when the provider
//! has nothing at the requested zoom.

use crate::{Error, Result};
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageFormat};
use std::io::Cursor;

/// Output texture edge in texels (matches the providers' tiles).
pub const TEXTURE_TEXELS: u32 = 256;

/// Turn provider bytes into the JPEG embedded in the GLB. JPEG passes
/// through untouched (deterministic, lossless); anything else is decoded and
/// encoded at `quality`.
pub fn to_jpeg(bytes: &[u8], quality: u8, what: &str) -> Result<Vec<u8>> {
    let format = sniff(bytes, what)?;
    if format == ImageFormat::Jpeg {
        return Ok(bytes.to_vec());
    }
    let img = decode(bytes, format, what)?;
    encode_jpeg(&img, quality, what)
}

/// Imagery for a tile `dz` levels below `ancestor_bytes`' tile, at window
/// offset `(qx, qy)`: crop the `256 >> dz` texel square the tile covers and
/// upscale it to 256×256 (bilinear — the ancestor is already the best data
/// there is; sharper filters only invent edges). `dz == 0` is [`to_jpeg`].
pub fn derive_from_ancestor(
    ancestor_bytes: &[u8],
    dz: u8,
    qx: u32,
    qy: u32,
    quality: u8,
    what: &str,
) -> Result<Vec<u8>> {
    if dz == 0 {
        return to_jpeg(ancestor_bytes, quality, what);
    }
    let format = sniff(ancestor_bytes, what)?;
    let img = decode(ancestor_bytes, format, what)?;
    let (w, h) = img.dimensions();
    let n = 1u32 << dz;
    if w < n || h < n {
        return Err(Error::Decode {
            what: what.into(),
            reason: format!("ancestor {w}×{h} too small to split into {n}×{n} children"),
        });
    }
    let (cw, ch) = (w / n, h / n);
    let cropped = img.crop_imm(qx * cw, qy * ch, cw, ch);
    let up = cropped.resize_exact(TEXTURE_TEXELS, TEXTURE_TEXELS, FilterType::Triangle);
    encode_jpeg(&up, quality, what)
}

/// Detect the format from the magic bytes — never from a file name: the
/// cache stores imagery under `.png` even when the provider sent JPEG.
fn sniff(bytes: &[u8], what: &str) -> Result<ImageFormat> {
    image::guess_format(bytes).map_err(|e| Error::Decode {
        what: what.into(),
        reason: e.to_string(),
    })
}

/// Decode `bytes` as `format`, tagging failures with `what` for the error.
fn decode(bytes: &[u8], format: ImageFormat, what: &str) -> Result<DynamicImage> {
    image::load_from_memory_with_format(bytes, format).map_err(|e| Error::Decode {
        what: what.into(),
        reason: e.to_string(),
    })
}

/// Encode as baseline RGB JPEG at `quality` (any alpha channel is dropped —
/// terrain imagery has no meaningful transparency).
fn encode_jpeg(img: &DynamicImage, quality: u8, what: &str) -> Result<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), quality)
        .encode_image(&rgb)
        .map_err(|e| Error::Decode {
            what: format!("{what} (jpeg encode)"),
            reason: e.to_string(),
        })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, RgbImage};

    fn png(img: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(Cursor::new(&mut out))
            .write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        out
    }

    #[test]
    fn jpeg_passes_through_png_is_reencoded() {
        let img = RgbImage::from_fn(256, 256, |x, _| image::Rgb([x as u8, 0, 0]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), 80)
            .encode_image(&img)
            .unwrap();
        assert_eq!(to_jpeg(&jpeg, 90, "t").unwrap(), jpeg);
        let from_png = to_jpeg(&png(&img), 90, "t").unwrap();
        assert_eq!(image::guess_format(&from_png).unwrap(), ImageFormat::Jpeg);
    }

    #[test]
    fn crop_of_a_checker_is_solid() {
        // 2×2 quadrants: NW red, NE green, SW blue, SE white
        let img = RgbImage::from_fn(256, 256, |x, y| match (x < 128, y < 128) {
            (true, true) => image::Rgb([255, 0, 0]),
            (false, true) => image::Rgb([0, 255, 0]),
            (true, false) => image::Rgb([0, 0, 255]),
            (false, false) => image::Rgb([255, 255, 255]),
        });
        let bytes = png(&img);
        let expect = |qx, qy, rgb: [u8; 3]| {
            let out = derive_from_ancestor(&bytes, 1, qx, qy, 95, "t").unwrap();
            let dec = image::load_from_memory(&out).unwrap().to_rgb8();
            assert_eq!(dec.dimensions(), (256, 256));
            let p = dec.get_pixel(128, 128).0;
            for c in 0..3 {
                assert!(
                    (i32::from(p[c]) - i32::from(rgb[c])).abs() < 8,
                    "q({qx},{qy}) {p:?} vs {rgb:?}"
                );
            }
        };
        expect(0, 0, [255, 0, 0]);
        expect(1, 0, [0, 255, 0]);
        expect(0, 1, [0, 0, 255]);
        expect(1, 1, [255, 255, 255]);
        // dz=0 is a plain pass-through/re-encode
        let same = derive_from_ancestor(&bytes, 0, 0, 0, 90, "t").unwrap();
        assert_eq!(image::guess_format(&same).unwrap(), ImageFormat::Jpeg);
    }

    #[test]
    fn deep_windows_still_produce_a_texture() {
        let img = RgbImage::from_fn(256, 256, |x, y| image::Rgb([x as u8, y as u8, 7]));
        let out = derive_from_ancestor(&png(&img), 8, 255, 3, 90, "t").unwrap(); // 1-texel window
        let dec = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!(dec.dimensions(), (256, 256));
        assert!(derive_from_ancestor(&png(&img), 9, 0, 0, 90, "t").is_err()); // 512 > 256
    }
}
