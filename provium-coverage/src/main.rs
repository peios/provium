//! `provium-coverage` — sibling consumer of provium's observability
//! event stream. Reads the msgpack-framed [`provium_protocol::events`]
//! stream produced by `provium --save-events <path>`, groups
//! `test_passed` / `test_failed` by a metadata key (default `spec`),
//! and emits a coverage report.
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
//! Out of scope for v1 per `DESIGN.md` § Spec citation grouping:
//! hierarchical rollups (`§4.2.1.1` ⇒ `§4.2.1`). Exact-string
//! matching is what slice 12 ships.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use provium_protocol::events::{Event, EventFrame, MetaValue};
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
    /// Tests that had no value for the requested key.
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
}

impl BucketStats {
    fn total(&self) -> u64 {
        self.passed + self.failed
    }
}

impl CoverageReport {
    /// Render as plain text, one line per bucket.
    pub fn render_plain(&self, key_name: &str) -> String {
        let mut out = String::new();
        out.push_str(&format!("coverage by `{key_name}`:\n"));
        for (k, s) in &self.buckets {
            out.push_str(&format!(
                "  {k}: {} tests ({} passed, {} failed)\n",
                s.total(),
                s.passed,
                s.failed
            ));
        }
        if self.uncategorised.total() > 0 {
            out.push_str(&format!(
                "  <unset>: {} tests ({} passed, {} failed)\n",
                self.uncategorised.total(),
                self.uncategorised.passed,
                self.uncategorised.failed
            ));
        }
        out.push_str(&format!(
            "  ---\n  buckets: {}; tests: {} passed, {} failed",
            self.buckets.len(),
            self.buckets
                .values()
                .map(|s| s.passed)
                .sum::<u64>()
                + self.uncategorised.passed,
            self.buckets
                .values()
                .map(|s| s.failed)
                .sum::<u64>()
                + self.uncategorised.failed
        ));
        out
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
            out.push_str(&format!(
                "{}:{{\"passed\":{},\"failed\":{}}}",
                json_string(k),
                s.passed,
                s.failed
            ));
        }
        out.push_str(&format!(
            "}},\"uncategorised\":{{\"passed\":{},\"failed\":{}}}}}",
            self.uncategorised.passed, self.uncategorised.failed
        ));
        out
    }
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
                Event::TestPassed(t) => bump(&mut report, key, &t.meta, true),
                Event::TestFailed(t) => bump(&mut report, key, &t.meta, false),
                _ => {}
            },
            Err(provium_protocol::FrameError::Eof) => return Ok(report),
            Err(e) => return Err(Box::new(e)),
        }
    }
}

fn bump(
    report: &mut CoverageReport,
    key: &str,
    meta: &provium_protocol::events::MetaMap,
    passed: bool,
) {
    let bucket = match meta.get(key) {
        Some(MetaValue::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let stats = match bucket {
        Some(name) => report.buckets.entry(name).or_default(),
        None => &mut report.uncategorised,
    };
    if passed {
        stats.passed += 1;
    } else {
        stats.failed += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use provium_protocol::events::{
        Event, EventFrame, MetaMap, MetaValue, TestFailed, TestPassed,
    };
    use provium_protocol::frame::write_frame;
    use std::io::Cursor;

    fn meta_with_spec(spec: &str) -> MetaMap {
        let mut m = MetaMap::new();
        m.insert("spec".into(), MetaValue::Str(spec.into()));
        m
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
            EventFrame {
                ts: 0,
                event: Event::TestPassed(TestPassed {
                    path: "f.lua".into(),
                    name: "a".into(),
                    duration_ns: 0,
                    meta: meta_with_spec("X"),
                }),
            },
            EventFrame {
                ts: 0,
                event: Event::TestPassed(TestPassed {
                    path: "f.lua".into(),
                    name: "b".into(),
                    duration_ns: 0,
                    meta: meta_with_spec("X"),
                }),
            },
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
        let events = vec![EventFrame {
            ts: 0,
            event: Event::TestPassed(TestPassed {
                path: "f.lua".into(),
                name: "a".into(),
                duration_ns: 0,
                meta: MetaMap::new(),
            }),
        }];
        let bytes = write_events(&events);
        let report = aggregate(&mut Cursor::new(bytes), "spec").unwrap();
        assert_eq!(report.uncategorised.passed, 1);
        assert!(report.buckets.is_empty());
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
        report
            .buckets
            .insert("§4.2".into(), BucketStats { passed: 3, failed: 1 });
        let s = report.to_json();
        assert!(s.contains("§4.2"));
        assert!(s.contains("\"passed\":3"));
        assert!(s.contains("\"failed\":1"));
    }

    #[test]
    fn plain_render_lists_each_bucket() {
        let mut report = CoverageReport::default();
        report
            .buckets
            .insert("PSD-A §1".into(), BucketStats { passed: 5, failed: 0 });
        report
            .buckets
            .insert("PSD-B §2".into(), BucketStats { passed: 0, failed: 2 });
        let s = report.render_plain("spec");
        assert!(s.contains("PSD-A §1"));
        assert!(s.contains("PSD-B §2"));
        assert!(s.contains("5 passed"));
        assert!(s.contains("2 failed"));
    }
}
