//! Images: decoded in this process with the `image` crate, whose decoders
//! are written in Rust, shrunk, and encoded again as JPEG or PNG.
//! See `docs/Media.md#images`.

use std::io::Cursor;

use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits,
    codecs::{gif::GifDecoder, jpeg::JpegEncoder, png::PngEncoder, webp::WebPDecoder},
    imageops::FilterType,
};

/// The longer side of an image the model sees.
pub const MAX_SIDE: u32 = 2048;
/// The longer side of a video frame.
pub const FRAME_SIDE: u32 = 1024;
/// The most frames taken from a video or an animation.
pub const MAX_FRAMES: usize = 8;
/// Images larger than this are refused.
const MAX_PIXELS: u64 = 50_000_000;
/// The most memory one decode may use.
const MAX_ALLOC: u64 = 512 * 1024 * 1024;
/// The most frames an animation may have.
const MAX_ANIMATION_FRAMES: usize = 5_000;

/// An image ready for the media table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The format of image bytes, if they are one Alyosha decodes.
pub fn format(bytes: &[u8]) -> Option<ImageFormat> {
    image::guess_format(bytes)
        .ok()
        .filter(|format| {
            matches!(
                format,
                ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP
            )
        })
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_ALLOC);
    limits
}

fn check_size(width: u32, height: u32) -> Result<(), String> {
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "the image is {width}×{height}, larger than 50 megapixels"
        ));
    }
    Ok(())
}

/// Decodes a still image, applying its EXIF orientation.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("could not read the image: {error}"))?;
    reader.limits(limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("could not read the image: {error}"))?;
    let (width, height) = decoder.dimensions();
    check_size(width, height)?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("could not read the image: {error}"))?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Ok(image)
}

/// Shrinks an image so its longer side is at most `max_side`, and encodes it
/// as PNG if it has transparency, otherwise as JPEG at quality 85.
pub fn encode(image: DynamicImage, max_side: u32) -> Result<Encoded, String> {
    let image = if image.width() > max_side || image.height() > max_side {
        image.resize(max_side, max_side, FilterType::Triangle)
    } else {
        image
    };
    let transparent = image.color().has_alpha()
        && image.to_rgba8().pixels().any(|pixel| pixel.0[3] < 255);
    let mut bytes = Vec::new();
    let mime = if transparent {
        image
            .write_with_encoder(PngEncoder::new(&mut bytes))
            .map_err(|error| format!("could not encode the image: {error}"))?;
        "image/png"
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, 85))
            .map_err(|error| format!("could not encode the image: {error}"))?;
        "image/jpeg"
    };
    Ok(Encoded {
        mime,
        bytes,
        width: image.width(),
        height: image.height(),
    })
}

/// Decodes and encodes a still image.
pub fn normalize(bytes: &[u8], max_side: u32) -> Result<Encoded, String> {
    encode(decode(bytes)?, max_side)
}

/// The frames of an animated GIF or WebP, up to `MAX_FRAMES` spread evenly
/// over it, or `None` for a still image.
pub fn animation_frames(bytes: &[u8]) -> Result<Option<Vec<DynamicImage>>, String> {
    let read = |error: image::ImageError| format!("could not read the animation: {error}");
    let frames: Box<dyn Fn() -> Result<image::Frames<'static>, String>> = match format(bytes) {
        Some(ImageFormat::Gif) => {
            let owned = bytes.to_vec();
            Box::new(move || {
                let mut decoder = GifDecoder::new(Cursor::new(owned.clone())).map_err(read)?;
                let (width, height) = decoder.dimensions();
                check_size(width, height)?;
                decoder.set_limits(limits()).map_err(read)?;
                Ok(decoder.into_frames())
            })
        }
        Some(ImageFormat::WebP) => {
            let decoder = WebPDecoder::new(Cursor::new(bytes)).map_err(read)?;
            if !decoder.has_animation() {
                return Ok(None);
            }
            let owned = bytes.to_vec();
            Box::new(move || {
                let decoder = WebPDecoder::new(Cursor::new(owned.clone())).map_err(read)?;
                let (width, height) = decoder.dimensions();
                check_size(width, height)?;
                Ok(decoder.into_frames())
            })
        }
        _ => return Ok(None),
    };
    // Count first, then decode again keeping only the chosen frames, so a
    // long animation is never held whole.
    let mut count = 0;
    for frame in frames()? {
        frame.map_err(read)?;
        count += 1;
        if count > MAX_ANIMATION_FRAMES {
            return Err(format!(
                "the animation has more than {MAX_ANIMATION_FRAMES} frames"
            ));
        }
    }
    if count <= 1 {
        return Ok(None);
    }
    let chosen = spread(count, MAX_FRAMES);
    let mut kept = Vec::new();
    for (index, frame) in frames()?.enumerate() {
        if chosen.contains(&index) {
            kept.push(DynamicImage::ImageRgba8(frame.map_err(read)?.into_buffer()));
        }
    }
    Ok(Some(kept))
}

/// Up to `most` indices spread evenly over `count` items, each in the middle
/// of its share.
pub fn spread(count: usize, most: usize) -> Vec<usize> {
    let taken = count.min(most);
    (0..taken)
        .map(|i| ((2 * i + 1) * count) / (2 * taken))
        .collect()
}

#[cfg(test)]
mod tests {
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    use super::*;

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([200, 10, 10])))
            .write_with_encoder(JpegEncoder::new(&mut bytes))
            .unwrap();
        bytes
    }

    #[test]
    fn large_images_shrink_to_the_longer_side() {
        let encoded = normalize(&jpeg(4000, 3000), MAX_SIDE).unwrap();
        assert_eq!((encoded.width, encoded.height), (2048, 1536));
        assert_eq!(encoded.mime, "image/jpeg");
        let small = normalize(&jpeg(30, 20), MAX_SIDE).unwrap();
        assert_eq!((small.width, small.height), (30, 20));
    }

    #[test]
    fn transparency_keeps_png() {
        let mut image = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255]));
        image.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_with_encoder(PngEncoder::new(&mut bytes))
            .unwrap();
        assert_eq!(normalize(&bytes, MAX_SIDE).unwrap().mime, "image/png");
        // An alpha channel that is opaque everywhere becomes JPEG.
        let mut opaque = Vec::new();
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 4, Rgba([1, 2, 3, 255])))
            .write_with_encoder(PngEncoder::new(&mut opaque))
            .unwrap();
        assert_eq!(normalize(&opaque, MAX_SIDE).unwrap().mime, "image/jpeg");
    }

    #[test]
    fn huge_images_are_refused() {
        // A PNG header claiming 10000×6000 pixels.
        let mut bytes = Vec::new();
        DynamicImage::ImageLuma8(image::GrayImage::new(1, 1))
            .write_with_encoder(PngEncoder::new(&mut bytes))
            .unwrap();
        bytes[16..20].copy_from_slice(&10_000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&6_000u32.to_be_bytes());
        // The header chunk's CRC covers its type and data.
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        let error = normalize(&bytes, MAX_SIDE).unwrap_err();
        assert!(error.contains("50 megapixels"), "{error}");
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn spread_takes_the_middle_of_each_share() {
        assert_eq!(spread(3, 8), vec![0, 1, 2]);
        assert_eq!(spread(16, 8), vec![1, 3, 5, 7, 9, 11, 13, 15]);
        assert_eq!(spread(1, 8), vec![0]);
    }

    #[test]
    fn animated_gifs_give_frames_and_stills_none() {
        use image::{Delay, Frame, codecs::gif::GifEncoder};
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for shade in [0u8, 80, 160] {
                encoder
                    .encode_frame(Frame::from_parts(
                        RgbaImage::from_pixel(6, 4, Rgba([shade, shade, shade, 255])),
                        0,
                        0,
                        Delay::from_numer_denom_ms(100, 1),
                    ))
                    .unwrap();
            }
        }
        let frames = animation_frames(&bytes).unwrap().unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(animation_frames(&jpeg(4, 4)).unwrap(), None);
    }
}
