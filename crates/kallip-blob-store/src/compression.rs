//! On-disk representation compression: zstd frames, sniffed on read.
//!
//! The storage format stays address-stable: blob ids hash the ORIGINAL
//! bytes (ingest hashes before encoding), so switching the on-disk
//! representation to zstd never moves an address and every existing
//! reference stays valid. Reads sniff the zstd frame magic, so legacy
//! uncompressed blobs and compressed ones coexist and a store flipped to
//! `Off` keeps reading both.

use std::io;

/// The zstd frame magic (`0x28 B5 2F FD`, little-endian): every zstd
/// stream starts with it, which is what makes representation sniffing a
/// 4-byte prefix check.
pub(crate) const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// The on-disk representation for newly ingested blobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Compression {
    /// Store raw bytes (the historical format).
    #[default]
    Off,
    /// Store a zstd frame at the given level (3 is the zstd default and
    /// our default; the levels above it trade CPU for ratio with
    /// rapidly diminishing returns).
    Zstd { level: i32 },
}

impl Compression {
    /// The config word selecting zstd framing.
    pub(crate) const ZSTD_WORD: &'static str = "zstd";
    /// The config word keeping objects raw (the default).
    pub(crate) const OFF_WORD: &'static str = "off";
    /// The word table config surfaces accept (CLI flags, env files).
    /// Single source: the args parser validates against it and the
    /// runtime mapping branches on the same words, so a new word or a
    /// rename lands in exactly one place.
    pub const CONFIG_WORDS: [&'static str; 2] = [Self::OFF_WORD, Self::ZSTD_WORD];

    /// Map a validated config word to the policy value. Unknown words
    /// fall back to `Off`; config surfaces validate against
    /// [`Self::CONFIG_WORDS`] before this runs.
    pub fn from_config_word(word: &str, zstd_level: i32) -> Self {
        match word {
            Self::ZSTD_WORD => Compression::Zstd { level: zstd_level },
            _ => Compression::Off,
        }
    }
}

/// Whether `bytes` starts with a zstd frame.
pub fn is_zstd_frame(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[..4] == ZSTD_MAGIC
}

/// Decode a full zstd frame. The blob-store API already materializes
/// whole blobs in memory (`get`), so a buffered decode matches the
/// surrounding cost model.
pub fn decode(frame: &[u8]) -> io::Result<Vec<u8>> {
    zstd::stream::decode_all(frame)
}
