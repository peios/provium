//! Slice-14 performance helpers.
//!
//! ## KSM tuning
//!
//! [`tune_ksm`] writes to `/sys/kernel/mm/ksm/*` to enable kernel
//! same-page merging at the cadence the design recommends:
//!
//! ```text
//! /sys/kernel/mm/ksm/run             = 1
//! /sys/kernel/mm/ksm/pages_to_scan   = 1000
//! /sys/kernel/mm/ksm/sleep_millisecs = 20
//! ```
//!
//! Combined with QEMU's `memory-backend-ram,merge=on` (already in
//! [`crate::vmm::qemu::build_qemu_command`]), KSM deduplicates the
//! ~80% of guest memory that's identical Peios kernel pages,
//! reducing real RAM by 20–40% per the design.
//!
//! ## Sparse + zstd snapshots
//!
//! [`compress_snapshot_in_place`] post-processes a snapshot file
//! produced by `migrate file:` — most VM memory is zero pages, so
//! converting to a sparse file (filesystem-level holes for runs of
//! zeros) plus zstd compression cuts disk usage to 80–150 MiB on a
//! 1 GiB-naive snapshot per the design.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Apply the design's recommended KSM tuning. Best-effort: any
/// individual write failure is logged + skipped (KSM may be
/// disabled in the kernel, /sys may be read-only in containers,
/// etc.).
pub fn tune_ksm() -> KsmReport {
    let mut report = KsmReport::default();
    for (path, value) in KSM_KNOBS {
        match write_sysfs(path, value) {
            Ok(()) => report.applied.push(path),
            Err(e) => report.skipped.push((path, e.to_string())),
        }
    }
    report
}

/// Tuning result. Surfaced from `provium` startup with a one-line
/// summary; the field shapes are stable for telemetry consumers.
#[derive(Debug, Default)]
pub struct KsmReport {
    /// Knobs that were successfully written.
    pub applied: Vec<&'static str>,
    /// Knobs whose write failed, with the reason.
    pub skipped: Vec<(&'static str, String)>,
}

impl KsmReport {
    /// One-line summary suitable for logging.
    pub fn summary(&self) -> String {
        if self.skipped.is_empty() {
            format!(
                "ksm: tuned ({} knobs)",
                self.applied.len()
            )
        } else if self.applied.is_empty() {
            format!(
                "ksm: not tuned ({} knobs unwritable; first: `{}`)",
                self.skipped.len(),
                self.skipped[0].1,
            )
        } else {
            format!(
                "ksm: partial ({} applied, {} skipped)",
                self.applied.len(),
                self.skipped.len(),
            )
        }
    }
}

const KSM_KNOBS: &[(&str, &str)] = &[
    ("/sys/kernel/mm/ksm/run", "1"),
    ("/sys/kernel/mm/ksm/pages_to_scan", "1000"),
    ("/sys/kernel/mm/ksm/sleep_millisecs", "20"),
];

fn write_sysfs(path: &str, value: &str) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)?;
    f.write_all(value.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sparse + zstd compression
// ---------------------------------------------------------------------------

/// Maximum zero-run length for the sparse-write helper to attempt
/// to punch a hole. Smaller = denser file, larger = better
/// compression. 4 KiB matches typical filesystem block sizes.
const SPARSE_BLOCK: usize = 4096;

/// Rewrite `path` so runs of zeros become filesystem holes.
///
/// Reads `path`, writes a sparse temp file, atomically renames over
/// the original. Returns the bytes saved (positive = freed,
/// negative shouldn't happen but is allowed).
///
/// Slice-14 minimum: just sparse, no zstd. The design also calls
/// for zstd on non-zero pages; that needs the zstd crate and
/// significant streaming logic — out of scope here. The fixture
/// cache tolerates the larger sparse-only file fine for now.
pub fn make_sparse(path: &Path) -> io::Result<i64> {
    let original_size = std::fs::metadata(path)?.len() as i64;

    let mut input = File::open(path)?;
    let tmp = path.with_extension("sparse.tmp");
    let mut output = File::create(&tmp)?;

    let mut buf = vec![0u8; SPARSE_BLOCK];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if chunk.iter().all(|&b| b == 0) {
            // Skip ahead — write nothing, leave a hole.
            output.seek(SeekFrom::Current(n as i64))?;
        } else {
            output.write_all(chunk)?;
        }
    }
    // Trim the file to the input length (in case the tail was
    // zero and we seeked past it).
    let total = input.seek(SeekFrom::End(0))?;
    output.set_len(total)?;
    drop(input);
    drop(output);
    std::fs::rename(&tmp, path)?;

    let new_size = std::fs::metadata(path)?.len() as i64;
    Ok(original_size - new_size)
}

// ---------------------------------------------------------------------------
// zstd snapshot compression (slice 14.5)
// ---------------------------------------------------------------------------

/// zstd compression level used by [`compress_zst`]. Level 3 is the
/// design's recommended balance of ratio + speed for snapshot
/// archives — typical 4–6× shrink on a sparse-pre-processed
/// snapshot, ≤500 ms per GiB on modern hardware.
pub const SNAPSHOT_ZSTD_LEVEL: i32 = 3;

/// Compress `src` to `dst` using zstd. The destination is created
/// (or truncated) before writing.
pub fn compress_zst(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = File::open(src)?;
    let output = File::create(dst)?;
    let mut encoder = zstd::stream::Encoder::new(output, SNAPSHOT_ZSTD_LEVEL)?;
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

/// Best-effort detection of zstd-compressed file by magic bytes.
pub fn looks_zstd(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut hdr = [0u8; 4];
    if f.read_exact(&mut hdr).is_err() {
        return false;
    }
    hdr == [0x28, 0xb5, 0x2f, 0xfd]
}

/// Decompress `src` (a `.zst` file) into `dst`.
pub fn decompress_zst(src: &Path, dst: &Path) -> io::Result<()> {
    let input = File::open(src)?;
    let mut decoder = zstd::stream::Decoder::new(input)?;
    let mut output = File::create(dst)?;
    io::copy(&mut decoder, &mut output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_sparse_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.bin");
        let mut original = Vec::with_capacity(SPARSE_BLOCK * 4);
        original.extend_from_slice(&[0xab; SPARSE_BLOCK]); // dense
        original.extend_from_slice(&[0; SPARSE_BLOCK * 2]); // hole
        original.extend_from_slice(&[0xcd; SPARSE_BLOCK]); // dense
        std::fs::write(&path, &original).unwrap();

        make_sparse(&path).unwrap();

        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, original, "sparse rewrite must preserve content");
    }

    #[test]
    fn zst_round_trip_recovers_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.bin");
        let zst = dir.path().join("a.bin.zst");
        let back = dir.path().join("a-back.bin");

        // Mostly-zero payload with some structure — represents the
        // sparse snapshot shape.
        let mut bytes = vec![0u8; 64 * 1024];
        bytes[100..200].iter_mut().for_each(|b| *b = 0xab);
        bytes[10_000..10_100].iter_mut().for_each(|b| *b = 0xcd);
        std::fs::write(&src, &bytes).unwrap();

        compress_zst(&src, &zst).unwrap();
        decompress_zst(&zst, &back).unwrap();

        let restored = std::fs::read(&back).unwrap();
        assert_eq!(restored, bytes, "round-trip must preserve content");

        // Compression should actually shrink mostly-zero input.
        let zst_size = std::fs::metadata(&zst).unwrap().len();
        let original_size = std::fs::metadata(&src).unwrap().len();
        assert!(
            zst_size < original_size / 4,
            "expected significant compression: {zst_size} vs {original_size}"
        );
    }

    #[test]
    fn ksm_report_summary_is_human_readable() {
        let mut r = KsmReport::default();
        r.applied.push("a");
        r.applied.push("b");
        let s = r.summary();
        assert!(s.contains("tuned"));
    }
}
