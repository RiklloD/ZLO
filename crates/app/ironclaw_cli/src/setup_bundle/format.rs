//! The `.zlobundle` container: a short plain header, then the payload
//! encrypted with AES-256-GCM in 1 MiB chunks (the STREAM construction:
//! each chunk's nonce is a random 7-byte prefix, the chunk counter and a
//! last-chunk flag, so chunks cannot be reordered, dropped or truncated
//! without the tag check failing). The key comes from the passphrase with
//! PBKDF2-HMAC-SHA256. The header is the associated data of every chunk.
//!
//! ```text
//! header  "ZLOSETUP" | version u16 | kdf u8 | 0u8 | iterations u32
//!         | salt [16] | nonce prefix [7] | chunk size u32        (big-endian)
//! body    chunk*  — each ciphertext(chunk size bytes) + 16-byte tag,
//!                   the last one shorter (possibly empty) with flag 1
//! ```
//!
//! Inside the decrypted stream, records (see [`Record`]):
//! `tag u8 | name length u16 | name | size u64 | bytes`.

use std::io::{Read, Write};

use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Nonce, Payload},
};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroize as _;

use super::BundleError;

pub(crate) const MAGIC: &[u8; 8] = b"ZLOSETUP";
/// The newest container format this build reads and the one it writes.
pub(crate) const FORMAT_VERSION: u16 = 1;
const KDF_PBKDF2_SHA256: u8 = 1;
/// OWASP's 2023 figure for PBKDF2-HMAC-SHA256.
pub(crate) const DEFAULT_ITERATIONS: u32 = 600_000;
#[cfg(not(test))]
const MIN_ITERATIONS: u32 = 100_000;
/// Unit tests derive keys in a debug build; keep them quick.
#[cfg(test)]
pub(crate) const MIN_ITERATIONS: u32 = 1_000;
/// A header asking for more is refused rather than spun on.
const MAX_ITERATIONS: u32 = 20_000_000;
const CHUNK_SIZE: u32 = 1 << 20;
const MIN_CHUNK_SIZE: u32 = 1 << 12;
const MAX_CHUNK_SIZE: u32 = 1 << 24;
const TAG_LEN: usize = 16;
const SALT_LEN: usize = 16;
const NONCE_PREFIX_LEN: usize = 7;
pub(crate) const HEADER_LEN: usize = 8 + 2 + 1 + 1 + 4 + SALT_LEN + NONCE_PREFIX_LEN + 4;

/// Record tags in the decrypted stream.
pub(crate) const TAG_END: u8 = 0;
pub(crate) const TAG_FILE: u8 = 1;
/// Reserved so a symlink entry is named and refused, never followed.
pub(crate) const TAG_SYMLINK: u8 = 2;
pub(crate) const TAG_MANIFEST: u8 = 3;
pub(crate) const MAX_NAME_LEN: usize = 512;

fn derive_key(
    passphrase: &SecretString,
    salt: &[u8],
    iterations: u32,
) -> Result<Aes256Gcm, BundleError> {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
        passphrase.expose_secret().as_bytes(),
        salt,
        iterations,
        &mut key,
    );
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| BundleError::Crypto);
    key.zeroize();
    cipher
}

fn nonce(prefix: &[u8; NONCE_PREFIX_LEN], counter: u32, last: bool) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..NONCE_PREFIX_LEN].copy_from_slice(prefix);
    nonce[NONCE_PREFIX_LEN..NONCE_PREFIX_LEN + 4].copy_from_slice(&counter.to_be_bytes());
    nonce[11] = u8::from(last);
    nonce
}

/// Writes the header, then encrypts everything written to it.
pub(crate) struct BundleWriter<W: Write> {
    inner: W,
    cipher: Aes256Gcm,
    header: [u8; HEADER_LEN],
    prefix: [u8; NONCE_PREFIX_LEN],
    buffer: Vec<u8>,
    counter: u32,
}

impl<W: Write> BundleWriter<W> {
    pub(crate) fn new(
        mut inner: W,
        passphrase: &SecretString,
        iterations: u32,
    ) -> Result<Self, BundleError> {
        let salt: [u8; SALT_LEN] = rand::random();
        let prefix: [u8; NONCE_PREFIX_LEN] = rand::random();
        let mut header = [0u8; HEADER_LEN];
        let mut at = 0;
        for part in [
            &MAGIC[..],
            &FORMAT_VERSION.to_be_bytes(),
            &[KDF_PBKDF2_SHA256, 0],
            &iterations.to_be_bytes(),
            &salt,
            &prefix,
            &CHUNK_SIZE.to_be_bytes(),
        ] {
            header[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        let cipher = derive_key(passphrase, &salt, iterations)?;
        inner.write_all(&header)?;
        Ok(Self {
            inner,
            cipher,
            header,
            prefix,
            buffer: Vec::with_capacity(CHUNK_SIZE as usize),
            counter: 0,
        })
    }

    fn seal(&mut self, last: bool) -> Result<(), BundleError> {
        let nonce_bytes = nonce(&self.prefix, self.counter, last);
        let nonce =
            Nonce::<Aes256Gcm>::try_from(&nonce_bytes[..]).map_err(|_| BundleError::Crypto)?;
        let sealed = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &self.buffer,
                    aad: &self.header,
                },
            )
            .map_err(|_| BundleError::Crypto)?;
        self.inner.write_all(&sealed)?;
        self.buffer.zeroize();
        self.buffer.clear();
        self.counter = self.counter.checked_add(1).ok_or(BundleError::TooLarge)?;
        Ok(())
    }

    pub(crate) fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), BundleError> {
        while !bytes.is_empty() {
            let room = CHUNK_SIZE as usize - self.buffer.len();
            let take = room.min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == CHUNK_SIZE as usize {
                self.seal(false)?;
            }
        }
        Ok(())
    }

    /// Seal the final chunk and hand back the inner writer.
    pub(crate) fn finish(mut self) -> Result<W, BundleError> {
        self.seal(true)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// Parsed plain header.
pub(crate) struct Header {
    bytes: [u8; HEADER_LEN],
    iterations: u32,
    salt: [u8; SALT_LEN],
    prefix: [u8; NONCE_PREFIX_LEN],
    chunk_size: u32,
}

impl Header {
    /// Read and check the header. Runs before the (slow) key derivation, so
    /// a file from a newer ZLO is refused without asking for anything.
    pub(crate) fn read(inner: &mut impl Read) -> Result<Self, BundleError> {
        let mut bytes = [0u8; HEADER_LEN];
        read_full(inner, &mut bytes).and_then(|n| {
            if n == HEADER_LEN {
                Ok(())
            } else {
                Err(BundleError::NotABundle)
            }
        })?;
        if &bytes[..8] != MAGIC {
            return Err(BundleError::NotABundle);
        }
        let version = u16::from_be_bytes([bytes[8], bytes[9]]);
        if version == 0 {
            return Err(BundleError::NotABundle);
        }
        if version > FORMAT_VERSION {
            return Err(BundleError::NewerFormat {
                found: version,
                supported: FORMAT_VERSION,
            });
        }
        if bytes[10] != KDF_PBKDF2_SHA256 || bytes[11] != 0 {
            return Err(BundleError::NotABundle);
        }
        let iterations = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
            return Err(BundleError::NotABundle);
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&bytes[16..16 + SALT_LEN]);
        let mut prefix = [0u8; NONCE_PREFIX_LEN];
        prefix.copy_from_slice(&bytes[32..32 + NONCE_PREFIX_LEN]);
        let chunk_size = u32::from_be_bytes([bytes[39], bytes[40], bytes[41], bytes[42]]);
        if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size) {
            return Err(BundleError::NotABundle);
        }
        Ok(Self {
            bytes,
            iterations,
            salt,
            prefix,
            chunk_size,
        })
    }
}

/// `read` until `buf` is full or the input ends; returns the bytes read.
fn read_full(inner: &mut impl Read, buf: &mut [u8]) -> Result<usize, BundleError> {
    let mut filled = 0;
    while filled < buf.len() {
        match inner.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(filled)
}

/// Decrypts and authenticates the body chunk by chunk.
pub(crate) struct BundleReader<R: Read> {
    inner: R,
    cipher: Aes256Gcm,
    header: Header,
    plain: Vec<u8>,
    position: usize,
    counter: u32,
    done: bool,
}

impl<R: Read> BundleReader<R> {
    /// Derive the key and authenticate the first chunk, so a wrong
    /// passphrase fails here, before the caller writes anything.
    pub(crate) fn open(mut inner: R, passphrase: &SecretString) -> Result<Self, BundleError> {
        let header = Header::read(&mut inner)?;
        let cipher = derive_key(passphrase, &header.salt, header.iterations)?;
        let mut reader = Self {
            inner,
            cipher,
            header,
            plain: Vec::new(),
            position: 0,
            counter: 0,
            done: false,
        };
        reader.next_chunk().map_err(|error| match error {
            BundleError::Damaged => BundleError::WrongPassphraseOrDamaged,
            other => other,
        })?;
        Ok(reader)
    }

    fn next_chunk(&mut self) -> Result<(), BundleError> {
        let full = self.header.chunk_size as usize + TAG_LEN;
        let mut sealed = vec![0u8; full];
        let n = read_full(&mut self.inner, &mut sealed)?;
        if n < TAG_LEN {
            return Err(BundleError::Truncated);
        }
        sealed.truncate(n);
        let last = n < full;
        let nonce_bytes = nonce(&self.header.prefix, self.counter, last);
        let nonce =
            Nonce::<Aes256Gcm>::try_from(&nonce_bytes[..]).map_err(|_| BundleError::Crypto)?;
        let plain = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &sealed,
                    aad: &self.header.bytes,
                },
            )
            .map_err(|_| BundleError::Damaged)?;
        self.plain.zeroize();
        self.plain = plain;
        self.position = 0;
        self.counter = self.counter.checked_add(1).ok_or(BundleError::TooLarge)?;
        if last {
            self.done = true;
            let mut probe = [0u8; 1];
            if read_full(&mut self.inner, &mut probe)? != 0 {
                return Err(BundleError::Damaged);
            }
        }
        Ok(())
    }

    /// Fill `buf` exactly from the decrypted stream.
    pub(crate) fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<(), BundleError> {
        while !buf.is_empty() {
            if self.position == self.plain.len() {
                if self.done {
                    return Err(BundleError::Truncated);
                }
                self.next_chunk()?;
                continue;
            }
            let take = (self.plain.len() - self.position).min(buf.len());
            buf[..take].copy_from_slice(&self.plain[self.position..self.position + take]);
            self.position += take;
            buf = &mut buf[take..];
        }
        Ok(())
    }

    /// After the end record: the stream must be fully consumed.
    pub(crate) fn finish(mut self) -> Result<(), BundleError> {
        while !self.done {
            if self.position != self.plain.len() {
                return Err(BundleError::Damaged);
            }
            self.next_chunk()?;
        }
        if self.position != self.plain.len() {
            return Err(BundleError::Damaged);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(text: &str) -> SecretString {
        SecretString::from(text.to_string())
    }

    fn seal(payload: &[u8], passphrase: &str) -> Vec<u8> {
        let mut writer =
            BundleWriter::new(Vec::new(), &pass(passphrase), MIN_ITERATIONS).expect("writer");
        writer.write_all(payload).expect("write");
        writer.finish().expect("finish")
    }

    fn open_all(bytes: &[u8], passphrase: &str, len: usize) -> Result<Vec<u8>, BundleError> {
        let mut reader = BundleReader::open(bytes, &pass(passphrase))?;
        let mut out = vec![0u8; len];
        reader.read_exact(&mut out)?;
        reader.finish()?;
        Ok(out)
    }

    #[test]
    fn round_trips_across_chunk_boundaries() {
        for len in [0usize, 5, CHUNK_SIZE as usize, CHUNK_SIZE as usize * 2 + 3] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let sealed = seal(&payload, "correct horse");
            assert_eq!(
                open_all(&sealed, "correct horse", len).expect("open"),
                payload
            );
        }
    }

    #[test]
    fn wrong_passphrase_tamper_truncation_and_newer_format_are_refused() {
        let payload = vec![7u8; CHUNK_SIZE as usize + 10];
        let sealed = seal(&payload, "right");
        assert!(matches!(
            BundleReader::open(&sealed[..], &pass("wrong")),
            Err(BundleError::WrongPassphraseOrDamaged)
        ));

        let mut tampered = sealed.clone();
        let last = tampered.len() - 3;
        tampered[last] ^= 1;
        assert!(matches!(
            open_all(&tampered, "right", payload.len()),
            Err(BundleError::Damaged)
        ));

        // Drop the final (short) chunk: the full first chunk is not "last".
        let cut = &sealed[..HEADER_LEN + CHUNK_SIZE as usize + TAG_LEN];
        assert!(open_all(cut, "right", payload.len()).is_err());

        let mut appended = sealed.clone();
        appended.push(0);
        assert!(open_all(&appended, "right", payload.len()).is_err());

        let mut header_edit = sealed.clone();
        header_edit[20] ^= 1; // salt byte: the header is authenticated
        assert!(BundleReader::open(&header_edit[..], &pass("right")).is_err());

        let mut newer = sealed;
        newer[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_be_bytes());
        assert!(matches!(
            BundleReader::open(&newer[..], &pass("right")),
            Err(BundleError::NewerFormat { found, .. }) if found == FORMAT_VERSION + 1
        ));

        assert!(matches!(
            BundleReader::open(&b"not a bundle at all, sorry, nope"[..], &pass("right")),
            Err(BundleError::NotABundle)
        ));
    }
}
