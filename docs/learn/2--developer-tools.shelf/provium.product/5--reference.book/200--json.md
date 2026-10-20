---
title: JSON
description:
  The `json` global, encode and decode JSON between Lua values and strings,
  backed by serde_json.
related:
  - provium/reference/provium-module
  - provium/reference/test-framework
---

`json` is a top-level Lua global. Two methods, both pure host-side functions
(no VM round-trip).

## `json.encode(value)`

Serialize a Lua value to a JSON string. Tables, numbers, booleans, strings,
and `nil` round-trip naturally.

```lua
json.encode({a = 1, b = "x"})        -- '{"a":1,"b":"x"}'
json.encode({10, 20, 30})            -- '[10,20,30]'
json.encode(true)                    -- 'true'
json.encode(nil)                     -- 'null'
json.encode({a = 1, b = nil})        -- '{"a":1}'  (b isn't in the table — Lua semantics)
json.encode({[1]=1, [3]=3})          -- '[1,null,3]'  (array padded to max int key)
```

Array and object detection: a table whose keys are positive integers (`1..=N`,
possibly with gaps) encodes as a JSON array of length `max(key)`; gaps emit
`null`. Anything else encodes as an object. Mixed tables
(`{1, 2, name = "x"}`) fall into the object case, with integer keys
stringified.

Encoding errors (cycles, function/userdata values, non-string-coercible keys)
raise a Lua error.

## `json.decode(string)`

Parse a JSON string into a Lua value.

```lua
local t = json.decode('{"x": 42}')
print(t.x)                           -- 42

local t = json.decode('{"a": {"b": ["c", "d"]}}')
print(t.a.b[2])                      -- "d"
```

### Number precision

JSON integers in `[i64::MIN, i64::MAX]` decode to Lua integers exactly, full
64-bit precision, no rounding through `f64`. Integers above `i64::MAX` (that
is u64-only values, up to `0xFFFFFFFFFFFFFFFF`) preserve their bit pattern
through a wrap-cast: `0xFFFFFFFFFFFFFFFF` decodes to `-1`, exactly matching
Lua 5.4's own `tonumber("0xFFFFFFFFFFFFFFFF")`. Bitwise ops still work
correctly on the result (the bits are the bits).

Non-integer JSON (`3.14`, `1e10`) decodes to Lua's float type as expected.

You can drop the "emit as hex string and `tonumber()` it" workaround that
older code used, large unsigned values now round-trip through `json.decode`
directly.

### Null handling

Lua tables can't hold `nil` as a value, assigning `nil` to a key removes it.
The binding uses the following JSON conversion rules:

| Input          | Lua result                      | Notes                                                  |
| -------------- | ------------------------------- | ------------------------------------------------------ |
| `null`         | `nil`                           | Top-level scalar.                                      |
| `{"k": null}`  | A table with no `k` key.        | `t.k == nil`, `next(t)` doesn't yield `k`.             |
| `[1, null, 3]` | `{1, nil, 3}`, array-with-hole. | `t[2] == nil`; `#t` is implementation-defined per Lua. |

This is lossy versus the original `"key exists but is null"`: round-tripping
`decode` → `encode` drops null fields. Tests that need to distinguish `null`
from absent should keep the source string and re-check.

Decoding errors raise a Lua error with the parse position.

## Typical use

Reading config the guest emitted:

```lua
local body = vm:read_file("/var/state/peios/state.json")
local state = json.decode(body)
t:assert_eq(state.ready, true)
```

Building a request body for a test client:

```lua
local body = json.encode({op = "write", key = "k", value = "v"})
vm:run("curl", {"-X", "POST", "-d", body, "http://localhost:8080/api"}):assert_ok()
```

## Performance

`encode` and `decode` are pure-host serde_json calls, microseconds for typical
config-sized inputs. No VM round-trip, no agent involvement. Safe to call from
inside tight test loops.
