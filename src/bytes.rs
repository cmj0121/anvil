//! Byte helpers every module shares: lowercase hex, random bytes from
//! `/dev/urandom` and a read that stops one byte past a limit.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};

/// `bytes` in lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Fills `buf` from `/dev/urandom` in place, so a `Zeroizing` buffer leaves
/// no copy; there is no weaker fallback.
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    File::open("/dev/urandom")?.read_exact(buf)
}

/// What is wrong with more than `limit` bytes.
pub fn too_large(limit: u64) -> String {
    format!("larger than the {limit} byte limit")
}

/// Why [`read_capped`] failed: the reader held more than the limit, or the
/// read itself failed (an `EFBIG` from the read is this, not `Over`).
#[derive(Debug)]
pub enum CappedError {
    Over { limit: u64 },
    Io(io::Error),
}

impl fmt::Display for CappedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CappedError::Over { limit } => f.write_str(&too_large(*limit)),
            CappedError::Io(err) => err.fmt(f),
        }
    }
}

/// Appends at most `limit + 1` bytes of `reader` to `bytes`, and is
/// [`CappedError::Over`] when that is more than `limit`. Reads straight into
/// `bytes`, so a `Zeroizing<Vec<u8>>` passed as `&mut *buf` keeps the only
/// copy; reserve `limit + 1` past its length first so it is never
/// reallocated.
pub fn read_capped(reader: impl Read, limit: u64, bytes: &mut Vec<u8>) -> Result<(), CappedError> {
    let read = reader
        .take(limit + 1)
        .read_to_end(bytes)
        .map_err(CappedError::Io)?;
    if read as u64 > limit {
        return Err(CappedError::Over { limit });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroizing;

    use super::*;

    #[test]
    fn hex_is_lowercase_and_two_digits_a_byte() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff, 0x5c]), "000aff5c");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn random_fills_the_whole_buffer_and_differs_each_call() {
        let (mut one, mut two) = ([0u8; 32], [0u8; 32]);
        fill_random(&mut one).unwrap();
        fill_random(&mut two).unwrap();
        assert_ne!(one, [0u8; 32]);
        assert_ne!(one, two);
    }

    #[test]
    fn a_read_at_the_limit_passes_and_one_past_it_is_too_large() {
        let mut bytes = b"head:".to_vec();
        read_capped(&[b'x'; 4][..], 4, &mut bytes).unwrap();
        assert_eq!(bytes, b"head:xxxx");

        let mut bytes = Vec::new();
        let err = read_capped(io::repeat(b'x'), 4, &mut bytes).unwrap_err();
        assert!(matches!(err, CappedError::Over { limit: 4 }), "{err:?}");
        assert_eq!(err.to_string(), "larger than the 4 byte limit");
        assert_eq!(bytes, b"xxxxx", "stops one byte past the limit");
    }

    #[test]
    fn an_efbig_from_the_read_is_a_read_error_not_over_the_limit() {
        struct Efbig;
        impl Read for Efbig {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "File too large",
                ))
            }
        }
        let err = read_capped(Efbig, 4, &mut Vec::new()).unwrap_err();
        assert!(
            matches!(&err, CappedError::Io(io) if io.kind() == io::ErrorKind::FileTooLarge),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "File too large");
    }

    #[test]
    fn a_zeroizing_buffer_is_read_into_without_reallocating() {
        let mut bytes = Zeroizing::new(Vec::with_capacity(9));
        let start = bytes.as_ptr();
        read_capped(&b"secret"[..], 8, &mut bytes).unwrap();
        assert_eq!(&bytes[..], b"secret");
        assert_eq!(bytes.as_ptr(), start);
    }
}
