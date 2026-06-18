//! `provium-coverage` — sibling consumer of provium's observability
//! event stream. Reads the msgpack-framed [`provium_protocol::events`]
//! stream produced by `provium --save-events <path>`, groups
//! `test_passed` / `test_failed` / `test_skipped` by a metadata key
//! (default `spec`), and emits a coverage report.
//!
//! Slice 12 surface (kept deliberately minimal):
//!
//! ```text
//! provium-coverage [--from PATH] [--by KEY] [--json]
//!     --from PATH    read event frames from PATH (default: stdin)
//!     --by KEY       group by `meta[KEY]` value (default: spec)
//!     --json         JSON output instead of plain text
//! ```
//!
//! Grouping value handling:
//!
//! * A **string** `meta[KEY]` is one bucket.
//! * An **array of strings** explodes into one bucket per element — a
//!   test tagged `rows = {"A", "B"}` counts toward both `A` and `B`.
//!   This is what lets a single test cover several matrix rows while
//!   still showing per-row coverage.
//! * Any other shape (missing, int, map, …) is `<unset>`.
//!
//! Skipped tests are counted (as `skipped`, distinct from passed) so a
//! row covered only by a tracked-but-not-yet-runnable test is visible as
//! covered-but-skipped rather than silently absent.
//!
//! Out of scope per `DESIGN.md` § Spec citation grouping: hierarchical
//! rollups (`§4.2.1.1` ⇒ `§4.2.1`). Exact-string matching is what ships.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use provium_protocol::events::{Event, EventFrame, MetaMap, MetaValue};
use provium_protocol::frame::{read_frame, DEFAULT_MAX_FRAME_BYTES};

#[derive(Debug, Parser)]
#[command(name = "provium-coverage", version, about)]
struct Args {
    /// Read frames from PATH. If omitted, read from stdin.
    #[arg(long, value_name = "PATH")]
    from: Option<PathBuf>,

    /// Metadata key to group by. Default `spec` matches the
    /// design's § Spec citation example.
    #[arg(long, default_value = "spec")]
    by: String,

    /// Emit JSON instead of plain text.
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("provium-coverage: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader: Box<dyn Read> = match &args.from {
        Some(path) => Box::new(BufReader::new(std::fs::File::open(path)?)),
        None => Box::new(BufReader::new(io::stdin())),
    };

    let report = aggregate(&mut reader, &args.by)?;

    if args.json {
        println!("{}", report.to_json());
    } else {
        println!("{}", report.render_plain(&args.by));
    }
    Ok(())
}

/// Aggregated coverage report.
#[derive(Debug, Default)]
pub struct CoverageReport {
    /// Tests that had no string value for the requested key.
    pub uncategorised: BucketStats,
    /// Buckets keyed by the metadata value.
    pub buckets: BTreeMap<String, BucketStats>,
}

/// Per-bucket counts.
#[derive(Debug, Default)]
pub struct BucketStats {
    /// Tests passed.
    pub passed: u64,
    /// Tests failed.
    pub failed: u64,
    /// Tests skipped (declarative `meta.skip`, `t:skip()`, or filtered).
    pub skipped: u64,
}

impl BucketStats {
    fn total(&self) -> u64 {
        self.passed + self.failed + self.skipped
    }

    fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Passed => self.passed += 1,
            Outcome::Failed => self.failed += 1,
            Outcome::Skipped => self.skipped += 1,
        }
    }
}

/// A single test's outcome, for [`bump`].
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Passed,
    Failed,
    Skipped,
}

impl CoverageReport {
    /// Render as plain text, one line per bucket.
    pub fn render_plain(&self, key_name: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!("coverage by `{key_name}`:\n"));
        for (k, s) in &self.buckets {
            out.push_str(&format!("  {k}: {}\n", render_counts(s)));
        }
        if self.uncategorised.total() > 0 {
            out.push_str(&format!("  <unset>: {}\n", render_counts(&self.uncategorised)));
        }
        let (passed, failed, skipped) = self.totals();
        out.push_str(&format!(
            "  ---\n  buckets: {}; {passed} passed, {failed} failed, {skipped} skipped",
            self.buckets.len(),
        ));
        out
    }

    /// Sum of every bucket plus uncategorised. With array grouping a
    /// test contributes to each of its buckets, so these are coverage
    /// entries, not necessarily distinct tests.
    fn totals(&self) -> (u64, u64, u64) {
        let mut t = (
            self.uncategorised.passed,
            self.uncategorised.failed,
            self.uncategorised.skipped,
        );
        for s in self.buckets.values() {
            t.0 += s.passed;
            t.1 += s.failed;
            t.2 += s.skipped;
        }
        t
    }

    /// Render as a JSON object. Hand-rolled to avoid pulling
    /// serde_json into this crate just for one rendering path.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"buckets\":{");
        let mut first = true;
        for (k, s) in &self.buckets {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&format!("{}:{}", json_string(k), bucket_json(s)));
        }
        out.push_str(&format!(
            "}},\"uncategorised\":{}}}",
            bucket_json(&self.uncategorised)
        ));
        out
    }
}

fn render_counts(s: &BucketStats) -> String {
    format!(
        "{} tests ({} passed, {} failed, {} skipped)",
        s.total(),
        s.passed,
        s.failed,
        s.skipped
    )
}

fn bucket_json(s: &BucketStats) -> String {
    format!(
        "{{\"passed\":{},\"failed\":{},\"skipped\":{}}}",
        s.passed, s.failed, s.skipped
    )
}

fn json_string(s: &str) -> String {
    let escaped: String = s
        .chars()
        .map(|c| match c {
            '"' => "\\\"".into(),
            '\\' => "\\\\".into(),
            '\n' => "\\n".into(),
            '\t' => "\\t".into(),
            c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32),
            c => c.to_string(),
        })
        .collect();
    format!("\"{escaped}\"")
}

/// Drain `reader` of msgpack-framed events and aggregate by `key`.
///
/// Public for slice-12 tests; binaries use [`run`].
pub fn aggregate<R: Read>(
    reader: &mut R,
    key: &str,
) -> Result<CoverageReport, Box<dyn std::error::Error>> {
    let mut report = CoverageReport::default();
    loop {
        match read_frame::<R, EventFrame>(reader, DEFAULT_MAX_FRAME_BYTES) {
            Ok(frame) => match frame.event {
                Event::TestPassed(t) => bump(&mut report, key, &t.meta, Outcome::Passed),
                Event::TestFailed(t) => bump(&mut report, key, &t.meta, Outcome::Failed),
                Event::TestSkipped(t) => bump(&mut report, key, &t.meta, Outcome::Skipped),
                _ => {}
            },
            Err(provium_protocol::FrameError::Eof) => return Ok(report),
            Err(e) => return Err(Box::new(e)),
        }
    }
}

/// Resolve the bucket names a test's `meta[key]` maps to. A string is
/// one bucket; an array of strings is one bucket per element; anything
/// else is no bucket (→ uncategorised).
fn bucket_names(meta: &MetaMap, key: &str) -> Vec<String> {
    match meta.get(key) {
        Some(MetaValue::Str(s)) => vec![s.clone()],
        Some(MetaValue::Array(items)) => items
            .iter()
            .filter_map(|v| match v {
                MetaValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn bump(report: &mut CoverageReport, key: &str, meta: &MetaMap, outcome: Outcome) {
    let names = bucket_names(meta, key);
    if names.is_empty() {
        report.uncategorised.record(outcome);
    } else {
        for name in names {
            report.buckets.entry(name).or_default().record(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use provium_protocol::events::{
        Event, EventFrame, MetaMap, MetaValue, TestFailed, TestPassed, TestSkipped,
    };
    use provium_protocol::frame::write_frame;
    use std::io::Cursor;

    fn meta_with_spec(spec: &str) -> MetaMap {
        let mut m = MetaMap::new();
        m.insert("spec".into(), MetaValue::Str(spec.into()));
        m
    }

    fn meta_with_rows(rows: &[&str]) -> MetaMap {
        let mut m = MetaMap::new();
        m.insert(
            "rows".into(),
            MetaValue::Array(rows.iter().map(|r| MetaValue::Str((*r).into())).collect()),
        );
        m
    }

    fn passed(name: &str, meta: MetaMap) -> EventFrame {
        EventFrame {
            ts: 0,
            event: Event::TestPassed(TestPassed {
                path: "f.lua".into(),
                name: name.into(),
                duration_ns: 0,
                meta,
            }),
        }
    }

    fn skipped(name: &str, meta: MetaMap) -> EventFrame {
        EventFrame {
            ts: 0,
            event: Event::TestSkipped(TestSkipped {
                path: "f.lua".into(),
                name: name.into(),
                reason: "needs a fixture".into(),
                meta,
            }),
        }
    }

    fn write_events(events: &[EventFrame]) -> Vec<u8> {
        let mut buf = Vec::new();
        for e in events {
            write_frame(&mut buf, e, DEFAULT_MAX_FRAME_BYTES).unwrap();
        }
        buf
    }

    #[test]
    fn aggregates_passed_failed_per_bucket() {
        let events = vec![
            passed("a", meta_with_spec("X")),
            passed("b", meta_with_spec("X")),
            EventFrame {
                ts: 0,
                event: Event::TestFailed(TestFailed {
                    path: "f.lua".into(),
                    name: "c".into(),
                    duration_ns: 0,
                    reason: "boom".into(),
                    console_excerpt: String::new(),
                    meta: meta_with_spec("Y"),
                }),
            },
        ];
        let bytes = write_events(&events);
        let report = aggregate(&mut Cursor::new(bytes), "spec").unwrap();
        assert_eq!(report.buckets.get("X").unwrap().passed, 2);
        assert_eq!(report.buckets.get("X").unwrap().failed, 0);
        assert_eq!(report.buckets.get("Y").unwrap().failed, 1);
    }

    #[test]
    fn uncategorised_counts_when_key_missing() {
        let events = vec![passed("a", MetaMap::new())];
        let bytes = write_events(&events);
        let report = aggregate(&mut Cursor::new(bytes), "spec").unwrap();
        assert_eq!(report.uncategorised.passed, 1);
        assert!(report.buckets.is_empty());
    }

    #[test]
    fn skipped_tests_are_counted_per_bucket() {
        let events = vec![
            passed("value check", meta_with_spec("PSD §1")),
            skipped("behaviour check", meta_with_spec("PSD §1")),
        ];
        let bytes = write_events(&events);
        let report = aggregate(&mut Cursor::new(bytes), "spec").unwrap();
        let b = report.buckets.get("PSD §1").unwrap();
        assert_eq!(b.passed, 1);
        assert_eq!(b.skipped, 1);
        assert_eq!(b.total(), 2);
    }

    #[test]
    fn array_meta_explodes_into_each_bucket() {
        // One test covering three rows passes → all three rows covered.
        let events = vec![passed("no MAC LSM", meta_with_rows(&["R3", "R4", "R5"]))];
        let bytes = write_events(&events);
        let report = aggregate(&mut Cursor::new(bytes), "rows").unwrap();
        for r in ["R3", "R4", "R5"] {
            assert_eq!(report.buckets.get(r).unwrap().passed, 1, "{r}");
        }
        assert_eq!(report.uncategorised.total(), 0);
    }

    #[test]
    fn skipped_array_meta_marks_each_row_skipped() {
        let events = vec![skipped("unsigned module load", meta_with_rows(&["R10"]))];
        let bytes = write_events(&events);
        let report = aggregate(&mut Cursor::new(bytes), "rows").unwrap();
        assert_eq!(report.buckets.get("R10").unwrap().skipped, 1);
        assert_eq!(report.buckets.get("R10").unwrap().passed, 0);
    }

    #[test]
    fn ignores_non_test_events() {
        use provium_protocol::events::{FileCompleted, FileStatus};
        let events = vec![EventFrame {
            ts: 0,
            event: Event::FileCompleted(FileCompleted {
                path: "f.lua".into(),
                status: FileStatus::Passed,
                duration_ns: 0,
            }),
        }];
        let bytes = write_events(&events);
        let report = aggregate(&mut Cursor::new(bytes), "spec").unwrap();
        assert!(report.buckets.is_empty());
        assert_eq!(report.uncategorised.passed, 0);
    }

    #[test]
    fn json_render_round_trips_through_parsing() {
        let mut report = CoverageReport::default();
        report.buckets.insert(
            "§4.2".into(),
            BucketStats { passed: 3, failed: 1, skipped: 2 },
        );
        let s = report.to_json();
        assert!(s.contains("§4.2"));
        assert!(s.contains("\"passed\":3"));
        assert!(s.contains("\"failed\":1"));
        assert!(s.contains("\"skipped\":2"));
    }

    #[test]
    fn plain_render_lists_each_bucket() {
        let mut report = CoverageReport::default();
        report.buckets.insert(
            "PSD-A §1".into(),
            BucketStats { passed: 5, failed: 0, skipped: 1 },
        );
        report.buckets.insert(
            "PSD-B §2".into(),
            BucketStats { passed: 0, failed: 2, skipped: 0 },
        );
        let s = report.render_plain("spec");
        assert!(s.contains("PSD-A §1"));
        assert!(s.contains("PSD-B §2"));
        assert!(s.contains("5 passed"));
        assert!(s.contains("2 failed"));
        assert!(s.contains("1 skipped"));
    }
}
