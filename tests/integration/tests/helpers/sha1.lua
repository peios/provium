-- SHA-1, in Lua, so a test can compute what netd derives from a digest —
-- interface ids, network ids, stable-privacy addresses — and compare
-- rather than only check that a value looks plausible.

local M = {}

local function rol(x, n) return ((x << n) | (x >> (32 - n))) & 0xFFFFFFFF end

--- The 20-byte SHA-1 digest of `msg`.
function M.digest(msg)
    local h0, h1, h2, h3, h4 = 0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0
    local len = #msg
    msg = msg .. "\x80" .. string.rep("\0", (55 - len) % 64) .. string.pack(">I8", len * 8)
    for chunk = 1, #msg, 64 do
        local w = {}
        for i = 0, 15 do w[i] = string.unpack(">I4", msg, chunk + i * 4) end
        for i = 16, 79 do w[i] = rol(w[i - 3] ~ w[i - 8] ~ w[i - 14] ~ w[i - 16], 1) end
        local a, b, c, d, e = h0, h1, h2, h3, h4
        for i = 0, 79 do
            local f, k
            if i < 20 then f, k = (b & c) | ((~b) & d), 0x5A827999
            elseif i < 40 then f, k = b ~ c ~ d, 0x6ED9EBA1
            elseif i < 60 then f, k = (b & c) | (b & d) | (c & d), 0x8F1BBCDC
            else f, k = b ~ c ~ d, 0xCA62C1D6 end
            local t = (rol(a, 5) + (f & 0xFFFFFFFF) + e + k + w[i]) & 0xFFFFFFFF
            e, d, c, b, a = d, c, rol(b, 30), a, t
        end
        h0 = (h0 + a) & 0xFFFFFFFF
        h1 = (h1 + b) & 0xFFFFFFFF
        h2 = (h2 + c) & 0xFFFFFFFF
        h3 = (h3 + d) & 0xFFFFFFFF
        h4 = (h4 + e) & 0xFFFFFFFF
    end
    return string.pack(">I4I4I4I4I4", h0, h1, h2, h3, h4)
end

--- The digest as lower-case hex.
function M.hex(msg)
    return (M.digest(msg):gsub(".", function(c) return string.format("%02x", c:byte()) end))
end

--- The first 16 bytes of the digest, stamped as a version 5, RFC 4122
--- variant UUID and written lower-case hyphenated — netd's interface and
--- network ids.
function M.uuid5(msg)
    local b = { M.digest(msg):byte(1, 16) }
    b[7] = (b[7] & 0x0f) | 0x50
    b[9] = (b[9] & 0x3f) | 0x80
    return string.format("%02x%02x%02x%02x-%02x%02x-%02x%02x-%02x%02x-%02x%02x%02x%02x%02x%02x",
        table.unpack(b))
end

return M
