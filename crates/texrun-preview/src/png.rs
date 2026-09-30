//! Minimal PNG header check.

use std::fs::File;
use std::io::Read;
use std::path::Path;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// Width and height from the `IHDR` chunk, or `None` if `bytes` does not start
/// like a PNG file.
pub(crate) fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = bytes.get(..24)?;
    if header[..8] != SIGNATURE || &header[12..16] != b"IHDR" {
        return None;
    }
    let be = |at: usize| u32::from_be_bytes(header[at..at + 4].try_into().expect("4 bytes"));
    let (w, h) = (be(16), be(20));
    (w > 0 && h > 0).then_some((w, h))
}

/// [`dimensions`] of the file at `path`.
pub(crate) fn file_dimensions(path: &Path) -> Option<(u32, u32)> {
    let mut header = [0u8; 24];
    File::open(path).ok()?.read_exact(&mut header).ok()?;
    dimensions(&header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ihdr() {
        let mut png = SIGNATURE.to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&600u32.to_be_bytes());
        png.extend_from_slice(&800u32.to_be_bytes());
        assert_eq!(dimensions(&png), Some((600, 800)));
        assert_eq!(dimensions(&png[..20]), None);
        let mut not_png = png.clone();
        not_png[1] = b'X';
        assert_eq!(dimensions(&not_png), None);
        let mut zero = png;
        zero[16..20].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(dimensions(&zero), None);
    }
}
