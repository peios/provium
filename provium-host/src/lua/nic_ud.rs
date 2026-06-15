//! NIC userdata. Slice-9-graph-state level — counters return zeros
//! and capture/disconnect/reconnect manipulate the bridge's
//! per-attachment state. Real netlink-driven NIC stats land in the
//! 9.6 follow-up.

use mlua::{UserData, UserDataMethods, Value};

use crate::bridge::Bridge;

/// Lua-facing NIC handle. Bound to a (bridge, vm) pair. Optionally
/// carries the parent [`crate::vm::Vm`] so `disconnect`/`reconnect`
/// can issue QMP `set_link` against the right netdev id rather than
/// only mutating bridge graph state.
#[derive(Clone)]
pub(crate) struct NicUd {
    bridge: Bridge,
    vm_name: String,
    vm: Option<crate::vm::Vm>,
}

impl NicUd {
    pub(crate) fn new(bridge: Bridge, vm_name: String) -> Self {
        Self { bridge, vm_name, vm: None }
    }

    /// Builder variant carrying the parent VM so link-state ops
    /// reach QMP.
    pub(crate) fn with_vm(bridge: Bridge, vm: crate::vm::Vm) -> Self {
        let vm_name = vm.name().to_owned();
        Self {
            bridge,
            vm_name,
            vm: Some(vm),
        }
    }
}

impl UserData for NicUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("counters", |lua, this, ()| {
            let table = lua.create_table()?;
            let tap = crate::bridge_realize::tap_name_for_vm_on_bridge(
                &this.vm_name,
                this.bridge.name(),
            );
            let read_stat = |key: &str| -> u64 {
                let path = format!("/sys/class/net/{tap}/statistics/{key}");
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(0)
            };
            // The TAP's `rx_*` is what the host *received from* the
            // guest — i.e. the guest's TX. Swap perspective so the
            // numbers match what `ip -s link` would print *inside*
            // the guest. DESIGN.md § NIC counters specifies
            // guest-perspective values.
            table.set("rx_bytes", read_stat("tx_bytes"))?;
            table.set("tx_bytes", read_stat("rx_bytes"))?;
            table.set("rx_packets", read_stat("tx_packets"))?;
            table.set("tx_packets", read_stat("rx_packets"))?;
            table.set(
                "errors",
                read_stat("rx_errors").saturating_add(read_stat("tx_errors")),
            )?;
            Ok(Value::Table(table))
        });

        // Per-tap packet capture. tcpdump on this NIC's host TAP
        // interface (`tap-<vm>` per the bridge_realize naming).
        methods.add_method("capture", |lua, this, ()| {
            // Pre-check: this VM's TAP must exist on the host.
            // bridge.is_realized() is true once ANY VM on the
            // bridge has booted; we need the per-VM check so a
            // multi-VM bridge with only some VMs booted doesn't
            // pass the gate for the un-booted ones.
            if !this.bridge.has_realized_tap(&this.vm_name) {
                return Err(mlua::Error::external(format!(
                    "nic:capture: vm `{}` has no TAP on bridge `{}` \
                     (not booted?). Call lab:boot() / vm:boot() first.",
                    this.vm_name,
                    this.bridge.name(),
                )));
            }
            let tap = crate::bridge_realize::tap_name_for_vm_on_bridge(
                &this.vm_name,
                this.bridge.name(),
            );
            let site = super::result_ud::capture_creation_site(lua);
            // tcpdump on the per-VM tap interface (not the whole
            // bridge) so the test author gets per-NIC traffic.
            // Capture-counter is still pinned to the bridge so the
            // snapshot precondition has a single rollup point.
            let stream = super::capture_ud::CaptureUd::spawn_on_iface_with_guard(
                &tap,
                &this.bridge,
                site,
            )
            .map_err(|e| mlua::Error::external(format!("nic capture: {e} ({tap})")))?;
            super::result_ud::register_resource(lua, stream, "stream")
        });

        methods.add_method("disconnect", |_, this, ()| {
            this.bridge.detach(&this.vm_name);
            if let Some(vm) = &this.vm {
                let netdev_id = format!("{}-{}", this.vm_name, this.bridge.name());
                vm.set_link(&netdev_id, false)
                    .map_err(mlua::Error::external)?;
            }
            Ok(())
        });
        methods.add_method("reconnect", |_, this, ()| {
            this.bridge.attach(this.vm_name.clone());
            if let Some(vm) = &this.vm {
                let netdev_id = format!("{}-{}", this.vm_name, this.bridge.name());
                vm.set_link(&netdev_id, true)
                    .map_err(mlua::Error::external)?;
            }
            Ok(())
        });
        methods.add_method("vm_name", |_, this, ()| Ok(this.vm_name.clone()));
        methods.add_method("bridge", |_, this, ()| {
            Ok(this.bridge.name().to_owned())
        });
    }
}
