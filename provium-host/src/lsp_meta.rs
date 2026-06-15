//! LSP definition assets — `---@meta` stub + `.luarc.json` template.
//!
//! Test authors don't see these contents at runtime. They're shipped
//! so the `provium lsp-setup` subcommand can drop them into a test
//! root, giving the Lua Language Server (`lua-language-server` /
//! sumneko-lua) enough information to stop flagging
//! `test`/`provium`/`wait_until`/`json` as undefined globals.
//!
//! The meta file documents the most common API; less-trafficked
//! methods are typed as `any` rather than enumerated exhaustively
//! — the goal is silencing diagnostics, not building a full IDE
//! experience.

/// `---@meta` stub describing every global the test framework
/// installs. Written verbatim to `<dir>/.provium-meta/types.lua`.
pub const TYPES_LUA: &str = include_str!("../meta/types.lua");

/// `.luarc.json` template pointing the LSP at the meta directory.
pub const LUARC_JSON: &str = include_str!("../meta/luarc.json");
