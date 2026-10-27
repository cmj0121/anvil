//! Check a downloaded release tarball before it is extracted.
//!
//! The checks run in order: the file is at most 64 MiB, its sha256sum line
//! matches the bytes, a trusted key's minisign signature verifies those bytes,
//! and the trusted comment is exactly the caller's `anvil <tag> <triple>`.
//! One 64 KiB read feeds both the checksum and `verify_stream`, which refuses
//! a legacy signature.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use minisign_verify::{PublicKey, Signature, StreamVerifier};
use sha2::{Digest, Sha256};

use crate::secret::shown_name;

/// Largest tarball [`verify`] reads. D4's download cap.
const MAX_BYTES: u64 = 64 << 20;
/// One read, in the chunks D5 names, feeds the checksum and the signature.
const CHUNK: usize = 64 << 10;

/// Why [`verify`] refused a tarball. The text is the message a caller logs.
#[derive(Debug)]
pub enum VerifyError {
    /// `NAME is N bytes, over the 64 MiB limit`.
    TooLarge { asset: String, bytes: u64 },
    /// The `.sha256` line is not `<64 hex>  <asset>`, or the digest differs.
    Checksum { asset: String },
    /// The `.minisig` is not from a trusted key, or does not verify.
    Signature {
        asset: String,
        reason: SignatureReason,
    },
    /// The signature verifies, for a different trusted comment.
    Comment { asset: String, comment: String },
    /// The tarball could not be read.
    Read { asset: String, message: String },
}

/// The `<reason>` in `signature check failed for NAME: <reason>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureReason {
    /// The signature's key id is neither trusted key.
    UnknownKey,
    /// The `.minisig` does not decode, or the signature bytes do not verify.
    BadSignature,
    /// A non-prehashed `Ed` signature. `verify_stream` refuses these.
    LegacySignature,
}

impl SignatureReason {
    fn as_str(self) -> &'static str {
        match self {
            SignatureReason::UnknownKey => "unknown key id",
            SignatureReason::BadSignature => "bad signature",
            SignatureReason::LegacySignature => "legacy signature",
        }
    }
}

impl VerifyError {
    fn checksum(asset: &str) -> Self {
        VerifyError::Checksum {
            asset: asset.to_owned(),
        }
    }

    fn signature(asset: &str, reason: SignatureReason) -> Self {
        VerifyError::Signature {
            asset: asset.to_owned(),
            reason,
        }
    }

    fn too_large(asset: &str, bytes: u64) -> Self {
        VerifyError::TooLarge {
            asset: asset.to_owned(),
            bytes,
        }
    }

    fn read(asset: &str, err: std::io::Error) -> Self {
        VerifyError::Read {
            asset: asset.to_owned(),
            message: err.to_string(),
        }
    }
}

/// An asset name and a trusted comment are chosen by whoever published the
/// release, so every message shows them through [`shown_name`].
impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::TooLarge { asset, bytes } => write!(
                f,
                "{} is {bytes} bytes, over the 64 MiB limit",
                shown_name(asset)
            ),
            VerifyError::Checksum { asset } => {
                write!(f, "checksum mismatch for {}", shown_name(asset))
            }
            VerifyError::Signature { asset, reason } => write!(
                f,
                "signature check failed for {}: {}",
                shown_name(asset),
                reason.as_str()
            ),
            VerifyError::Comment { asset, comment } => write!(
                f,
                "signature is for \"{}\", not {}",
                shown_name(comment),
                shown_name(asset)
            ),
            VerifyError::Read { asset, message } => {
                write!(f, "{}: {message}", shown_name(asset))
            }
        }
    }
}

impl std::error::Error for VerifyError {}

/// Check `tarball`'s size, then its `.sha256` text, then `minisig`, then
/// `comment`; the first failure is the error.
///
/// `keys` is the current key and, when there is one, the next. `asset` is the
/// file name the checksum line must carry and the name errors quote. `comment`
/// is the trusted comment the signature must carry, exactly
/// `anvil <tag> <triple>`.
pub fn verify(
    tarball: &Path,
    sha256_file: &str,
    minisig: &str,
    keys: &[PublicKey],
    asset: &str,
    comment: &str,
) -> Result<(), VerifyError> {
    let bytes = std::fs::metadata(tarball)
        .map_err(|err| VerifyError::read(asset, err))?
        .len();
    if bytes > MAX_BYTES {
        return Err(VerifyError::too_large(asset, bytes));
    }
    let expected = parse_sha256(sha256_file, asset).ok_or_else(|| VerifyError::checksum(asset))?;

    // The verifier is opened before the read so one pass feeds both hashes;
    // why it could not be opened waits until the checksum has been judged.
    let signature = Signature::decode(minisig);
    let mut opened = match &signature {
        Ok(signature) => open_stream(keys, signature).map(|stream| (stream, signature)),
        Err(_) => Err(SignatureReason::BadSignature),
    };
    let stream = opened.as_mut().ok().map(|(stream, _)| stream);
    if hash_file(tarball, asset, stream)? != expected {
        return Err(VerifyError::checksum(asset));
    }
    let (mut stream, signature) = opened.map_err(|reason| VerifyError::signature(asset, reason))?;
    stream
        .finalize()
        .map_err(|_| VerifyError::signature(asset, SignatureReason::BadSignature))?;
    if signature.trusted_comment() != comment {
        return Err(VerifyError::Comment {
            asset: asset.to_owned(),
            comment: signature.trusted_comment().to_owned(),
        });
    }
    Ok(())
}

/// A verifier from whichever of `keys` carries the signature's key id.
fn open_stream<'a>(
    keys: &'a [PublicKey],
    signature: &'a Signature,
) -> Result<StreamVerifier<'a>, SignatureReason> {
    for key in keys {
        match key.verify_stream(signature) {
            Ok(stream) => return Ok(stream),
            Err(minisign_verify::Error::UnexpectedKeyId) => {}
            Err(minisign_verify::Error::UnsupportedLegacyMode) => {
                return Err(SignatureReason::LegacySignature);
            }
            Err(_) => return Err(SignatureReason::BadSignature),
        }
    }
    Err(SignatureReason::UnknownKey)
}

/// SHA-256 of `path`, also fed to `stream` when a signature is ready.
/// A file that grows past [`MAX_BYTES`] while it is read is refused.
fn hash_file(
    path: &Path,
    asset: &str,
    mut stream: Option<&mut StreamVerifier<'_>>,
) -> Result<[u8; 32], VerifyError> {
    let mut file = File::open(path).map_err(|err| VerifyError::read(asset, err))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|err| VerifyError::read(asset, err))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_BYTES {
            return Err(VerifyError::too_large(asset, total));
        }
        hasher.update(&buf[..n]);
        if let Some(stream) = stream.as_mut() {
            stream.update(&buf[..n]);
        }
    }
    Ok(hasher.finalize().into())
}

/// One sha256sum line: `<64 lowercase hex>  <asset>`, an optional `\n`, and
/// nothing else (no `\r`).
fn parse_sha256(text: &str, asset: &str) -> Option<[u8; 32]> {
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.contains(['\n', '\r']) {
        return None;
    }
    let (hex, name) = line.split_once("  ")?;
    if name != asset || hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
        *byte = (from_hex(pair[0])? << 4) | from_hex(pair[1])?;
    }
    Some(out)
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::path::PathBuf;

    use super::*;
    use crate::update::keys::TrustedKeys;

    const ASSET: &str = "anvil-v9.9.9-aarch64-apple-darwin.tar.gz";
    const COMMENT: &str = "anvil v9.9.9 aarch64-apple-darwin";
    const TARBALL: &[u8] =
        include_bytes!("../../tests/fixtures/update/anvil-v9.9.9-aarch64-apple-darwin.tar.gz");
    const SHA256_FILE: &str =
        include_str!("../../tests/fixtures/update/anvil-v9.9.9-aarch64-apple-darwin.tar.gz.sha256");
    const SIG_A: &str = include_str!(
        "../../tests/fixtures/update/anvil-v9.9.9-aarch64-apple-darwin.tar.gz.minisig"
    );
    const SIG_B: &str = include_str!(
        "../../tests/fixtures/update/anvil-v9.9.9-aarch64-apple-darwin.tar.gz.b.minisig"
    );
    const SIG_C: &str = include_str!(
        "../../tests/fixtures/update/anvil-v9.9.9-aarch64-apple-darwin.tar.gz.c.minisig"
    );
    const SIG_WRONG: &str = include_str!(
        "../../tests/fixtures/update/anvil-v9.9.9-aarch64-apple-darwin.tar.gz.wrong.minisig"
    );
    const SIG_LEGACY: &str = include_str!(
        "../../tests/fixtures/update/anvil-v9.9.9-aarch64-apple-darwin.tar.gz.legacy.minisig"
    );
    const PUB_A: &str = include_str!("../../tests/fixtures/update/a.pub");
    const PUB_B: &str = include_str!("../../tests/fixtures/update/b.pub");
    const PUB_C: &str = include_str!("../../tests/fixtures/update/c.pub");

    /// A private dir holding the fixture tarball, removed on drop.
    struct Staged(PathBuf);

    impl Staged {
        fn new(tag: &str, bytes: &[u8]) -> Staged {
            let staged = Staged(crate::test_util::temp_dir(tag));
            crate::test_util::write_private(&staged.tarball(), bytes);
            staged
        }

        fn tarball(&self) -> PathBuf {
            self.0.join(ASSET)
        }

        fn verify(
            &self,
            sha256_file: &str,
            minisig: &str,
            comment: &str,
        ) -> Result<(), VerifyError> {
            let keys = TrustedKeys::parse([PUB_A, PUB_B]).unwrap();
            verify(
                &self.tarball(),
                sha256_file,
                minisig,
                keys.as_slice(),
                ASSET,
                comment,
            )
        }
    }

    impl Drop for Staged {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn message(result: Result<(), VerifyError>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn accepts_a_tarball_signed_by_the_current_key() {
        let staged = Staged::new("update-a", TARBALL);
        staged.verify(SHA256_FILE, SIG_A, COMMENT).unwrap();
    }

    #[test]
    fn accepts_a_tarball_signed_by_the_next_key() {
        let staged = Staged::new("update-b", TARBALL);
        staged.verify(SHA256_FILE, SIG_B, COMMENT).unwrap();
    }

    #[test]
    fn rejects_an_untrusted_key() {
        let staged = Staged::new("update-c", TARBALL);
        // C's signature is sound: it verifies once C is trusted.
        let keys = TrustedKeys::parse([PUB_C]).unwrap();
        verify(
            &staged.tarball(),
            SHA256_FILE,
            SIG_C,
            keys.as_slice(),
            ASSET,
            COMMENT,
        )
        .unwrap();
        // A wrong expected comment must not hide the key-id failure.
        assert_eq!(
            message(staged.verify(SHA256_FILE, SIG_C, "nope")),
            format!("signature check failed for {ASSET}: unknown key id")
        );
    }

    #[test]
    fn rejects_a_bad_checksum() {
        let mut tampered = TARBALL.to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        let staged = Staged::new("update-sum", &tampered);
        // The signature fails too; the checksum is the earlier check.
        assert_eq!(
            message(staged.verify(SHA256_FILE, SIG_A, COMMENT)),
            format!("checksum mismatch for {ASSET}")
        );
        assert_eq!(
            message(staged.verify(SHA256_FILE, SIG_C, COMMENT)),
            format!("checksum mismatch for {ASSET}")
        );
    }

    #[test]
    fn rejects_a_bad_sha256_line() {
        let staged = Staged::new("update-line", TARBALL);
        let digest = SHA256_FILE.split_once("  ").unwrap().0;
        let flipped = format!(
            "{}{}  {ASSET}\n",
            if digest.starts_with('0') { '1' } else { '0' },
            &digest[1..]
        );
        let bad = [
            flipped,
            "not a checksum\n".to_string(),
            String::new(),
            format!("{digest} {ASSET}\n"),
            format!("{digest} *{ASSET}\n"),
            format!("{digest}  other.tar.gz\n"),
            format!("{}  {ASSET}\n", digest.to_ascii_uppercase()),
            format!("{}  {ASSET}\n", &digest[1..]),
            format!("{digest}  {ASSET}\n{digest}  {ASSET}\n"),
            format!("{digest}  {ASSET}\r\n"),
            format!("{digest}  {ASSET}\r"),
        ];
        for line in &bad {
            assert_eq!(
                message(staged.verify(line, SIG_A, COMMENT)),
                format!("checksum mismatch for {ASSET}"),
                "{line:?}"
            );
        }
    }

    #[test]
    fn rejects_a_legacy_signature() {
        let staged = Staged::new("update-legacy", TARBALL);
        // The fixture is a sound legacy signature, refused only for its mode.
        let key = minisign_verify::PublicKey::decode(PUB_A).unwrap();
        let signature = Signature::decode(SIG_LEGACY).unwrap();
        key.verify(TARBALL, &signature, true).unwrap();
        assert_eq!(
            message(staged.verify(SHA256_FILE, SIG_LEGACY, COMMENT)),
            format!("signature check failed for {ASSET}: legacy signature")
        );
    }

    #[test]
    fn rejects_a_bad_signature() {
        let staged = Staged::new("update-sig", TARBALL);
        let mut lines: Vec<&str> = SIG_A.lines().collect();
        // 64 zero bytes: a global signature that decodes and does not verify.
        let zeros = format!("{}==", "A".repeat(86));
        lines[3] = &zeros;
        let tampered = lines.join("\n");
        for minisig in [tampered.as_str(), "not a signature\n"] {
            assert_eq!(
                message(staged.verify(SHA256_FILE, minisig, COMMENT)),
                format!("signature check failed for {ASSET}: bad signature"),
                "{minisig:?}"
            );
        }
    }

    #[test]
    fn rejects_a_changed_tarball_with_a_matching_checksum() {
        let mut tampered = TARBALL.to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        let staged = Staged::new("update-forged", &tampered);
        let digest: String = Sha256::digest(&tampered)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            message(staged.verify(&format!("{digest}  {ASSET}\n"), SIG_A, COMMENT)),
            format!("signature check failed for {ASSET}: bad signature")
        );
    }

    #[test]
    fn rejects_a_wrong_trusted_comment() {
        let staged = Staged::new("update-comment", TARBALL);
        assert_eq!(
            message(staged.verify(SHA256_FILE, SIG_WRONG, COMMENT)),
            format!("signature is for \"anvil v9.9.8 aarch64-apple-darwin\", not {ASSET}")
        );
        assert!(matches!(
            staged.verify(SHA256_FILE, SIG_A, &format!("{COMMENT} ")),
            Err(VerifyError::Comment { .. })
        ));
    }

    #[test]
    fn rejects_a_tarball_over_the_size_limit() {
        let staged = Staged::new("update-size", b"");
        let bytes = MAX_BYTES + 1;
        OpenOptions::new()
            .write(true)
            .open(staged.tarball())
            .unwrap()
            .set_len(bytes)
            .unwrap();
        // A bad checksum line must not be reported ahead of the size.
        assert_eq!(
            message(staged.verify("nope\n", SIG_LEGACY, COMMENT)),
            format!("{ASSET} is {bytes} bytes, over the 64 MiB limit")
        );
    }

    #[test]
    fn errors_cut_and_escape_untrusted_text() {
        let staged = Staged::new("update-escape", TARBALL);
        let asset = format!("a\n\x1b[2J\t{}", "n".repeat(80));
        let err = verify(&staged.tarball(), "nope\n", SIG_A, &[], &asset, COMMENT).unwrap_err();
        let shown = err.to_string();
        assert!(!shown.chars().any(char::is_control), "{shown}");
        assert!(
            shown.starts_with("checksum mismatch for a\\n\\x1b[2J\\t"),
            "{shown}"
        );
        assert!(shown.ends_with('…'), "{shown}");

        let comment = VerifyError::Comment {
            asset: ASSET.into(),
            comment: "ok\n\x1b[2J\tpwned".into(),
        };
        assert_eq!(
            comment.to_string(),
            format!("signature is for \"ok\\n\\x1b[2J\\tpwned\", not {ASSET}")
        );
    }

    #[test]
    fn a_missing_tarball_names_the_asset() {
        let staged = Staged::new("update-missing", TARBALL);
        std::fs::remove_file(staged.tarball()).unwrap();
        let shown = message(staged.verify(SHA256_FILE, SIG_A, COMMENT));
        assert!(shown.starts_with(&format!("{ASSET}: ")), "{shown}");
    }
}
