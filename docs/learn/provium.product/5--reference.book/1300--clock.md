---
title: Clock
description:
  The Clock userdata, set, advance, sleep, and query the guest's wall clock in
  seconds or nanoseconds, including moving time backwards.
related:
  - provium/reference/vm
  - provium/writing-tests/vms-and-profiles
---

A Clock wraps the guest's wall clock. Operations dispatch to the agent, which
adjusts the kernel's `CLOCK_REALTIME` (and equivalents) under the hood.

The clock is signed, you can move the guest backwards in time. This matters
for testing time-dependent code: certificate expiry, timeout handling,
leap-second handling, retry backoff.

## Constructing

| Source       | Returns            |
| ------------ | ------------------ |
| `vm:clock()` | The guest's clock. |

## Methods

### `clock:get()`

Returns the current time as seconds-since-epoch (float). For sub-microsecond
accuracy use `:get_ns()`.

```lua
local t = vm:clock():get()
-- t ≈ 1700000000.0
```

### `clock:get_ns()`

Returns nanoseconds-since-epoch as a 64-bit signed integer. Lua 5.4 integers
are 64-bit so no precision is lost.

### `clock:set(seconds)`

Set the wall clock to `seconds` seconds since epoch. Accepts integers or
floats. Negative values are allowed, the guest goes pre-epoch, useful for
testing `time_t` boundary handling.

Errors on NaN or infinity
(`clock:set: value must be a finite number (got NaN/inf)`). Without this
guard, the integer cast would silently saturate or round to zero.

### `clock:set_ns(ns)`

Set the wall clock to `ns` nanoseconds since epoch (i64). Same semantics as
`:set` but at full precision.

### `clock:sleep(duration)`

Sleep for `duration` seconds. Accepts:

- Integer / float, seconds.
- String, `"100ms"` / `"5s"` / `"5m"` / `"2h"`.

Errors on NaN, infinity, or negative.

### `clock:advance(duration)`

Advance the wall clock by `duration` seconds (signed). Unlike `:sleep`, this
does not block, the kernel's `CLOCK_REALTIME` jumps forward (or backward).

Negative values are explicitly allowed (`clock:advance(-3600)` rolls the guest
back an hour). Errors on NaN or infinity but not on negative.

## Example: certificate expiry test

```lua
test("client refuses an expired cert", function(t)
    local vm = provium:vm("v", "peios"):boot()
    -- Set the clock to before the cert was issued.
    vm:clock():set(1500000000)  -- 2017-07-14
    local r = vm:run("openssl s_client -connect server:443 < /dev/null")
    r:assert_ok()  -- baseline: cert is valid

    -- Jump past expiry.
    vm:clock():advance(20 * 365 * 24 * 3600)  -- +20 years
    local r2 = vm:run("openssl s_client -connect server:443 < /dev/null")
    t:assert(not r2:ok())  -- cert is now expired
    t:assert(r2.stderr:find("certificate has expired"))
end)
```

## Notes

- `clock:get()` (float) loses precision below ~250 ns at modern epoch values
  because of f64 mantissa width. Use `:get_ns()` if you need exact ns
  roundtrip.
- The clock is not a guest namespace, every process inside the guest sees the
  same time axis after `:set` / `:advance`.
- Boot-time initial clock is set through `boot_opts.initial_time` (see
  [VM boot opts](~provium/reference/vm#boot-opts)). The Clock methods are for
  adjustment after boot.

## See also


- [VM](~provium/reference/vm): `vm:clock()` and the `boot_opts.initial_time`
  boot-time setter.
