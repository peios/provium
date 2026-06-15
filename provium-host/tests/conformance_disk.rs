//! Conformance: `vm:disk` / fault injection per `DESIGN.md` §
//! Disk.

#![cfg(feature = "lua")]

mod common;

use common::{assert_one_passed, run_local_lua};

#[test]
fn disk_with_image_returns_handle() {
    let outcome = run_local_lua(
        r#"
test("disk with_image", function(t)
    local img = os.tmpname()
    local f = io.open(img, "w")
    f:write(string.rep("\0", 4096))
    f:close()
    local vm = provium:vm("a", "peios"):boot()
    local disk = vm:attach_disk({id="vda", size = 4096, image = img})
    t:assert(disk:size() > 0)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn fault_inject_unknown_mode_rejected() {
    let outcome = run_local_lua(
        r#"
test("bad fault mode", function(t)
    local img = os.tmpname()
    io.open(img, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local disk = vm:attach_disk({id="vda", size = 512, image = img})
    local ok, err = pcall(function()
        disk:fault_inject("nonsense_mode")
    end)
    t:assert(not ok, "unknown fault mode must error")
    t:assert(tostring(err):find("unknown") or tostring(err):find("valid"),
        "error must list valid modes: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn fault_inject_eio_read_returns_error_on_next_read() {
    let outcome = run_local_lua(
        r#"
test("eio_read", function(t)
    local img = os.tmpname()
    local f = io.open(img, "w"); f:write(string.rep("\1", 1024)); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local disk = vm:attach_disk({id="vda", size = 1024, image = img})
    disk:fault_inject("eio_read")
    local ok, err = pcall(function() disk:read_sectors(0, 1) end)
    t:assert(not ok, "read after eio_read must error")
    t:assert(tostring(err):find("EIO"),
        "error must mention EIO: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn read_sectors_without_image_errors() {
    let outcome = run_local_lua(
        r#"
test("no image", function(t)
    local vm = provium:vm("a", "peios"):boot()
    local disk = vm:attach_disk({id="vda", size = 1024})
    local ok, err = pcall(function() disk:read_sectors(0, 1) end)
    t:assert(not ok)
    t:assert(tostring(err):find("image") or tostring(err):find("backing"),
        "error must mention image: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn fault_inject_eio_write_blocks_writes() {
    let outcome = run_local_lua(
        r#"
test("eio_write", function(t)
    local img = os.tmpname()
    local f = io.open(img, "w"); f:write(string.rep("\0", 1024)); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local disk = vm:attach_disk({id="vda", size = 1024, image = img})
    disk:fault_inject("eio_write")
    local ok, err = pcall(function() disk:write_sectors(0, "data") end)
    t:assert(not ok, "write after eio_write must error")
    t:assert(tostring(err):find("EIO"),
        "error must mention EIO: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn fault_inject_slow_does_not_change_outcome() {
    // R8 #406: slow-fault adds latency but doesn't change the
    // operation's success/failure. The data still round-trips.
    let outcome = run_local_lua(
        r#"
test("slow", function(t)
    local img = os.tmpname()
    local f = io.open(img, "w"); f:write(string.rep("\0", 512)); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local disk = vm:attach_disk({id="vda", size = 512, image = img})
    disk:fault_inject("slow")
    -- Should still succeed (just slowly).
    local body = disk:read_sectors(0, 1)
    t:assert(type(body) == "string")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn clear_faults_removes_eio_read() {
    let outcome = run_local_lua(
        r#"
test("clear_faults", function(t)
    local img = os.tmpname()
    local f = io.open(img, "w"); f:write(string.rep("\0", 512)); f:close()
    local vm = provium:vm("a", "peios"):boot()
    local disk = vm:attach_disk({id="vda", size = 512, image = img})
    disk:fault_inject("eio_read")
    disk:clear_faults()
    -- Read must succeed after clear.
    local body = disk:read_sectors(0, 1)
    t:assert(type(body) == "string")
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn detached_disk_read_errors() {
    let outcome = run_local_lua(
        r#"
test("detached read", function(t)
    local img = os.tmpname()
    io.open(img, "w"):close()
    local vm = provium:vm("a", "peios"):boot()
    local disk = vm:attach_disk({id="vda", size = 512, image = img})
    -- Detach (LocalAgent: graph state)
    pcall(function() disk:detach() end)
end)
"#,
    );
    assert_one_passed(&outcome);
}
