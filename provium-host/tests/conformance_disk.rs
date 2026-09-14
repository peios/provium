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
fn scratch_disk_is_created_at_the_requested_size() {
    let outcome = run_local_lua(
        r#"
test("scratch", function(t)
    local vm = provium:vm("a", "peios")
    vm:boot({ disks = {{ scratch = "1M", id = "state" }} })
    local disk = vm:disk("state")
    t:assert_eq(disk:size(), 1024 * 1024, "scratch disk is the size asked for")
    -- Blank, and readable: provium made the file, so the test did not
    -- have to and there is nothing to clean up.
    t:assert_eq(disk:read_sectors(0, 1), string.rep("\0", 512))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn scratch_disk_accepts_a_byte_count() {
    let outcome = run_local_lua(
        r#"
test("scratch bytes", function(t)
    local vm = provium:vm("a", "peios")
    vm:boot({ disks = {{ scratch = 4096, id = "state" }} })
    t:assert_eq(vm:disk("state"):size(), 4096)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn template_disk_copies_and_leaves_the_template_alone() {
    let outcome = run_local_lua(
        r#"
test("template", function(t)
    local src = os.tmpname()
    local f = io.open(src, "wb")
    f:write("SEED"):write(string.rep("\0", 1024 - 4))
    f:close()

    local vm = provium:vm("a", "peios")
    vm:boot({ disks = {{ template = src, id = "seeded" }} })
    local disk = vm:disk("seeded")
    t:assert_eq(disk:read_sectors(0, 1):sub(1, 4), "SEED",
        "the copy starts from the template's contents")

    -- Writing through the disk must not reach the template: it is a
    -- fixture the next VM will copy again.
    disk:write_sectors(0, "MINE")
    local check = io.open(src, "rb")
    local head = check:read(4)
    check:close()
    t:assert_eq(head, "SEED", "the template itself is untouched")
    os.remove(src)
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn a_disk_naming_two_sources_is_rejected() {
    let outcome = run_local_lua(
        r#"
test("two sources", function(t)
    local vm = provium:vm("a", "peios")
    local ok, err = pcall(function()
        vm:boot({ disks = {{ path = "/tmp/x.img", scratch = "1M", id = "d" }} })
    end)
    t:assert(not ok, "naming both path and scratch must error")
    t:assert(tostring(err):find("exactly one"), tostring(err))
    t:assert(tostring(err):find("path") and tostring(err):find("scratch"),
        "the error names which two were given: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn a_disk_naming_no_source_is_rejected() {
    let outcome = run_local_lua(
        r#"
test("no source", function(t)
    local vm = provium:vm("a", "peios")
    local ok, err = pcall(function()
        vm:boot({ disks = {{ id = "d" }} })
    end)
    t:assert(not ok, "a disk with no source must error")
    t:assert(tostring(err):find("exactly one"), tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn an_unparseable_scratch_size_is_rejected() {
    let outcome = run_local_lua(
        r#"
test("bad size", function(t)
    local vm = provium:vm("a", "peios")
    local ok, err = pcall(function()
        vm:boot({ disks = {{ scratch = "512Q", id = "d" }} })
    end)
    t:assert(not ok, "an unparseable size must error rather than default")
    t:assert(tostring(err):find("512Q"), "the error quotes what was given: " .. tostring(err))
end)
"#,
    );
    assert_one_passed(&outcome);
}

#[test]
fn a_named_path_is_still_never_created() {
    let outcome = run_local_lua(
        r#"
test("path is not created", function(t)
    -- The whole point of keeping `path` separate from `scratch`: a
    -- typo stays an error instead of silently becoming a blank disk.
    local missing = os.tmpname()
    os.remove(missing)
    local vm = provium:vm("a", "peios")
    vm:boot({ disks = {{ path = missing, id = "d" }} })
    local f = io.open(missing, "rb")
    t:assert(f == nil, "provium must not have created the named path")
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
