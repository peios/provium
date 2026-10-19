-- MessagePack, both ways, for the length-prefixed query channels the
-- network daemons speak (netd's control socket, resolvd's native socket).
--
-- Decoding covers every type those channels send. A map decodes to a Lua
-- table keyed by its keys, an array to a sequence, nil to nil, so a field
-- the peer sent as nil is simply absent. Encoding takes plain Lua values:
-- a table with a `[1]` (or `msgpack.array(t)`) is an array, any other
-- table a map, and `msgpack.NIL` is an explicit nil.

local M = {}

M.NIL = setmetatable({}, { __tostring = function() return "msgpack.NIL" end })
local ARRAY = {}

--- Mark `t` as an array even when it is empty.
function M.array(t) return setmetatable(t or {}, ARRAY) end

local function decode(b, at)
    local tag = b:byte(at)
    if not tag then error("msgpack: ran off the end at " .. at) end
    if tag < 0x80 then return tag, at + 1 end
    if tag >= 0xe0 then return tag - 0x100, at + 1 end
    local function map(n, from)
        local out = {}
        for _ = 1, n do
            local k, v
            k, from = decode(b, from)
            v, from = decode(b, from)
            out[k] = v
        end
        return out, from
    end
    local function arr(n, from)
        local out = {}
        for i = 1, n do out[i], from = decode(b, from) end
        return out, from
    end
    local function str(n, from) return b:sub(from, from + n - 1), from + n end
    if tag <= 0x8f then return map(tag - 0x80, at + 1) end
    if tag <= 0x9f then return arr(tag - 0x90, at + 1) end
    if tag <= 0xbf then return str(tag - 0xa0, at + 1) end
    if tag == 0xc0 then return nil, at + 1 end
    if tag == 0xc2 then return false, at + 1 end
    if tag == 0xc3 then return true, at + 1 end
    if tag == 0xc4 or tag == 0xd9 then return str(b:byte(at + 1), at + 2) end
    if tag == 0xc5 or tag == 0xda then return str(string.unpack(">I2", b, at + 1), at + 3) end
    if tag == 0xc6 or tag == 0xdb then return str(string.unpack(">I4", b, at + 1), at + 5) end
    if tag == 0xca then return string.unpack(">f", b, at + 1), at + 5 end
    if tag == 0xcb then return string.unpack(">d", b, at + 1), at + 9 end
    if tag == 0xcc then return string.unpack(">I1", b, at + 1), at + 2 end
    if tag == 0xcd then return string.unpack(">I2", b, at + 1), at + 3 end
    if tag == 0xce then return string.unpack(">I4", b, at + 1), at + 5 end
    if tag == 0xcf then return string.unpack(">I8", b, at + 1), at + 9 end
    if tag == 0xd0 then return string.unpack(">i1", b, at + 1), at + 2 end
    if tag == 0xd1 then return string.unpack(">i2", b, at + 1), at + 3 end
    if tag == 0xd2 then return string.unpack(">i4", b, at + 1), at + 5 end
    if tag == 0xd3 then return string.unpack(">i8", b, at + 1), at + 9 end
    if tag == 0xdc then return arr(string.unpack(">I2", b, at + 1), at + 3) end
    if tag == 0xdd then return arr(string.unpack(">I4", b, at + 1), at + 5) end
    if tag == 0xde then return map(string.unpack(">I2", b, at + 1), at + 3) end
    if tag == 0xdf then return map(string.unpack(">I4", b, at + 1), at + 5) end
    error(string.format("msgpack: unhandled tag 0x%02x at %d", tag, at))
end

--- Decode one value from `bytes`. Returns the value and the index after it.
function M.decode(bytes, at) return decode(bytes, at or 1) end

local function encode(v, out)
    local t = type(v)
    if v == nil or v == M.NIL then
        out[#out + 1] = "\xc0"
    elseif t == "boolean" then
        out[#out + 1] = v and "\xc3" or "\xc2"
    elseif t == "number" and math.type(v) == "integer" then
        if v >= 0 and v < 0x80 then out[#out + 1] = string.char(v)
        elseif v < 0 and v >= -32 then out[#out + 1] = string.char(v + 0x100)
        elseif v >= 0 and v <= 0xFFFFFFFF then out[#out + 1] = "\xce" .. string.pack(">I4", v)
        else out[#out + 1] = "\xd3" .. string.pack(">i8", v) end
    elseif t == "number" then
        out[#out + 1] = "\xcb" .. string.pack(">d", v)
    elseif t == "string" then
        local n = #v
        if n < 32 then out[#out + 1] = string.char(0xa0 + n)
        elseif n < 0x100 then out[#out + 1] = "\xd9" .. string.char(n)
        elseif n < 0x10000 then out[#out + 1] = "\xda" .. string.pack(">I2", n)
        else out[#out + 1] = "\xdb" .. string.pack(">I4", n) end
        out[#out + 1] = v
    elseif t == "table" then
        if getmetatable(v) == ARRAY or v[1] ~= nil then
            local n = #v
            out[#out + 1] = n < 16 and string.char(0x90 + n) or ("\xdc" .. string.pack(">I2", n))
            for i = 1, n do encode(v[i], out) end
        else
            local keys = {}
            for k in pairs(v) do keys[#keys + 1] = k end
            table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
            local n = #keys
            out[#out + 1] = n < 16 and string.char(0x80 + n) or ("\xde" .. string.pack(">I2", n))
            for _, k in ipairs(keys) do encode(k, out); encode(v[k], out) end
        end
    else
        error("msgpack: cannot encode a " .. t)
    end
end

--- Encode a Lua value. Map keys are written in sorted order, so equal
--- tables encode to equal bytes.
function M.encode(v)
    local out = {}
    encode(v, out)
    return table.concat(out)
end

return M
