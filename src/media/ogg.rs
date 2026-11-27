//! The length of an Ogg Opus recording, such as a Telegram voice message,
//! read from its pages without decoding it.

/// The recording's length in seconds, if the bytes are Ogg Opus.
pub fn opus_duration(bytes: &[u8]) -> Option<f64> {
    if !is_ogg_opus(bytes) {
        return None;
    }
    // The ID header packet starts in the first page's data, after its
    // segment table; its pre-skip is at bytes 10..12.
    let segments = usize::from(*bytes.get(26)?);
    let head = bytes.get(27 + segments..)?;
    let pre_skip = u16::from_le_bytes([*head.get(10)?, *head.get(11)?]);
    // The last page's granule position counts 48 kHz samples.
    let last = bytes.windows(4).rposition(|window| window == b"OggS")?;
    let granule = i64::from_le_bytes(bytes.get(last + 6..last + 14)?.try_into().ok()?);
    let samples = granule - i64::from(pre_skip);
    (samples >= 0).then(|| samples as f64 / 48_000.0)
}

/// Whether the bytes start an Ogg stream whose first packet is an Opus header.
pub fn is_ogg_opus(bytes: &[u8]) -> bool {
    bytes.starts_with(b"OggS")
        && bytes
            .get(26)
            .and_then(|segments| bytes.get(27 + usize::from(*segments)..))
            .is_some_and(|head| head.starts_with(b"OpusHead"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal two-page Ogg Opus stream whose last granule is `granule`.
    pub(crate) fn sample(granule: i64) -> Vec<u8> {
        let page = |granule: i64, body: &[u8]| {
            let mut page = b"OggS".to_vec();
            page.push(0);
            page.push(0);
            page.extend_from_slice(&granule.to_le_bytes());
            page.extend_from_slice(&[0; 12]);
            page.push(1);
            page.push(body.len() as u8);
            page.extend_from_slice(body);
            page
        };
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 1]);
        head.extend_from_slice(&312u16.to_le_bytes());
        head.extend_from_slice(&[0; 7]);
        let mut bytes = page(0, &head);
        bytes.extend(page(granule, &[1, 2, 3]));
        bytes
    }

    #[test]
    fn length_comes_from_the_last_granule() {
        let bytes = sample(48_000 * 12 + 312);
        assert!(is_ogg_opus(&bytes));
        assert_eq!(opus_duration(&bytes), Some(12.0));
        assert_eq!(opus_duration(b"not ogg"), None);
    }
}
