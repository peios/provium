//! Lua bindings for [`Bridge`].

use mlua::{MetaMethod, UserData, UserDataMethods, Value};

use crate::bridge::Bridge;

/// Lua-facing wrapper.
#[derive(Clone)]
pub(crate) struct BridgeUd {
    pub(crate) bridge: Bridge,
}

impl BridgeUd {
    pub(crate) fn wrap(bridge: Bridge) -> Self {
        Self { bridge }
    }

    /// Cheap clone of the underlying [`Bridge`] handle. Used by
    /// `lab:include(bridge_ud)` so the lab can register the same
    /// bridge under a new sibling.
    pub(crate) fn clone_bridge(&self) -> Bridge {
        self.bridge.clone()
    }
}

impl UserData for BridgeUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Membership. The Lua side accepts either a VmUd or a
        // bare string. When a VmUd is passed we also record the
        // attachment on the VM itself so its boot can produce a
        // matching `-netdev tap` flag.
        methods.add_method("attach", |_, this, vm: Value| {
            // DESIGN line 625: `bridge:attach(vm or [vms])`.
            // Accept a single VM/string OR an array of them so
            // `lan:attach({dc1, dc2})` works for atomic multi-VM
            // setup. Two-phase: validate every element FIRST so
            // a partial-attach can't leave half the table wired
            // when element N+1 turns out to be a bad type.
            fn commit_one(
                bridge: &crate::bridge::Bridge,
                v: &Value,
                name: String,
            ) {
                bridge.attach(name);
                if let Value::UserData(ud) = v {
                    if let Ok(vmud) = ud.borrow::<super::vm_ud::VmUd>() {
                        vmud.add_bridge_attachment(bridge.clone());
                    }
                }
            }
            if let Value::Table(ref t) = vm {
                let mut staged: Vec<(Value, String)> = Vec::new();
                for pair in t.clone().sequence_values::<Value>() {
                    let v = pair?;
                    let name = vm_name_from_value(&v)?;
                    staged.push((v, name));
                }
                for (v, name) in staged {
                    commit_one(&this.bridge, &v, name);
                }
                return Ok(());
            }
            let name = vm_name_from_value(&vm)?;
            commit_one(&this.bridge, &vm, name);
            Ok(())
        });
        methods.add_method("detach", |_, this, vm: Value| {
            let name = vm_name_from_value(&vm)?;
            // DESIGN says detach "triggers link-down on guest". If
            // the caller passed a VmUd we have the live VM handle
            // available — issue a QMP set_link(false) on the
            // matching netdev so the guest sees the link drop.
            // Mirrors nic:disconnect's pattern. Bare-string detach
            // is graph-state-only (no VM ref to drive QMP from).
            if let Value::UserData(ref ud) = vm {
                if let Ok(vmud) = ud.borrow::<super::vm_ud::VmUd>() {
                    let vm_ref = vmud.vm_clone();
                    let netdev_id = format!("{}-{}", name, this.bridge.name());
                    let _ = vm_ref.set_link(&netdev_id, false);
                }
            }
            this.bridge.detach(&name);
            Ok(())
        });
        methods.add_method("members", |_, this, ()| Ok(this.bridge.members()));

        // L3 routing — DESIGN documents this but the v1 impl is
        // graph-state only (no nft forwarding rules installed
        // between bridge subnets). Emit a one-shot warning so
        // tests calling route() expecting actual cross-bridge
        // reachability find out at the call site rather than
        // when packets silently fail to arrive.
        methods.add_method("route", |_, this, other: Value| {
            let name = match &other {
                Value::String(s) => s
                    .to_str()
                    .map(|s| s.to_string())
                    .map_err(mlua::Error::external)?,
                Value::UserData(ud) => {
                    if let Ok(b) = ud.borrow::<BridgeUd>() {
                        b.bridge.name().to_string()
                    } else {
                        return Err(mlua::Error::external(
                            "bridge:route: expected bridge userdata or string",
                        ));
                    }
                }
                Value::Table(t) => {
                    for v in t.clone().sequence_values::<Value>() {
                        let v = v?;
                        let n = match v {
                            Value::String(s) => s.to_str().map(|s| s.to_string())
                                .map_err(mlua::Error::external)?,
                            Value::UserData(ud) => ud.borrow::<BridgeUd>()
                                .map(|b| b.bridge.name().to_string())
                                .map_err(mlua::Error::external)?,
                            _ => return Err(mlua::Error::external(
                                "bridge:route: list entries must be bridge or string",
                            )),
                        };
                        this.bridge.route(&n);
                    }
                    warn_route_unimpl(this.bridge.name());
                    return Ok(());
                }
                _ => return Err(mlua::Error::external(
                    "bridge:route: expected bridge|string|list",
                )),
            };
            this.bridge.route(&name);
            warn_route_unimpl(this.bridge.name());
            Ok(())
        });

        // Partitions. Accept either `(a, b)` (symmetric) or
        // `({from=a, to=b})` (directional, blocks a→b only).
        // Pre-check that both endpoints are attached to this
        // bridge — without it, a partition rule on an unattached
        // VM realises onto a non-existent TAP and silently
        // does nothing, the kind of silent-pass that the
        // matrix audit kept catching.
        methods.add_method("partition", |_, this, args: mlua::Variadic<Value>| {
            let (a, b, directional) = parse_partition_args(&args, "partition")?;
            // Only check attachment for directional partitions —
            // those realise per-TAP nft rules that need the TAP
            // to exist. Symmetric partition is graph-state and
            // works on bare strings even before attach (DESIGN
            // allows the same for detach).
            if directional {
                check_endpoints_attached(&this.bridge, &a, &b, "partition")?;
                this.bridge.partition_directional(&a, &b);
            } else {
                this.bridge.partition(&a, &b);
            }
            Ok(())
        });
        methods.add_method("unpartition", |_, this, args: mlua::Variadic<Value>| {
            let (a, b, directional) = parse_partition_args(&args, "unpartition")?;
            // R9 lab-M2: don't enforce attachment for unpartition.
            // Unlike partition (which installs per-TAP nft rules
            // requiring the TAP to exist), unpartition only
            // removes a state entry. Detaching a VM and then
            // unpartitioning it is a valid lifecycle sequence —
            // detach() already clears any directional partition
            // entries for the VM, so this becomes a no-op rather
            // than an error.
            if directional {
                this.bridge.unpartition_directional(&a, &b);
            } else {
                this.bridge.unpartition(&a, &b);
            }
            Ok(())
        });
        methods.add_method("partition_all", |_, this, ()| {
            this.bridge.partition_all();
            Ok(())
        });
        methods.add_method("restore_all", |_, this, ()| {
            this.bridge.restore_all();
            Ok(())
        });
        methods.add_method("is_partitioned", |_, this, (a, b): (Value, Value)| {
            let a = vm_name_from_value(&a)?;
            let b = vm_name_from_value(&b)?;
            Ok(this.bridge.is_partitioned(&a, &b))
        });

        // Whole-bridge OR directional impairments. Per design,
        // each `bridge:add_latency(...)`-style call accepts either
        // a scalar (whole-bridge) or a table with `from`/`to` keys
        // (directional).
        methods.add_method("add_latency", |_, this, arg: Value| {
            let bridge = this.bridge.clone();
            apply_impairment(arg, "add_latency", "ms", |ms| {
                bridge.add_latency(ms);
                Ok(())
            }, |from, to, ms| {
                // R9 reg-2: directional impairments install
                // per-TAP netem qdiscs. Without an attachment
                // check the rule lands on a non-existent TAP and
                // silently does nothing — same footgun as
                // partition_directional (R8 #399).
                check_endpoints_attached(&bridge, &from, &to, "add_latency")?;
                bridge.add_directional_latency(&from, &to, ms);
                Ok(())
            })
        });
        methods.add_method("drop_rate", |_, this, arg: Value| {
            let bridge = this.bridge.clone();
            apply_impairment(arg, "drop_rate", "p", |pct| {
                bridge.drop_rate(pct);
                Ok(())
            }, |from, to, pct| {
                check_endpoints_attached(&bridge, &from, &to, "drop_rate")?;
                bridge.directional_drop_rate(&from, &to, pct);
                Ok(())
            })
        });
        methods.add_method("bandwidth_limit", |_, this, arg: Value| {
            apply_bandwidth(arg, |bps| {
                this.bridge.bandwidth_limit(bps);
                Ok(())
            }, |from, to, bps| {
                // Directional bandwidth: HTB-root with one class on
                // the source TAP, plus a netem child when latency
                // or drop_rate is also set for the same source.
                // Like directional latency/drop, the `to` argument
                // is graph-recorded but the realization shapes
                // *all* traffic leaving the source TAP — destinations
                // can't be selected at the L2 bridge layer with HTB
                // alone. See bridge.rs::set_directional_bandwidth
                // for the worst-case-merge semantics.
                this.bridge.set_directional_bandwidth(&from, &to, bps);
                Ok(())
            })
        });
        methods.add_method("reset", |_, this, ()| {
            this.bridge.reset();
            Ok(())
        });

        // Introspection.
        methods.add_method("name", |_, this, ()| Ok(this.bridge.name().to_owned()));
        methods.add_method("latency_ms", |_, this, ()| Ok(this.bridge.latency_ms()));
        methods.add_method("drop_rate_pct", |_, this, ()| Ok(this.bridge.drop_rate_pct()));
        methods.add_method("bandwidth_bps", |_, this, ()| Ok(this.bridge.bandwidth_bps()));

        // Construct a NIC bound to (this bridge, vm). When `vm` is
        // a VM userdata we carry its handle through so link-state
        // ops can reach QMP; when only a name string is supplied
        // the NIC is graph-state only.
        methods.add_method("nic", |_, this, vm: Value| {
            if let Value::UserData(ud) = &vm {
                if let Ok(vmud) = ud.borrow::<super::vm_ud::VmUd>() {
                    return Ok(super::nic_ud::NicUd::with_vm(
                        this.bridge.clone(),
                        vmud.vm_clone(),
                    ));
                }
            }
            let name = vm_name_from_value(&vm)?;
            Ok(super::nic_ud::NicUd::new(this.bridge.clone(), name))
        });

        // Capture a packet stream off the bridge by spawning
        // `tcpdump -i <bridge> -U -w -`. Returns a Stream userdata
        // that reads pcap-formatted bytes from tcpdump's stdout.
        // Requires `tcpdump` on PATH and CAP_NET_RAW (in addition to
        // the CAP_NET_ADMIN already required for the bridge itself).
        methods.add_method("capture", |lua, this, ()| {
            let site = super::result_ud::capture_creation_site(lua);
            let stream = super::capture_ud::CaptureUd::spawn_with_guard(
                &this.bridge,
                site,
            )
            .map_err(mlua::Error::external)?;
            super::result_ud::register_resource(lua, stream, "stream")
        });

        // Uplink controls — install/remove a NAT masquerade rule
        // between the bridge and the host's default-route interface.
        methods.add_method("enable_uplink", |_, this, ()| {
            this.bridge.enable_uplink().map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method("disable_uplink", |_, this, ()| {
            this.bridge.disable_uplink().map_err(mlua::Error::external)?;
            Ok(())
        });

        methods.add_method("routes", |_, this, ()| Ok(this.bridge.routes()));

        // isolate / unisolate.
        methods.add_method("isolate", |_, this, vm: Value| {
            let name = vm_name_from_value(&vm)?;
            this.bridge.isolate(&name);
            Ok(())
        });
        methods.add_method("unisolate", |_, this, vm: Value| {
            let name = vm_name_from_value(&vm)?;
            this.bridge.unisolate(&name);
            Ok(())
        });
        methods.add_method("is_isolated", |_, this, vm: Value| {
            let name = vm_name_from_value(&vm)?;
            Ok(this.bridge.is_isolated(&name))
        });

        // Directional impairments (already in Bridge struct).
        methods.add_method(
            "add_directional_latency",
            |_, this, opts: mlua::Table| {
                let from: String = opts.get("from")?;
                let to: String = opts.get("to")?;
                let ms: u32 = opts.get("ms")?;
                check_endpoints_attached(
                    &this.bridge, &from, &to, "add_directional_latency",
                )?;
                this.bridge.add_directional_latency(&from, &to, ms);
                Ok(())
            },
        );
        methods.add_method(
            "directional_drop_rate",
            |_, this, opts: mlua::Table| {
                let from: String = opts.get("from")?;
                let to: String = opts.get("to")?;
                let pct: u32 = opts.get("p")?;
                check_endpoints_attached(
                    &this.bridge, &from, &to, "directional_drop_rate",
                )?;
                this.bridge.directional_drop_rate(&from, &to, pct);
                Ok(())
            },
        );

        // Auto-close hook used by the resource-graph walker.
        // Tears down host-side networking; idempotent.
        methods.add_method("close", |_, this, ()| {
            let _ = this.bridge.unrealize();
            Ok(())
        });

        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!(
                "bridge({}; {} member(s))",
                this.bridge.name(),
                this.bridge.members().len()
            ))
        });
    }
}

/// Helper for `add_latency`/`drop_rate` which accept either a
/// scalar (whole-bridge effect) or a table `{from=…, to=…, <key>=N}`
/// (directional effect). `key` names the table field carrying the
/// magnitude (`ms` for latency, `p` for drop pct).
fn apply_impairment<W, D>(
    arg: Value,
    op: &'static str,
    key: &'static str,
    whole: W,
    directional: D,
) -> mlua::Result<()>
where
    W: FnOnce(u32) -> mlua::Result<()>,
    D: FnOnce(String, String, u32) -> mlua::Result<()>,
{
    match arg {
        Value::Integer(n) if n >= 0 => whole(n as u32),
        Value::Number(n) if n >= 0.0 => whole(n as u32),
        Value::Table(t) => {
            let from: String = t.get("from").map_err(|_| {
                mlua::Error::external(format!(
                    "bridge:{op}: directional form needs `from`"
                ))
            })?;
            let to: String = t.get("to").map_err(|_| {
                mlua::Error::external(format!(
                    "bridge:{op}: directional form needs `to`"
                ))
            })?;
            let n: u32 = t.get(key).map_err(|_| {
                mlua::Error::external(format!(
                    "bridge:{op}: directional form needs `{key}`"
                ))
            })?;
            directional(from, to, n)
        }
        other => Err(mlua::Error::external(format!(
            "bridge:{op}: expected int or table, got {}",
            other.type_name()
        ))),
    }
}

/// Bandwidth flavour of [`apply_impairment`] — the magnitude is
/// `u64` (bps) instead of `u32`.
fn apply_bandwidth<W, D>(arg: Value, whole: W, directional: D) -> mlua::Result<()>
where
    W: FnOnce(u64) -> mlua::Result<()>,
    D: FnOnce(String, String, u64) -> mlua::Result<()>,
{
    match arg {
        Value::Integer(n) if n >= 0 => whole(n as u64),
        Value::Number(n) if n >= 0.0 => whole(n as u64),
        Value::Table(t) => {
            let from: String = t.get("from")?;
            let to: String = t.get("to")?;
            let n: u64 = t.get("bps")?;
            directional(from, to, n)
        }
        other => Err(mlua::Error::external(format!(
            "bridge:bandwidth_limit: expected int or table, got {}",
            other.type_name()
        ))),
    }
}

/// Parse `partition`/`unpartition` arguments. Accepts:
/// * `(a, b)` — symmetric pair.
/// * `({from=a, to=b})` — single-table directional form.
///
/// Returns `(a, b, directional)`. The directional flag is reserved
/// for a future per-direction iptables/nft realisation; the graph
/// state currently treats both forms identically.
fn parse_partition_args(
    args: &mlua::Variadic<Value>,
    op: &'static str,
) -> mlua::Result<(String, String, bool)> {
    if args.len() == 1 {
        if let Value::Table(t) = &args[0] {
            let a: String = t.get("from").map_err(|_| {
                mlua::Error::external(format!("bridge:{op}: needs `from`"))
            })?;
            let b: String = t.get("to").map_err(|_| {
                mlua::Error::external(format!("bridge:{op}: needs `to`"))
            })?;
            return Ok((a, b, true));
        }
    }
    if args.len() < 2 {
        return Err(mlua::Error::external(format!(
            "bridge:{op}: expected (a, b) or ({{from=a, to=b}})"
        )));
    }
    let a = vm_name_from_value(&args[0])?;
    let b = vm_name_from_value(&args[1])?;
    Ok((a, b, false))
}

/// Resolve a Lua-side argument to a VM name. Accepts:
/// * a string (treat as the name directly)
/// * a [`super::vm_ud::VmUd`] (use its `vm.name()`)
fn vm_name_from_value(v: &Value) -> mlua::Result<String> {
    match v {
        Value::String(s) => Ok(s
            .to_str()
            .map_err(|e| e.to_string())
            .map_err(mlua::Error::external)?
            .to_string()),
        Value::UserData(ud) => {
            if let Ok(vm) = ud.borrow::<super::vm_ud::VmUd>() {
                return Ok(vm.vm_name());
            }
            Err(mlua::Error::external(
                "bridge expected a VM userdata or string",
            ))
        }
        other => Err(mlua::Error::external(format!(
            "bridge expected a VM userdata or string, got {}",
            other.type_name()
        ))),
    }
}

/// One-shot warning for `bridge:route` per bridge name. v1
/// records the route in graph state but installs no nft
/// forwarding rules — actual L3 reachability between bridge
/// subnets is deferred (would need per-bridge subnet tracking
/// + ip family forward rules). Print once so a test author
/// notices at the call site rather than chasing silently
/// dropped packets.
/// Refuse partition / unpartition calls naming a VM that's
/// not attached to this bridge. Without this guard the rule
/// realises onto a non-existent TAP — silent no-op that the
/// test author can't see.
fn check_endpoints_attached(
    bridge: &crate::bridge::Bridge,
    a: &str,
    b: &str,
    op: &str,
) -> mlua::Result<()> {
    let members = bridge.members();
    for vm in [a, b] {
        if !members.iter().any(|m| m == vm) {
            return Err(mlua::Error::external(format!(
                "bridge:{op}: vm `{vm}` is not attached to bridge `{}` \
                 (attached: {:?}). Call bridge:attach({vm:?}) first.",
                bridge.name(),
                members,
            )));
        }
    }
    Ok(())
}

fn warn_route_unimpl(bridge_name: &str) {
    use std::sync::Mutex;
    static WARNED: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let set = WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let mut g = set.lock().unwrap();
    if g.insert(bridge_name.to_owned()) {
        eprintln!(
            "provium: bridge:route on `{bridge_name}` is graph-state only in v1 \
             (no nft forward rules installed). Cross-bridge IP traffic will \
             not actually flow until the L3 routing slice lands."
        );
    }
}
