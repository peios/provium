//! Bridge → host-network-command translation (slice 9.5).
//!
//! Slice 9 captured the bridge graph state (attachments, partitions,
//! impairments) but did nothing to the host. Slice 9.5 adds the
//! translation layer: given a [`crate::bridge::Bridge`], emit the
//! sequence of `ip` / `tc` invocations that would realise it.
//!
//! ## Why this isn't wired into VM boot yet
//!
//! Realising a bridge requires `CAP_NET_ADMIN` (TAP creation,
//! bridge creation, qdisc setup). CI environments routinely lack
//! it, and the design's broader networking story (TAP-per-VM,
//! per-VM `-netdev tap` arg passed to QEMU) is meaningfully
//! larger than this slice. Slice 9.6 wires this output into
//! `QemuVmm::launch`. For now it's a pure function with thorough
//! tests so the command sequences are correct and reviewable.

use std::process::Command;

use crate::bridge::{Bridge, DirectionalImpairment};

/// Plan describing the host-network commands that realise a bridge.
///
/// One [`NetOp`] per `ip` / `tc` invocation, in the order they need
/// to run. Tests inspect the produced sequence; production code
/// pipes it through [`run_plan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgePlan {
    /// Bridge name. Doubles as the host bridge interface name.
    pub bridge_name: String,
    /// Sequenced operations.
    pub ops: Vec<NetOp>,
}

/// One network-administration operation. The argv[0] is the
/// program (`ip` / `tc` / `nft`); `args` is everything after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetOp {
    /// Program name. Resolved on `PATH`.
    pub program: &'static str,
    /// Argument list. UTF-8 strings; we never need byte paths in
    /// `ip` / `tc` invocations.
    pub args: Vec<String>,
}

impl NetOp {
    fn ip(args: &[&str]) -> Self {
        Self {
            program: "ip",
            args: args.iter().map(|s| (*s).to_string()).collect(),
        }
    }
}

/// Translate a [`Bridge`] into a [`BridgePlan`].
///
/// The output captures four phases:
///
/// 1. Bridge creation (`ip link add … type bridge`, `ip link set … up`).
/// 2. Per-VM TAP creation + attach. The VM's TAP interface is named
///    `tap-<vm_name>`; QEMU is told to use that interface via
///    `-netdev tap,ifname=tap-<vm_name>` (slice 9.6 wiring).
/// 3. Whole-bridge impairments via `tc qdisc add … root netem`.
/// 4. Per-direction impairments via `tc qdisc add … parent`.
///
/// Partitions are not yet realised — they need iptables / nftables
/// rules and a more invasive plan. Slice 9.6.
pub fn plan_for(bridge: &Bridge) -> BridgePlan {
    let mut ops = Vec::new();
    let bridge_name = bridge.name().to_owned();

    // Phase 1: bridge interface.
    ops.push(NetOp::ip(&["link", "add", &bridge_name, "type", "bridge"]));
    ops.push(NetOp::ip(&["link", "set", "dev", &bridge_name, "up"]));

    // Phase 2: TAPs per attached VM.
    for vm in bridge.members() {
        let tap = tap_name_for_vm_on_bridge(&vm, &bridge_name);
        ops.push(NetOp::ip(&["tuntap", "add", "dev", &tap, "mode", "tap"]));
        ops.push(NetOp::ip(&["link", "set", "dev", &tap, "master", &bridge_name]));
        ops.push(NetOp::ip(&["link", "set", "dev", &tap, "up"]));
    }

    // Phase 3: whole-bridge impairments via netem on the bridge
    // interface itself (affects every member).
    let lat = bridge.latency_ms();
    let drop = bridge.drop_rate_pct();
    if lat > 0 || drop > 0 {
        let mut netem = vec![
            "qdisc".to_string(),
            "add".into(),
            "dev".into(),
            bridge_name.clone(),
            "root".into(),
            "netem".into(),
        ];
        if lat > 0 {
            netem.push("delay".into());
            netem.push(format!("{lat}ms"));
        }
        if drop > 0 {
            netem.push("loss".into());
            netem.push(format!("{drop}%"));
        }
        ops.push(NetOp {
            program: "tc",
            args: netem,
        });
    }

    // Phase 4: directional impairments. Each VM collapses its
    // outbound (from, *) pairs into a single per-TAP qdisc chain
    // — worst-case (max) across latency, drop, and bandwidth.
    //
    // Shape depends on which axes are non-zero:
    // - bandwidth only      → `htb default 10` root + one class
    // - latency/drop only   → `netem` root
    // - bandwidth + netem   → `htb default 10` root + class +
    //                          `netem` child on the class
    //
    // Both plan_for (this function) and the live realize_for_vm
    // path must emit the same sequence — divergence between them
    // is a bug the slice-9.5 tests catch.
    for vm in bridge.members() {
        let Some((latency_ms, drop_rate_pct, bandwidth_bps)) =
            bridge.collapsed_directional_for_pub(&vm)
        else {
            continue;
        };
        if latency_ms == 0 && drop_rate_pct == 0 && bandwidth_bps == 0 {
            continue;
        }
        let tap = tap_name_for_vm_on_bridge(&vm, &bridge_name);
        if bandwidth_bps > 0 {
            ops.push(NetOp {
                program: "tc",
                args: vec![
                    "qdisc".into(), "add".into(), "dev".into(), tap.clone(),
                    "root".into(), "handle".into(), "1:".into(), "htb".into(),
                    "default".into(), "10".into(),
                ],
            });
            let rate_str = format!("{bandwidth_bps}bit");
            let burst = (bandwidth_bps / 800).max(1500);
            ops.push(NetOp {
                program: "tc",
                args: vec![
                    "class".into(), "add".into(), "dev".into(), tap.clone(),
                    "parent".into(), "1:".into(), "classid".into(), "1:10".into(),
                    "htb".into(), "rate".into(), rate_str.clone(),
                    "ceil".into(), rate_str, "burst".into(), burst.to_string(),
                ],
            });
            if latency_ms > 0 || drop_rate_pct > 0 {
                let mut netem = vec![
                    "qdisc".to_string(),
                    "add".into(),
                    "dev".into(),
                    tap,
                    "parent".into(),
                    "1:10".into(),
                    "handle".into(),
                    "10:".into(),
                    "netem".into(),
                ];
                if latency_ms > 0 {
                    netem.push("delay".into());
                    netem.push(format!("{latency_ms}ms"));
                }
                if drop_rate_pct > 0 {
                    netem.push("loss".into());
                    netem.push(format!("{drop_rate_pct}%"));
                }
                ops.push(NetOp {
                    program: "tc",
                    args: netem,
                });
            }
        } else {
            let mut netem = vec![
                "qdisc".to_string(),
                "add".into(),
                "dev".into(),
                tap,
                "root".into(),
                "netem".into(),
            ];
            if latency_ms > 0 {
                netem.push("delay".into());
                netem.push(format!("{latency_ms}ms"));
            }
            if drop_rate_pct > 0 {
                netem.push("loss".into());
                netem.push(format!("{drop_rate_pct}%"));
            }
            ops.push(NetOp {
                program: "tc",
                args: netem,
            });
        }
    }

    BridgePlan { bridge_name, ops }
}

/// TAP interface name conventions: 11-char limit on Linux means we
/// can't always include the full VM name. Truncate at 10 (10 + 4-char
/// `tap-` prefix = 14 — Linux's IFNAMSIZ is actually 16 but sanity
/// margin). Should suffice for most names.
pub fn tap_name_for_vm(vm_name: &str) -> String {
    // Single-bridge form — kept for back-compat with the original
    // single-bridge layout. Multi-bridge callers MUST use
    // [`tap_name_for_vm_on_bridge`] instead, otherwise two NICs on
    // the same VM (one per bridge) collide on the same TAP name.
    let cleaned: String = vm_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let take = cleaned.len().min(10);
    format!("tap-{}", &cleaned[..take])
}

/// TAP name for a (vm, bridge) attachment pair. Linux IFNAMSIZ is
/// 16 bytes (15 chars + NUL). We need a name that is stable for
/// the same pair across runs *and* unique per pair so multi-homed
/// VMs (NIC-on-lan + NIC-on-mgmt) don't collide.
///
/// Format: `tap-<vm6><h7>` where `<vm6>` is the cleaned vm prefix
/// (≤ 6 chars) and `<h7>` is a 7-hex-char SipHash of `vm:bridge`.
/// Total length stays at ≤ 4 + 6 + 7 = 17 — wait, that's over.
/// → trim vm to 4 instead → 4 + 4 + 7 = 15. Within the limit and
/// the hash absorbs the rest of the disambiguation.
pub fn tap_name_for_vm_on_bridge(vm_name: &str, bridge_name: &str) -> String {
    let cleaned: String = vm_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let take = cleaned.len().min(4);
    // Use FNV-1a (64-bit) with the standard offset+prime so the
    // tap name is stable across Rust toolchain versions.
    // `DefaultHasher` doesn't promise that and a toolchain bump
    // would silently rename every TAP — breaking lab:restore (it
    // looks up TAPs by snapshot-recorded name) and any external
    // tooling that captured the names.
    let h = fnv1a_64(vm_name.as_bytes(), bridge_name.as_bytes());
    // 7 hex chars = 28 bits. With ~thousands of pairs the birthday
    // probability stays comfortably low.
    let hash_part = format!("{:07x}", h & 0x0FFF_FFFF);
    format!("tap-{}{}", &cleaned[..take], hash_part)
}

/// FNV-1a 64-bit over the concatenation `vm_name + ":" +
/// bridge_name`. Inlined (rather than pulling in a crate) so the
/// algorithm is pinned to this exact constant set forever.
fn fnv1a_64(a: &[u8], b: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x00000100000001B3;
    let mut h = OFFSET;
    for &byte in a.iter().chain(std::iter::once(&b':')).chain(b.iter()) {
        h ^= byte as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Spawn each [`NetOp`] in the plan via [`Command`]. Returns the
/// first failure with the command's stderr appended.
///
/// **Requires `CAP_NET_ADMIN`.** Tests that exercise this path
/// must run privileged or use the stub `RecordingRunner`.
pub fn run_plan(plan: &BridgePlan) -> std::io::Result<()> {
    for op in &plan.ops {
        let output = Command::new(op.program).args(&op.args).output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "{} {}: {}",
                op.program,
                op.args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bridge_just_creates_the_bridge_interface() {
        let b = Bridge::new("lan");
        let plan = plan_for(&b);
        assert_eq!(plan.bridge_name, "lan");
        assert_eq!(plan.ops.len(), 2);
        assert_eq!(plan.ops[0].args[0..3], ["link", "add", "lan"]);
        assert_eq!(plan.ops[1].args[0..3], ["link", "set", "dev"]);
    }

    #[test]
    fn each_attached_vm_gets_a_tap() {
        let b = Bridge::new("lan");
        b.attach("dc1");
        b.attach("dc2");
        let plan = plan_for(&b);
        let taps: Vec<String> = plan
            .ops
            .iter()
            .filter(|op| {
                op.program == "ip"
                    && op.args.first().map(|s| s.as_str()) == Some("tuntap")
            })
            .map(|op| op.args[3].clone())
            .collect();
        assert_eq!(
            taps,
            vec![
                tap_name_for_vm_on_bridge("dc1", "lan"),
                tap_name_for_vm_on_bridge("dc2", "lan"),
            ]
        );
    }

    #[test]
    fn whole_bridge_latency_emits_netem_qdisc() {
        let b = Bridge::new("lan");
        b.add_latency(100);
        let plan = plan_for(&b);
        let qdisc = plan
            .ops
            .iter()
            .find(|op| op.program == "tc")
            .expect("a tc op");
        assert!(qdisc.args.iter().any(|s| s == "netem"));
        assert!(qdisc.args.iter().any(|s| s == "delay"));
        assert!(qdisc.args.iter().any(|s| s == "100ms"));
    }

    #[test]
    fn directional_latency_emits_qdisc_on_source_tap() {
        let b = Bridge::new("lan");
        b.attach("a");
        b.attach("b");
        b.add_directional_latency("a", "b", 50);
        let plan = plan_for(&b);
        let source_tap = tap_name_for_vm_on_bridge("a", "lan");
        let directional = plan
            .ops
            .iter()
            .filter(|op| op.program == "tc")
            .find(|op| op.args.iter().any(|s| s == &source_tap))
            .expect("source-tap qdisc");
        assert!(directional.args.iter().any(|s| s == "delay"));
        assert!(directional.args.iter().any(|s| s == "50ms"));
    }

    #[test]
    fn directional_bandwidth_emits_htb_root_and_class() {
        let b = Bridge::new("lan");
        b.attach("a");
        b.attach("b");
        b.set_directional_bandwidth("a", "b", 1_000_000);
        let plan = plan_for(&b);
        let source_tap = tap_name_for_vm_on_bridge("a", "lan");
        // HTB root qdisc.
        let root = plan
            .ops
            .iter()
            .filter(|op| op.program == "tc")
            .find(|op| {
                op.args.iter().any(|s| s == "htb")
                    && op.args.iter().any(|s| s == "root")
                    && op.args.iter().any(|s| s == &source_tap)
            })
            .expect("htb root qdisc on source tap");
        assert!(root.args.iter().any(|s| s == "default"));
        assert!(root.args.iter().any(|s| s == "10"));
        // HTB class under it.
        let class = plan
            .ops
            .iter()
            .filter(|op| op.program == "tc")
            .find(|op| {
                op.args.first().map(|s| s.as_str()) == Some("class")
                    && op.args.iter().any(|s| s == &source_tap)
            })
            .expect("htb class on source tap");
        assert!(class.args.iter().any(|s| s == "1000000bit"));
        assert!(class.args.iter().any(|s| s == "burst"));
    }

    #[test]
    fn directional_bandwidth_plus_latency_emits_htb_with_netem_child() {
        let b = Bridge::new("lan");
        b.attach("a");
        b.attach("b");
        b.set_directional_bandwidth("a", "b", 500_000);
        b.add_directional_latency("a", "b", 25);
        let plan = plan_for(&b);
        let source_tap = tap_name_for_vm_on_bridge("a", "lan");
        // netem child should target parent 1:10, not root.
        let netem_child = plan
            .ops
            .iter()
            .filter(|op| op.program == "tc")
            .find(|op| {
                op.args.iter().any(|s| s == "netem")
                    && op.args.iter().any(|s| s == &source_tap)
                    && op.args.iter().any(|s| s == "1:10")
            })
            .expect("netem child of htb class");
        assert!(netem_child.args.iter().any(|s| s == "parent"));
        assert!(netem_child.args.iter().any(|s| s == "25ms"));
        // Must NOT have a `netem … root` qdisc on the same TAP.
        let has_netem_root = plan.ops.iter().any(|op| {
            op.program == "tc"
                && op.args.iter().any(|s| s == "netem")
                && op.args.iter().any(|s| s == "root")
                && op.args.iter().any(|s| s == &source_tap)
        });
        assert!(
            !has_netem_root,
            "must not emit a competing `netem … root` when htb owns root"
        );
    }

    #[test]
    fn directional_latency_only_keeps_netem_root_path() {
        // No bandwidth set → falls back to the legacy netem-root
        // shape. Regression guard against accidentally always using
        // HTB.
        let b = Bridge::new("lan");
        b.attach("a");
        b.attach("b");
        b.add_directional_latency("a", "b", 10);
        let plan = plan_for(&b);
        let source_tap = tap_name_for_vm_on_bridge("a", "lan");
        let netem = plan
            .ops
            .iter()
            .filter(|op| op.program == "tc")
            .find(|op| {
                op.args.iter().any(|s| s == &source_tap)
                    && op.args.iter().any(|s| s == "netem")
            })
            .expect("netem qdisc");
        assert!(netem.args.iter().any(|s| s == "root"));
        // HTB should NOT appear.
        let has_htb = plan.ops.iter().any(|op| {
            op.program == "tc" && op.args.iter().any(|s| s == "htb")
        });
        assert!(!has_htb, "no bandwidth set → no HTB");
    }

    #[test]
    fn directional_bandwidth_max_across_pairs() {
        // (a → b: 1M) and (a → c: 5M) collapse to the higher rate
        // on a's TAP (5M). "Worst case = loosest cap" so no
        // recorded pair is over-shaped.
        let b = Bridge::new("lan");
        b.attach("a");
        b.attach("b");
        b.attach("c");
        b.set_directional_bandwidth("a", "b", 1_000_000);
        b.set_directional_bandwidth("a", "c", 5_000_000);
        let plan = plan_for(&b);
        let source_tap = tap_name_for_vm_on_bridge("a", "lan");
        let class = plan
            .ops
            .iter()
            .filter(|op| op.program == "tc")
            .find(|op| {
                op.args.first().map(|s| s.as_str()) == Some("class")
                    && op.args.iter().any(|s| s == &source_tap)
            })
            .expect("htb class");
        assert!(class.args.iter().any(|s| s == "5000000bit"));
    }

    #[test]
    fn drop_rate_emits_loss_clause() {
        let b = Bridge::new("lan");
        b.drop_rate(7);
        let plan = plan_for(&b);
        let qdisc = plan
            .ops
            .iter()
            .find(|op| op.program == "tc")
            .expect("a tc op");
        assert!(qdisc.args.iter().any(|s| s == "loss"));
        assert!(qdisc.args.iter().any(|s| s == "7%"));
    }

    #[test]
    fn tap_name_truncates_long_vm_names() {
        assert_eq!(tap_name_for_vm("short"), "tap-short");
        assert_eq!(tap_name_for_vm("very-long-vm-name"), "tap-verylongvm");
    }

    #[test]
    fn tap_name_strips_special_chars() {
        assert_eq!(tap_name_for_vm("dc.1/x"), "tap-dc1x");
    }
}
