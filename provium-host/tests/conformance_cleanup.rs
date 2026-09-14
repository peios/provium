//! Conformance: auto-close ordering and the documented scope rules.
//! Test-scope resources die at test
//! end; file-scope resources survive across tests in the same
//! file but die at file end.

#![cfg(feature = "lua")]

mod common;

use common::{assert_all_passed, run_local_lua};

#[test]
fn automatic_cleanup_emits_one_shutdown_per_booted_vm() {
    use provium_host::{
        lab::Lab,
        scheduler::EventSink,
        vmm::{local_agent::LocalAgentVmm, Vmm},
    };
    use provium_protocol::events::Event;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Events(Mutex<Vec<Event>>);
    impl EventSink for Events {
        fn emit(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleanup.test.lua");
    std::fs::write(
        &path,
        r#"
local shared = provium:vm("shared", "peios"):boot()
local never_booted = provium:vm("never", "peios")
test("implicit close", function(t)
    provium:vm("temporary", "peios"):boot()
end)
test("explicit close", function(t)
    local vm = provium:vm("explicit", "peios"):boot()
    vm:shutdown()
    vm:close()
end)
"#,
    )
    .unwrap();
    let config = common::default_config();
    let vmm: Arc<dyn Vmm> = Arc::new(LocalAgentVmm::new());
    let lab = Lab::new("file", Arc::clone(&config), Arc::clone(&vmm));
    let sink = Arc::new(Events::default());
    let outcome =
        provium_host::lua::run_file_with_lab_and_sink(&path, config, vmm, lab, sink.clone())
            .unwrap();
    assert_all_passed(&outcome);
    let events = sink.0.lock().unwrap();
    let mut spawned: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::VmSpawned(e) => Some((e.file.clone(), e.vm_name.clone())),
            _ => None,
        })
        .collect();
    let mut shut: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::VmShutdown(e) => Some((e.file.clone(), e.vm_name.clone())),
            _ => None,
        })
        .collect();
    spawned.sort();
    shut.sort();
    assert_eq!(spawned.len(), 3);
    assert_eq!(shut, spawned);
}

#[test]
fn streams_auto_close_at_test_end() {
    // After test() returns, any test-scope stream should be
    // closed automatically. Next test starts clean.
    let outcome = run_local_lua(
        r#"
local vm = provium:vm("a", "peios"):boot()
local stream
test("test-1 opens stream", function(t)
    local tmp = os.tmpname()
    local f = io.open(tmp, "w"); f:write(""); f:close()
    -- Open + leak intentionally — auto-close should reap it.
    stream = vm:tail_file(tmp)
    os.remove(tmp)
end)

test("test-2 sees clean slate", function(t)
    -- A leaked stream from test-1 would block lab snapshot
    -- preconditions; surface as a basic operation working.
    local vm = provium:vm("a")
    t:assert_eq(vm:state(), "booted")
    t:assert(stream:eof(), "test-scope stream must be closed")
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn workers_auto_close_at_test_end() {
    let outcome = run_local_lua(
        r#"
local vm = provium:vm("a", "peios"):boot()
local worker
test("worker leak", function(t)
    worker = vm:spawn_worker()
    -- intentionally don't close
end)

test("subsequent test", function(t)
    local vm = provium:vm("a")
    t:assert_eq(vm:state(), "booted")
    local ok = pcall(function() worker:run("true") end)
    t:assert(not ok, "test-scope worker must be closed")
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn vms_survive_across_tests_in_same_file() {
    // File-scope VMs survive; test-scope VMs are independent.
    let outcome = run_local_lua(
        r#"
local shared = provium:vm("a", "peios"):boot()
local scratch
test("test-1 boots", function(t)
    scratch = provium:vm("scratch", "peios"):boot()
end)

test("test-2 finds same VM", function(t)
    -- VM "a" must still be reachable; not auto-shutdown.
    local vm = provium:vm("a")
    t:assert_eq(vm:state(), "booted")
    t:assert_eq(scratch:state(), "shutdown")
    t:assert(provium.scratch == nil)
    t:assert_eq(provium:vm("scratch", "peios"):boot():state(), "booted")
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn bridges_survive_across_tests_in_same_file() {
    let outcome = run_local_lua(
        r#"
local shared = provium:bridge("lan", {})
test("create bridge", function(t)
    provium:bridge("temporary", {})
end)

test("bridge still here", function(t)
    local lan = provium:bridge("lan")
    t:assert(lan)
    t:assert(provium.temporary == nil)
    t:assert(provium:bridge("temporary", {}))
end)
"#,
    );
    assert_all_passed(&outcome);
}

#[test]
fn file_scope_stream_survives_across_tests() {
    // DESIGN § Cross-test stream sharing: streams created
    // OUTSIDE any test() block are owned by the file.
    let outcome = run_local_lua(
        r#"
local tmp = os.tmpname()
local f = io.open(tmp, "w"); f:write(""); f:close()
local vm = provium:vm("a", "peios"):boot()
local stream = vm:tail_file(tmp)

test("test-1 sees file-scope stream", function(t)
    t:assert(stream ~= nil)
end)

test("test-2 still sees it", function(t)
    -- Auto-close would have reaped a test-scope stream; this
    -- one survives because it was declared at file scope.
    t:assert(stream ~= nil)
end)
"#,
    );
    assert_all_passed(&outcome);
}
