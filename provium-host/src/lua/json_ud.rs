//! `json` Lua global — JSON encode/decode backed by `serde_json`.
//!
//! `json.encode(value)` returns a JSON string; `json.decode(string)`
//! returns the corresponding Lua value.
//!
//! ## Null handling
//!
//! Lua tables can't hold `nil` as a value — assigning `nil` to a
//! key is the same as removing it. So we adopt the convention used
//! by every popular Lua JSON library (dkjson, lua-cjson):
//!
//! Decode:
//! - Top-level `null` → Lua `nil`.
//! - Object value `null` → key is **not inserted**. `t.k` reads as
//!   `nil`, `next(t)` doesn't yield it. Lossy versus the original
//!   "key exists but is null" — tests that need that distinction
//!   should re-check the source string.
//! - Array element `null` → Lua `nil` at that index — i.e. a hole.
//!   `#t` is implementation-defined per Lua's reference; this is
//!   the standard array-with-hole behaviour and we don't introduce
//!   a sentinel to paper over it.
//!
//! Encode:
//! - Top-level `nil` → `"null"`.
//! - Lua tables can't store `nil` values to begin with, so
//!   `{a=1, b=nil}` is just `{a=1}` — encodes to `{"a":1}`.
//!   No special handling needed; this is Lua semantics.
//! - Array with holes: `{1, [3]=3}` (which is what `{1, nil, 3}`
//!   actually constructs in Lua) encodes to `[1,null,3]` — we
//!   detect array-shaped tables by max integer key and emit a JSON
//!   array padded with `null` for absent integer slots.

use mlua::{Lua, Table, Value};

/// Install the `json` global onto `lua`. Idempotent — calling
/// twice replaces the previous globals.
pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    let json = lua.create_table()?;

    json.set(
        "encode",
        lua.create_function(|_, value: Value| {
            let v = lua_to_json(value)?;
            serde_json::to_string(&v).map_err(mlua::Error::external)
        })?,
    )?;

    json.set(
        "decode",
        lua.create_function(|lua, s: String| {
            let v: serde_json::Value =
                serde_json::from_str(&s).map_err(mlua::Error::external)?;
            json_to_lua(lua, &v)
        })?,
    )?;

    lua.globals().set("json", json)?;
    Ok(())
}

/// Recursive `serde_json::Value` → `mlua::Value`. See module doc
/// for the null-handling convention.
fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    match value {
        serde_json::Value::Null => Ok(Value::Nil),
        serde_json::Value::Bool(b) => Ok(Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            // Prefer integer decoding so 64-bit values keep full
            // precision. Three cases:
            //   1. Fits in i64 (signed range)         → Value::Integer
            //   2. Fits in u64 but not i64            → wrap-cast to i64
            //      (preserves the bit pattern; matches Lua 5.4's own
            //      `tonumber("0xFFFFFFFFFFFFFFFF")` → -1 behaviour).
            //      Bitwise ops on the result work identically — the
            //      bits are the bits.
            //   3. Non-integer (float syntax in JSON) → Value::Number.
            //
            // Without case 2 we'd send u64-above-i64::MAX through
            // f64, losing precision above 2^53. Users masking on
            // kernel-flag values (where the high bit is meaningful)
            // would see silent bit corruption — work-arounded today
            // by emitting masks as hex strings and `tonumber()`ing
            // them, which we now make unnecessary.
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(u) = n.as_u64() {
                Ok(Value::Integer(u as i64))
            } else {
                Ok(Value::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => {
            Ok(Value::String(lua.create_string(s.as_str())?))
        }
        serde_json::Value::Array(arr) => {
            let t = lua.create_table_with_capacity(arr.len(), 0)?;
            for (i, v) in arr.iter().enumerate() {
                // Lua arrays are 1-indexed. Setting a nil value
                // here is equivalent to leaving the slot empty,
                // which is exactly what we want for array holes.
                let lua_v = json_to_lua(lua, v)?;
                t.raw_set(i + 1, lua_v)?;
            }
            Ok(Value::Table(t))
        }
        serde_json::Value::Object(obj) => {
            let t = lua.create_table_with_capacity(0, obj.len())?;
            for (k, v) in obj.iter() {
                // Skip null-valued keys entirely so `t.k == nil`
                // is true and `next(t)` doesn't yield them — the
                // standard Lua JSON convention.
                if v.is_null() {
                    continue;
                }
                let lua_v = json_to_lua(lua, v)?;
                t.raw_set(k.as_str(), lua_v)?;
            }
            Ok(Value::Table(t))
        }
    }
}

/// Recursive `mlua::Value` → `serde_json::Value`. Distinguishes
/// arrays from objects by inspecting table keys: a table whose
/// keys are exactly `1..=N` (with possible gaps) for some N >= 1
/// encodes as a JSON array of length N (gaps → `null`). Anything
/// else encodes as an object.
fn lua_to_json(value: Value) -> mlua::Result<serde_json::Value> {
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(b)),
        Value::Integer(i) => Ok(serde_json::Value::Number(i.into())),
        Value::Number(n) => Ok(serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)),
        Value::String(s) => Ok(serde_json::Value::String(
            s.to_str()
                .map_err(mlua::Error::external)?
                .to_string(),
        )),
        Value::Table(t) => table_to_json(t),
        other => Err(mlua::Error::external(format!(
            "json.encode: unsupported value type `{}`",
            other.type_name()
        ))),
    }
}

fn table_to_json(t: Table) -> mlua::Result<serde_json::Value> {
    // Single pass to discover key shape: track max positive integer
    // key and whether any non-int-key (or non-positive int) exists.
    let mut max_int_key: i64 = 0;
    let mut has_non_array_key = false;
    let mut entry_count: usize = 0;
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        entry_count += 1;
        match k {
            Value::Integer(i) if i >= 1 => {
                if i > max_int_key {
                    max_int_key = i;
                }
            }
            Value::Number(n) if n.fract() == 0.0 && n >= 1.0 && n.is_finite() => {
                let as_int = n as i64;
                if as_int > max_int_key {
                    max_int_key = as_int;
                }
            }
            _ => {
                has_non_array_key = true;
            }
        }
    }
    // Array shape: at least one positive-int key, no other keys.
    // Empty table → object (`{}`), matching dkjson / lua-cjson
    // default. Tests that need an empty JSON array should emit it
    // as a literal string.
    if !has_non_array_key && max_int_key > 0 {
        let mut arr = Vec::with_capacity(max_int_key as usize);
        for i in 1..=max_int_key {
            let v: Value = t.raw_get(i)?;
            arr.push(lua_to_json(v)?);
        }
        return Ok(serde_json::Value::Array(arr));
    }
    if entry_count == 0 {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let mut obj = serde_json::Map::with_capacity(entry_count);
    for pair in t.pairs::<Value, Value>() {
        let (k, v) = pair?;
        let k_str = match k {
            Value::String(s) => s
                .to_str()
                .map_err(mlua::Error::external)?
                .to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => {
                // Match Lua's tostring(): "3.0" for non-integer
                // floats, "3" for integer-valued floats. Keeps
                // mixed-key objects predictable across encode rounds.
                if n.fract() == 0.0 && n.is_finite() {
                    (n as i64).to_string()
                } else {
                    n.to_string()
                }
            }
            Value::Boolean(b) => b.to_string(),
            other => {
                return Err(mlua::Error::external(format!(
                    "json.encode: table key must be string, number, \
                     or boolean (got `{}`)",
                    other.type_name()
                )));
            }
        };
        obj.insert(k_str, lua_to_json(v)?);
    }
    Ok(serde_json::Value::Object(obj))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_lua() -> Lua {
        let lua = Lua::new();
        install(&lua).unwrap();
        lua
    }

    #[test]
    fn encode_decode_roundtrip_object() {
        let lua = fresh_lua();
        let out: String = lua
            .load(r#"return json.encode({a = 1, b = "x"})"#)
            .eval()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], "x");
    }

    #[test]
    fn encode_array() {
        let lua = fresh_lua();
        let out: String = lua
            .load(r#"return json.encode({10, 20, 30})"#)
            .eval()
            .unwrap();
        assert_eq!(out, "[10,20,30]");
    }

    #[test]
    fn encode_nil_is_null() {
        let lua = fresh_lua();
        let out: String = lua.load(r#"return json.encode(nil)"#).eval().unwrap();
        assert_eq!(out, "null");
    }

    #[test]
    fn encode_object_with_nil_field_drops_key() {
        // `b = nil` doesn't put b in the table at all — Lua
        // semantics, not a json-specific thing. The encoder
        // therefore can't see it.
        let lua = fresh_lua();
        let out: String = lua
            .load(r#"return json.encode({a = 1, b = nil})"#)
            .eval()
            .unwrap();
        assert_eq!(out, r#"{"a":1}"#);
    }

    #[test]
    fn encode_array_with_hole_emits_null() {
        // `{1, nil, 3}` in Lua is actually `{1, [3]=3}` — but our
        // encoder pads up to max_int_key with nulls, so the
        // intuitive shape comes out.
        let lua = fresh_lua();
        let out: String = lua
            .load(r#"return json.encode({[1]=1, [3]=3})"#)
            .eval()
            .unwrap();
        assert_eq!(out, "[1,null,3]");
    }

    #[test]
    fn decode_object_into_table() {
        let lua = fresh_lua();
        let n: i64 = lua
            .load(r#"local t = json.decode('{"x": 42}'); return t.x"#)
            .eval()
            .unwrap();
        assert_eq!(n, 42);
    }

    #[test]
    fn decode_nested() {
        let lua = fresh_lua();
        let s: String = lua
            .load(
                r#"local t = json.decode('{"a": {"b": ["c", "d"]}}'); return t.a.b[2]"#,
            )
            .eval()
            .unwrap();
        assert_eq!(s, "d");
    }

    #[test]
    fn decode_top_level_null_is_nil() {
        let lua = fresh_lua();
        let r: Value = lua.load(r#"return json.decode("null")"#).eval().unwrap();
        assert!(matches!(r, Value::Nil), "got {r:?}");
    }

    #[test]
    fn decode_object_null_skips_the_key() {
        // After decode, t.k reads as nil AND next(t) doesn't yield
        // it — i.e. the key is genuinely absent, not stored-as-nil
        // (which Lua can't do anyway).
        let lua = fresh_lua();
        let (k_is_nil, next_is_nil): (bool, bool) = lua
            .load(
                r#"
                local t = json.decode('{"k": null, "a": 1}')
                local _, _ = next(t)        -- get the first key
                local keys = {}
                for k, _ in pairs(t) do keys[#keys+1] = k end
                -- assert "k" never appears in iteration
                local saw_k = false
                for _, key in ipairs(keys) do if key == "k" then saw_k = true end end
                return t.k == nil, not saw_k
            "#,
            )
            .eval()
            .unwrap();
        assert!(k_is_nil);
        assert!(next_is_nil);
    }

    #[test]
    fn decode_array_null_creates_hole() {
        // t[2] is nil; t[1] and t[3] keep their values.
        let lua = fresh_lua();
        let (v1, v2_is_nil, v3): (i64, bool, i64) = lua
            .load(
                r#"
                local t = json.decode('[1, null, 3]')
                return t[1], t[2] == nil, t[3]
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(v1, 1);
        assert!(v2_is_nil);
        assert_eq!(v3, 3);
    }

    #[test]
    fn decode_large_i64_keeps_precision() {
        // i64::MAX = 9223372036854775807. Well above 2^53
        // (9007199254740992) — would lose precision through f64.
        let lua = fresh_lua();
        let n: i64 = lua
            .load(r#"return json.decode("9223372036854775807")"#)
            .eval()
            .unwrap();
        assert_eq!(n, i64::MAX);
    }

    #[test]
    fn decode_u64_above_i64_max_wraps_bitwise() {
        // u64::MAX (= 0xFFFFFFFFFFFFFFFF) is above i64::MAX. We
        // preserve the bit pattern via wrap-cast — same convention
        // as Lua 5.4's own `tonumber("0xFFFFFFFFFFFFFFFF")` which
        // returns -1. Bitwise ops on the result still get the
        // right bits.
        let lua = fresh_lua();
        let n: i64 = lua
            .load(r#"return json.decode("18446744073709551615")"#)
            .eval()
            .unwrap();
        assert_eq!(n, -1);
        // Sanity: AND-mask still yields the expected value.
        let masked: i64 = lua
            .load(r#"return json.decode("18446744073709551615") & 0xFF"#)
            .eval()
            .unwrap();
        assert_eq!(masked, 0xFF);
    }

    #[test]
    fn decode_float_still_float() {
        // Non-integer JSON still decodes as a Lua number (float),
        // not an integer. Regression guard against the new u64
        // path swallowing valid floats.
        let lua = fresh_lua();
        let n: f64 = lua
            .load(r#"return json.decode("3.14")"#)
            .eval()
            .unwrap();
        assert!((n - 3.14).abs() < 1e-9);
    }

    #[test]
    fn decode_invalid_errors() {
        let lua = fresh_lua();
        let err = lua
            .load(r#"return json.decode("not json")"#)
            .eval::<Value>()
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.to_lowercase().contains("json") || msg.contains("expected"));
    }

    #[test]
    fn null_roundtrip_through_decode_then_encode_loses_the_field() {
        // Documented lossy behaviour: `{"k": null}` → decode →
        // encode → `{}` because the key was dropped on decode.
        // Tests that care about the null-vs-absent distinction
        // should keep the source string around.
        let lua = fresh_lua();
        let out: String = lua
            .load(r#"return json.encode(json.decode('{"k": null}'))"#)
            .eval()
            .unwrap();
        assert_eq!(out, "{}");
    }
}
