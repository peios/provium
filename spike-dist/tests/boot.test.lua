-- Spike: does dist's real boot image survive provium's agent injection,
-- and can the initramfs-resident agent reach the real Peios root at /sysroot?
--
-- After prelude boots it MS_MOVEs /proc,/sys,/dev into /sysroot, deletes the
-- old initramfs (Phase 6), and chroots into /sysroot. So the agent listener
-- ends up in a gutted rootfs with no shell — every check here uses direct
-- agent syscall ops (stat/listdir/read_file), never vm:run.

local function names_in(vm, path)
  local ok, entries = pcall(function() return vm:listdir(path) end)
  if not ok or not entries then return nil end
  local out = {}
  for _, e in ipairs(entries) do out[#out + 1] = e.name end
  return out
end

test("dist image boots and the agent reaches /sysroot", function(t)
  local vm = provium:vm("d", "peios-dist")
  vm:boot() -- returns once the agent answers ping == chain reached the listener

  -- (c) The crux: agent runs in the (gutted) initramfs; the real userspace
  -- lives behind prelude's chroot at /sysroot. Wait for the overlay to be
  -- mounted + populated (prelude races a moment behind the agent listener).
  local bins = wait_until(function()
    local n = names_in(vm, "/sysroot/usr/bin")
    if n and #n > 0 then return n end
  end, { timeout = "15s", interval = "200ms", desc = "/sysroot/usr/bin populated" })

  local joined = table.concat(bins, " ")
  t:log("== /sysroot/usr/bin (pre-chroot) ==\n" .. joined)
  t:assert_contains(joined, "peipkg", "peipkg present in the real root")

  -- Before chroot, the agent's own / is the gutted initramfs: /usr/bin
  -- should NOT contain the real userspace.
  local before = names_in(vm, "/usr/bin") or {}
  t:log("== /usr/bin (pre-chroot, initramfs view) ==\n" .. table.concat(before, " "))

  -- The proposed mechanism: chroot the agent process itself into the real
  -- root via raw syscall. x86_64: chroot=161, chdir=80. All agent op-threads
  -- share one fs_struct, so this re-roots every subsequent op.
  local r1 = vm:syscall(161, {bufs = {"/sysroot\0"}, ptrs = {0}})
  t:assert_eq(r1.errno, 0, "chroot(/sysroot) errno")
  local r2 = vm:syscall(80, {bufs = {"/\0"}, ptrs = {0}})
  t:assert_eq(r2.errno, 0, "chdir(/) errno")

  -- After chroot, the agent's / IS the real root: /usr/bin now resolves to
  -- the real userspace, with no /sysroot prefix.
  local after = names_in(vm, "/usr/bin") or {}
  t:log("== /usr/bin (post-chroot, real-root view) ==\n" .. table.concat(after, " "))
  t:assert_contains(table.concat(after, " "), "peipkg",
    "post-chroot /usr/bin shows the real userspace")

  -- And exec resolves in the real root too (needs /bin/sh from the real root).
  local v = vm:run("peipkg --version 2>&1 || peipkg version 2>&1 || echo NORUN")
  t:log("== peipkg via vm:run (post-chroot) ==\n" .. (v.stdout or "") .. (v.stderr or ""))

  vm:shutdown()
end)
