//! Bounded runtime-disk ports. The caller serializes all QMP operations.
use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

use super::VmmError;

pub(super) const PORT_COUNT: usize = 8;

/// Keep internal port IDs clear of all launch disk/device/backend IDs.
pub(super) fn port_names<'a>(disk_ids: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut used = BTreeSet::new();
    for id in disk_ids {
        used.extend([id.to_owned(), format!("{id}-node"), format!("{id}-port")]);
    }
    (0..PORT_COUNT)
        .map(|i| {
            let mut name = format!("provium-hotplug-{i}");
            while !used.insert(name.clone()) {
                name.push('_');
            }
            name
        })
        .collect()
}

pub(super) struct DiskPorts {
    slots: Vec<(String, Option<String>)>,
}

#[derive(Deserialize)]
struct PciBus {
    devices: Vec<PciDevice>,
}
#[derive(Deserialize)]
struct PciBridge {
    #[serde(default)]
    devices: Vec<PciDevice>,
}
#[derive(Deserialize)]
struct PciDevice {
    qdev_id: String,
    pci_bridge: Option<PciBridge>,
}

fn live_devices(value: Value) -> Result<BTreeSet<String>, VmmError> {
    fn collect(devices: Vec<PciDevice>, ids: &mut BTreeSet<String>) {
        for device in devices {
            ids.insert(device.qdev_id);
            if let Some(bridge) = device.pci_bridge {
                collect(bridge.devices, ids);
            }
        }
    }
    let buses: Vec<PciBus> = serde_json::from_value(value)
        .map_err(|e| error(format!("invalid QMP PCI inventory: {e}")))?;
    let mut ids = BTreeSet::new();
    for bus in buses {
        collect(bus.devices, &mut ids);
    }
    Ok(ids)
}

fn error(message: String) -> VmmError {
    VmmError::Io(std::io::Error::other(message))
}

impl DiskPorts {
    pub(super) fn new(names: Vec<String>) -> Self {
        Self {
            slots: names.into_iter().map(|name| (name, None)).collect(),
        }
    }

    pub(super) fn attach(
        &mut self,
        disk_id: &str,
        image: &Path,
        readonly: bool,
        mut execute: impl FnMut(&str, Value) -> Result<Value, VmmError>,
    ) -> Result<(), VmmError> {
        // device_del is asynchronous. Only an authoritative PCI inventory
        // can release a port; a local detached flag or QMP ack cannot.
        let live = live_devices(execute("query-pci", json!({}))?)?;
        for (_, disk) in &mut self.slots {
            if let Some(id) = disk.as_ref() {
                if !live.contains(id) {
                    execute("blockdev-del", json!({"node-name": format!("{id}-node")}))?;
                    *disk = None;
                }
            }
        }
        let Some((port, occupied)) = self.slots.iter_mut().find(|(_, disk)| disk.is_none()) else {
            return Err(error(format!("no free runtime disk port (capacity {PORT_COUNT}); wait for guest removal before attaching another disk")));
        };
        let node = format!("{disk_id}-node");
        execute(
            "blockdev-add",
            json!({
                "driver": "raw", "node-name": node, "read-only": readonly,
                "file": {"driver": "file", "filename": image.to_string_lossy()},
            }),
        )?;
        // Reserve immediately so a failed device_add + failed rollback does
        // not lose ownership of the block node. The next attach retries cleanup.
        *occupied = Some(disk_id.to_owned());
        if let Err(add) = execute(
            "device_add",
            json!({
                "driver": "virtio-blk-pci", "drive": node, "id": disk_id, "bus": port,
            }),
        ) {
            // Keep terminal errors typed so Vm marks a lost control plane dead.
            if control_lost(&add) {
                return Err(add);
            }
            match execute("blockdev-del", json!({"node-name": node})) {
                Ok(_) => *occupied = None,
                Err(cleanup) => {
                    if control_lost(&cleanup) {
                        return Err(cleanup);
                    }
                    return Err(error(format!(
                        "{add}; cleaning up runtime disk `{disk_id}` also failed: {cleanup}"
                    )));
                }
            }
            return Err(add);
        }
        Ok(())
    }
}

fn control_lost(error: &VmmError) -> bool {
    use provium_qmp::QmpError;
    matches!(
        error,
        VmmError::Qmp(QmpError::Timeout(_) | QmpError::Io(_) | QmpError::Closed(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Monitor {
        live: BTreeSet<String>,
        nodes: BTreeSet<String>,
        calls: Vec<(String, Value)>,
        fail_add: bool,
        fail_cleanup: bool,
    }
    impl Monitor {
        fn execute(&mut self, name: &str, args: Value) -> Result<Value, VmmError> {
            self.calls.push((name.into(), args.clone()));
            match name {
                "query-pci" => {
                    return Ok(json!([{"devices": [{"qdev_id": "port", "pci_bridge": {
                        "devices": self.live.iter().map(|id| json!({"qdev_id": id})).collect::<Vec<_>>()
                    }}]}]))
                }
                "blockdev-add" => {
                    if !self
                        .nodes
                        .insert(args["node-name"].as_str().unwrap().into())
                    {
                        return Err(error("duplicate node".into()));
                    }
                }
                "device_add" => {
                    if self.fail_add {
                        return Err(error("device rejected".into()));
                    }
                    assert!(args["bus"]
                        .as_str()
                        .unwrap()
                        .starts_with("provium-hotplug-"));
                    assert!(self.live.insert(args["id"].as_str().unwrap().into()));
                }
                "blockdev-del" => {
                    if self.fail_cleanup {
                        return Err(error("cleanup rejected".into()));
                    }
                    assert!(self.nodes.remove(args["node-name"].as_str().unwrap()));
                }
                _ => panic!("unexpected command {name}"),
            }
            Ok(json!({}))
        }
        fn attach(&mut self, ports: &mut DiskPorts, id: &str) -> Result<(), VmmError> {
            ports.attach(id, Path::new("/test.img"), true, |n, a| self.execute(n, a))
        }
    }
    fn ports() -> DiskPorts {
        DiskPorts::new(port_names(std::iter::empty()))
    }

    #[test]
    fn capacity_waits_for_real_removal_then_reuses_the_port_and_id() {
        let mut p = ports();
        let mut q = Monitor::default();
        for i in 0..PORT_COUNT {
            q.attach(&mut p, &format!("disk{i}")).unwrap();
        }
        let before = q.calls.len();
        assert!(q
            .attach(&mut p, "extra")
            .unwrap_err()
            .to_string()
            .contains("capacity 8"));
        // A guest that has not completed unplug still owns every port: no
        // block node or device may be added merely because detach was requested.
        assert_eq!(q.calls.len(), before + 1);
        q.live.remove("disk0");
        q.calls.clear();
        q.attach(&mut p, "disk0").unwrap();
        assert_eq!(
            q.calls.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["query-pci", "blockdev-del", "blockdev-add", "device_add"]
        );
        assert_eq!(q.calls[3].1["bus"], "provium-hotplug-0");
        assert_eq!(q.calls[2].1["read-only"], true);
        assert_eq!(q.calls[2].1["file"]["filename"], "/test.img");
    }

    #[test]
    fn rejected_device_rolls_back_its_node_without_spending_capacity() {
        let mut p = ports();
        let mut q = Monitor {
            fail_add: true,
            ..Default::default()
        };
        assert!(q
            .attach(&mut p, "disk")
            .unwrap_err()
            .to_string()
            .contains("device rejected"));
        assert!(q.nodes.is_empty());
        assert!(p.slots.iter().all(|(_, d)| d.is_none()));
        q.fail_add = false;
        q.attach(&mut p, "disk").unwrap();
    }

    #[test]
    fn failed_rollback_is_reported_and_retried_before_new_attachment() {
        let mut p = ports();
        let mut q = Monitor {
            fail_add: true,
            fail_cleanup: true,
            ..Default::default()
        };
        let err = q.attach(&mut p, "disk").unwrap_err().to_string();
        assert!(err.contains("device rejected") && err.contains("cleanup rejected"));
        assert!(q.nodes.contains("disk-node"));
        q.fail_add = false;
        q.calls.clear();
        assert!(q.attach(&mut p, "next").is_err());
        assert_eq!(q.calls.len(), 2); // query then failed cleanup, never a new node
        q.fail_cleanup = false;
        q.attach(&mut p, "next").unwrap();
        assert!(!q.nodes.contains("disk-node"));
    }

    #[test]
    fn malformed_inventory_cannot_release_an_occupied_port() {
        let mut p = ports();
        let mut q = Monitor::default();
        q.attach(&mut p, "disk").unwrap();
        let mut calls = 0;
        assert!(p
            .attach("next", Path::new("/test.img"), false, |name, _| {
                calls += 1;
                assert_eq!(name, "query-pci");
                Ok(json!([{"devices": [{"unexpected": "shape"}]}]))
            })
            .is_err());
        assert_eq!(calls, 1);
        assert_eq!(p.slots[0].1.as_deref(), Some("disk"));
    }

    #[test]
    fn terminal_device_add_failure_remains_a_terminal_qmp_error() {
        let mut p = ports();
        let mut q = Monitor::default();
        let err = p
            .attach("disk", Path::new("/test.img"), false, |n, a| {
                if n == "device_add" {
                    return Err(VmmError::Qmp(provium_qmp::QmpError::Closed("test")));
                }
                q.execute(n, a)
            })
            .unwrap_err();
        assert!(control_lost(&err));
        assert!(q.nodes.contains("disk-node"));
        assert!(!q.calls.iter().any(|(n, _)| n == "blockdev-del"));
    }

    #[test]
    fn internal_port_names_do_not_collide_with_launch_disks() {
        let names = port_names(["provium-hotplug-0", "provium-hotplug-0_"].into_iter());
        assert_eq!(names.len(), PORT_COUNT);
        assert_eq!(names[0], "provium-hotplug-0__");
    }
}
