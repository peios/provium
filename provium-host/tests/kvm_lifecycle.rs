//! VM lifecycle against a real machine: `vm:reset()` and
//! `vm:power_button()`.
//!
//! Why this file exists: PEI-1112 shipped three separate faults in
//! `vm:reset()` and nothing in the suite could have caught any of them.
//! Every `conformance_*.rs` routes Lua through `LocalAgentVmm`, which
//! has no machine to reset, so `reset_from_booted_succeeds` is
//! `#[ignore]`d and asserts only `vm:state() == "booted"` — an
//! assertion that stayed **true** while the VM was destroyed, silent,
//! or permanently unusable. That weak assertion is not merely
//! insufficient; it is the reason the bug shipped (PEI-1119).
//!
//! So this lane drives `QemuVmm` directly, the way `nbd_qemu.rs` drives
//! real QEMU binaries, and skips loudly when a prerequisite is absent
//! rather than failing.
//!
//! # What it needs, and why it skips rather than builds
//!
//! A real boot needs a composed Peios image, which provium does not
//! carry: the profiles live in the `peios-integration-tests` repository
//! and their artifacts are produced by `provium prepare`. This lane
//! uses those artifacts when they are already built and **will not**
//! compose one. A `cargo test` that silently kicked off a full image
//! compose would be a bad surprise, and a very slow one.
//!
//! Override the config with `PROVIUM_KVM_CONFIG=<provium.toml>`, and
//! the profile with `PROVIUM_KVM_PROFILE` (default `peinit`).
//!
//! # No capability is required
//!
//! `CAP_NET_ADMIN` is only needed once a bridge or TAP is realised, and
//! `scheduler::preflight` says so explicitly; nothing here attaches a
//! NIC. So unlike the `provium` binary — whose pre-flight demands it up
//! front — this lane needs no `setcap`, which is what makes it usable
//! from a plain `cargo test`.

#![cfg(feature = "lua")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use provium_host::lua::{run_file, FileOutcome, TestStatus};
use provium_host::profile::Config;
use provium_host::vmm::qemu::QemuVmm;
use provium_host::vmm::Vmm;

/// Everything the lane needs, or the reason it cannot run.
enum Prereq {
    Ready {
        config: Arc<Config>,
        profile: String,
    },
    Skip(String),
}

fn kvm_present() -> bool {
    Path::new("/dev/kvm").exists()
}

fn qemu_present() -> bool {
    Command::new("qemu-system-x86_64")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The `provium.toml` to boot from.
///
/// Falls back to the conventional sibling checkout, because provium
/// carries no profiles of its own: without it the lane would skip
/// everywhere by default, including on the machine that just changed
/// the code it is meant to cover.
fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PROVIUM_KVM_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let sibling = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()? // provium/
        .parent()? // the workspace root holding both checkouts
        .join("peios-integration-tests/provium.toml");
    sibling.exists().then_some(sibling)
}

fn prereqs() -> Prereq {
    if !kvm_present() {
        return Prereq::Skip("/dev/kvm is not present".into());
    }
    if !qemu_present() {
        return Prereq::Skip("qemu-system-x86_64 is not installed".into());
    }
    let Some(path) = config_path() else {
        return Prereq::Skip(
            "no provium.toml found beside this checkout — set PROVIUM_KVM_CONFIG".into(),
        );
    };
    let mut config = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => return Prereq::Skip(format!("could not load {}: {e}", path.display())),
    };
    config.expand_build_outputs();

    let name = std::env::var("PROVIUM_KVM_PROFILE").unwrap_or_else(|_| "peinit".into());
    let Some(profile) = config.profile(&name) else {
        return Prereq::Skip(format!("profile `{name}` is not in {}", path.display()));
    };

    // Each artifact is named on its own. "the image was never built"
    // and "the built image has no kernel in it" are different problems
    // with different fixes, and a lane that reported only "skipped"
    // would hide both behind the same silence.
    if let Err(e) = profile.resolve_kernel() {
        return Prereq::Skip(format!(
            "profile `{name}` has no bootable kernel ({e}) — run `provium prepare {name}`"
        ));
    }
    if !profile.initrd.exists() {
        return Prereq::Skip(format!(
            "initrd `{}` is not built — run `provium prepare {name}`",
            profile.initrd.display()
        ));
    }
    for disk in &profile.disks {
        if !disk.path.exists() {
            return Prereq::Skip(format!(
                "disk `{}` is not built — run `provium prepare {name}`",
                disk.path.display()
            ));
        }
    }

    Prereq::Ready {
        config: Arc::new(config),
        profile: name,
    }
}

/// Run one Lua source against a real machine, or say why it did not.
///
/// `None` means skipped, and the reason has already been printed.
fn run_or_skip(source: &str) -> Option<FileOutcome> {
    let (config, profile) = match prereqs() {
        Prereq::Ready { config, profile } => (config, profile),
        Prereq::Skip(why) => {
            eprintln!("skipping: {why}");
            return None;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kvm.test.lua");
    // The profile name is the one thing the Lua cannot hard-code, since
    // it is selectable. Substituted rather than parameterised so the
    // body stays readable as the Lua a user would actually write.
    let disk_image = dir.path().join("hotplug.raw");
    if source.contains("{disk_image}") {
        std::fs::File::create(&disk_image)
            .unwrap()
            .set_len(17 * 1024 * 1024)
            .unwrap();
    }
    std::fs::write(
        &path,
        source.replace("{profile}", &profile)
            .replace("{disk_image}", &disk_image.to_string_lossy()),
    ).unwrap();

    let vmm: Arc<dyn Vmm> = Arc::new(QemuVmm::new());
    Some(run_file(&path, config, vmm).expect("running the test file"))
}

fn assert_all_passed(outcome: &FileOutcome) {
    assert!(!outcome.tests.is_empty(), "no test ran");
    for t in &outcome.tests {
        assert_eq!(
            t.status,
            TestStatus::Passed,
            "test `{}` failed: {:?}",
            t.name,
            t.message,
        );
    }
}

#[test]
fn suspend_query_distinguishes_running_from_host_pause() {
    let Some(outcome) = run_or_skip(r#"
test("query suspend state without changing lifecycle", function(t)
    local vm = provium:vm("query-suspend", "{profile}"):boot()
    t:assert_eq(vm:is_suspended(), false)
    vm:pause()
    local ok, err = pcall(vm.is_suspended, vm)
    t:assert(not ok and tostring(err):find("paused"), tostring(err))
    vm:resume()
    t:assert_eq(vm:is_suspended(), false)
    vm:shutdown()
    local closed, why = pcall(vm.is_suspended, vm)
    t:assert(not closed and tostring(why):find("shutdown"), tostring(why))
end)
"#) else { return; };
    assert_all_passed(&outcome);
}

/// `vm:reset()` now returns only once the guest is serving its agent
/// again, so the test is simply: reset, then use the VM. No polling.
///
/// That the polling is gone is the point. A version that waited for the
/// agent would let a half-fixed reset pass as long as the guest
/// eventually recovered — the same weakness that let the bug ship.
const RESET_TEST: &str = r#"
provium:claim({ memory = 1124 * 1024 * 1024, cpus = 1 })

--- Occurrences of a plain (non-pattern) needle.
local function count(haystack, needle)
    local n, pos = 0, 1
    while true do
        local i = haystack:find(needle, pos, true)
        if not i then break end
        n, pos = n + 1, i + 1
    end
    return n
end

-- Printed once per boot. It appears TWICE per boot on this profile:
-- the profile sets `console=hvc0` and provium appends `console=ttyS0`,
-- so guest output reaches the captured console by two routes. What
-- matters is that the count GROWS, never its absolute value.
local MARK = "peinit: phase1 starting"

local vm = provium:vm("r", "{profile}", { memory = "1G", cpus = 1 })
vm:boot({ disks = { { scratch = "16M", id = "state" } } })
vm:console():expect("peinit: phase2 boot complete", 120)

test("vm:reset() reboots the guest and returns it ready to use", function(t)
    -- The agent must answer BEFORE the reset, or "it answered after"
    -- would prove nothing.
    local before_ok = pcall(function() return vm:run("true") end)
    t:assert(before_ok, "the agent must answer before the reset for this "
        .. "test to mean anything")

    local before = count(vm:console():read_log(), MARK)

    local ok, err = pcall(function() vm:reset() end)
    t:assert(ok, "vm:reset() errored: " .. tostring(err))

    -- No polling, deliberately. reset() is documented to return with the
    -- guest serving its agent, so this must succeed on the first attempt.
    local ran, r = pcall(function() return vm:run("true") end)
    t:assert(ran, "the agent did not answer immediately after vm:reset() "
        .. "returned — reset() is supposed to wait for it. state="
        .. tostring(vm:state()))
    t:assert(r.exit_code == 0,
        ("a command after the reset exited %d"):format(r.exit_code))

    -- And the guest really did re-run its boot path, rather than the
    -- agent connection simply having survived.
    local after = count(vm:console():read_log(), MARK)
    t:assert(after > before, ("the guest did not reboot: boot markers "
        .. "stayed at %d. The console accumulates across a reset now "
        .. "(logappend=on), so an unchanged count is real."):format(after))

    t:assert_eq(vm:state(), "booted")
end)
"#;

/// The power button gets the same treatment, and for the same reason:
/// `power_button_from_booted_succeeds` is `#[ignore]`d and asserts only
/// that the state field reads `shutdown`.
const POWER_BUTTON_TEST: &str = r#"
provium:claim({ memory = 1124 * 1024 * 1024, cpus = 1 })

local vm = provium:vm("p", "{profile}", { memory = "1G", cpus = 1 })
vm:boot()
vm:console():expect("peinit: phase2 boot complete", 120)

test("vm:power_button() really does shut the guest down", function(t)
    local before = pcall(function() return vm:run("true") end)
    t:assert(before, "the agent must answer before the power button, or "
        .. "'it stopped answering' afterwards would prove nothing")

    vm:power_button()

    -- ACPI is a request, and the guest runs a real shutdown sequence, so
    -- it is given time — but it must genuinely end. Measured at ~11s on
    -- this profile; 60 is a backstop, not a working budget.
    local gone = false
    for _ = 1, 60 do
        if not pcall(function() return vm:run("true") end) then
            gone = true
            break
        end
        pcall(function() vm:clock():sleep("1s") end)
    end
    t:assert(gone, "the agent was still answering 60s after the power "
        .. "button; state=" .. tostring(vm:state()))

    -- Console evidence independently confirms the guest completed its
    -- shutdown sequence rather than merely becoming unreachable.
    local log = vm:console():read_log()
    t:assert(log:find("reboot: Power down", 1, true),
        "the kernel never reported powering down, so the guest did not "
        .. "complete a graceful shutdown")
end)
"#;

/// PEI-1120: completion, state, console evidence and reservation release.
const POWER_BUTTON_STATE_TEST: &str = r#"
provium:claim({ memory = 1124 * 1024 * 1024, cpus = 1 })

local vm = provium:vm("p", "{profile}", { memory = "1G", cpus = 1 })
vm:boot()
vm:console():expect("peinit: phase2 boot complete", 120)

test("vm:power_button() leaves the VM in `shutdown`", function(t)
    vm:power_button()
    -- VM reference, `vm:power_button()`: "Post-call state is `Shutdown`."
    t:assert_eq(vm:state(), "shutdown")
    t:assert(vm:console():read_log():find("reboot: Power down", 1, true))
    t:assert(not pcall(function() vm:run("true") end))
    t:assert_eq(vm:state(), "shutdown", "an agent call must not relabel poweroff as dead")
    vm:shutdown() -- idempotent after confirmed poweroff
    -- The first VM released the entire one-VM claim on successful return.
    local next_vm = provium:vm("next", "{profile}", { memory = "1G", cpus = 1 })
    next_vm:boot()
    next_vm:run("true"):assert_ok()
    next_vm:shutdown()
end)
"#;

#[test]
fn reset_returns_a_guest_that_is_serving_its_agent_again() {
    let Some(outcome) = run_or_skip(RESET_TEST) else {
        return;
    };
    assert_all_passed(&outcome);
}

#[test]
fn the_power_button_shuts_the_guest_down() {
    let Some(outcome) = run_or_skip(POWER_BUTTON_TEST) else {
        return;
    };
    assert_all_passed(&outcome);
}

#[test]
fn the_power_button_leaves_the_vm_in_shutdown() {
    let Some(outcome) = run_or_skip(POWER_BUTTON_STATE_TEST) else {
        return;
    };
    assert_all_passed(&outcome);
}

// PEI-1103: checking the local detached flag alone misses a failed
// device_del. Observe the block device disappear inside the real guest.
const LAUNCH_DISK_DETACH_TEST: &str = r#"
provium:claim({ memory = 1124 * 1024 * 1024, cpus = 1 })
local vm = provium:vm("detach", "{profile}", { memory = "1G", cpus = 1 })
vm:boot({ disks = { { scratch = "17M", id = "detachable" } } })
vm:console():expect("peinit: phase2 boot complete", 120)

test("detaching a launch disk removes its guest block device", function(t)
    -- Identify the scratch disk by size, independent of profile disk count.
    local found = vm:run([[for p in /sys/class/block/vd*; do
        if [ "$(cat "$p/size")" = 34816 ]; then basename "$p"; fi
    done]])
    t:assert_eq(found.exit_code, 0)
    local name = found.stdout:gsub("%s+$", "")
    t:assert(name:match("^vd%a+$"), "expected one 17 MiB scratch disk: " .. name)
    local disk = vm:disk("detachable")
    disk:detach()
    t:assert(disk:is_detached())
    local gone = false
    for _ = 1, 100 do
        if vm:run("test ! -e /sys/class/block/" .. name).exit_code == 0 then
            gone = true
            break
        end
        vm:clock():sleep("100ms")
    end
    t:assert(gone, "device_del did not remove launch disk " .. name)
    t:assert_eq(vm:run("true").exit_code, 0, "guest remains usable after detach")
end)
"#;

#[test]
fn detaching_a_launch_disk_removes_the_guest_device() {
    let Some(outcome) = run_or_skip(LAUNCH_DISK_DETACH_TEST) else {
        return;
    };
    assert_all_passed(&outcome);
}

// PEI-1430: the QMP success and the guest's device inventory must agree.
const HOT_DISK_TEST: &str = r#"
provium:claim({ memory = 1124 * 1024 * 1024, cpus = 1 })
local vm = provium:vm("hotdisk", "{profile}", { memory = "1G", cpus = 1 })
vm:boot()
vm:console():expect("peinit: phase2 boot complete", 120)

test("a runtime disk appears in the guest and disappears after detach", function(t)
    local disk = vm:attach_disk({id="hotdisk", size=17 * 1024 * 1024, image="{disk_image}"})
    local name = ""
    for _ = 1, 100 do
        local found = vm:run([[for p in /sys/class/block/vd*; do
            if [ "$(cat "$p/size")" = 34816 ]; then basename "$p"; fi
        done]])
        t:assert_eq(found.exit_code, 0)
        name = found.stdout:gsub("%s+$", "")
        if name:match("^vd%a+$") then break end
        vm:clock():sleep("100ms")
    end
    t:assert(name:match("^vd%a+$"), "runtime disk did not appear in guest: " .. name)
    disk:detach()
    local gone = false
    for _ = 1, 100 do
        if vm:run("test ! -e /sys/class/block/" .. name).exit_code == 0 then
            gone = true
            break
        end
        vm:clock():sleep("100ms")
    end
    t:assert(gone, "runtime disk remained after detach")
    t:assert_eq(vm:run("true").exit_code, 0)
end)
"#;

#[test]
fn runtime_disk_appears_and_disappears_in_the_guest() {
    let Some(outcome) = run_or_skip(HOT_DISK_TEST) else {
        return;
    };
    assert_all_passed(&outcome);
}

const HOT_DISK_CAPACITY_TEST: &str = r#"
provium:claim({ memory = 1124 * 1024 * 1024, cpus = 1 })
local vm = provium:vm("capacity", "{profile}", { memory = "1G", cpus = 1 })
vm:boot()
vm:console():expect("peinit: phase2 boot complete", 120)

local function count_disks()
    local r = vm:run([[n=0; for p in /sys/class/block/vd*; do
        if [ "$(cat "$p/size")" = 34816 ]; then n=$((n+1)); fi
    done; echo "$n"]])
    assert(r.exit_code == 0, r.stderr)
    return tonumber(r.stdout)
end
local function wait_count(t, want)
    for _ = 1, 100 do
        if count_disks() == want then return end
        vm:clock():sleep("100ms")
    end
    t:assert_eq(count_disks(), want, "unexpected runtime disk inventory")
end
local function attach(id)
    return vm:attach_disk({id=id, size=17 * 1024 * 1024, image="{disk_image}", readonly=true})
end

test("runtime disk capacity is bounded and an unplugged port can be reused", function(t)
    local disks = {}
    for i = 1, 8 do disks[i] = attach("hot" .. i) end
    wait_count(t, 8)
    local ok, err = pcall(function() attach("overflow") end)
    t:assert(not ok and tostring(err):find("capacity 8", 1, true), tostring(err))
    -- The guest cannot complete removal while paused. Requesting detach
    -- must not let a ninth device borrow an occupied port.
    vm:pause()
    disks[1]:detach()
    ok, err = pcall(function() attach("overflow") end)
    t:assert(not ok and tostring(err):find("capacity 8", 1, true), tostring(err))
    vm:resume()
    wait_count(t, 7)
    for _ = 1, 100 do
        ok, err = pcall(function() return attach("overflow") end)
        if ok then break end
        t:assert(tostring(err):find("capacity 8", 1, true), tostring(err))
        vm:clock():sleep("100ms")
    end
    t:assert(ok, tostring(err))
    wait_count(t, 8)
    t:assert_eq(vm:state(), "booted", "capacity error must not mark the VM dead")
    t:assert_eq(vm:run("true").exit_code, 0)
end)
"#;

#[test]
fn runtime_disk_ports_wait_for_guest_removal_and_are_reusable() {
    let Some(outcome) = run_or_skip(HOT_DISK_CAPACITY_TEST) else {
        return;
    };
    assert_all_passed(&outcome);
}
