//! The shared zstd encode pass: one frame family for both the ingest
//! path and the rewrite pass, so the two cannot drift apart. Every
//! frame pledges its source size in the header and carries a checksum.

use std::io::{self, Read, Write};
use std::path::Path;

/// Bytes per copy read; same rationale as ingest's chunk.
const CHUNK: usize = 64 * 1024;

/// Encode the bytes from `source` (exactly `source_len` of them, which
/// is pledged in the frame header so `stat` can report the logical size
/// from the header alone) as a zstd frame at `level` into the freshly
/// created file at `staged`. A checksum is included. The staged file is
/// fsynced before returning; the caller commits it by atomic rename and
/// owns removing it if this fails.
pub fn encode_frame_into<R: Read>(
    staged: &Path,
    source: &mut R,
    source_len: u64,
    level: i32,
) -> io::Result<()> {
    let output = std::fs::File::create(staged)?;
    let mut encoder = zstd::stream::raw::Encoder::with_dictionary(level, &[])?;
    encoder.set_pledged_src_size(Some(source_len))?;
    encoder.set_parameter(zstd::stream::raw::CParameter::ChecksumFlag(true))?;
    let mut writer =
        zstd::stream::write::Encoder::with_encoder(std::io::BufWriter::new(output), encoder);
    let mut buf = vec![0u8; CHUNK];
    let mut remaining = source_len;
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        remaining = remaining.saturating_sub(n as u64);
    }
    debug_assert_eq!(remaining, 0, "source shorter than the pledged size");
    // finish() writes the frame trailer and hands back the buffered
    // writer; flush before the fsync so every byte is on disk.
    let mut inner = writer.finish()?;
    inner.flush()?;
    inner.get_ref().sync_all()?;
    Ok(())
}
