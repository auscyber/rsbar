//! A `require` that lets a module's own top level call an async `rsbar`
//! function.
//!
//! A real `SketchyBar` config is several files, wired together with
//! `require("bar")`, `require("items")` and so on, each calling `sbar.add`
//! and friends at its own top level — not inside a function, just as the file
//! runs. Every one of those calls is async here (see `api::install`): the
//! whole script runs as one Lua coroutine, and a call like `rsbar.bar({...})`
//! suspends that coroutine (`coroutine.yield`) while its dispatcher future is
//! pending, then resumes it once the daemon answers.
//!
//! `LuaJIT`'s built-in `require` cannot sit between that yield and the
//! coroutine's resume point: it runs the module chunk behind a C stack frame
//! that was never written to support yielding across it (unlike `pcall`,
//! `xpcall` and a handful of others, which `LuaJIT` extends to allow exactly
//! this). The result is not a clean error a config could work around —
//! `attempt to yield across a C-call boundary` aborts the whole load, and it
//! fires on the very first `require` a real config makes.
//!
//! The fix is this module: a `require` written entirely in Lua. `loadfile`
//! is still a C function, but it only ever *returns* a chunk — the chunk
//! itself is then called as a plain Lua value, which is just another Lua
//! stack frame, and yields through it exactly as the rest of the script does.
//!
//! Deliberately narrower than the real thing: only `package.path`-style
//! Lua-file modules resolve. There is no `package.cpath`/`loadlib` search, so
//! a config's own `require` of a native module still fails — the same
//! contract violation `require("rsbar")` would have without loading it,
//! just for a different missing string.
//!
//! Not wired into [`crate::api::install`] itself, since only a host that owns
//! the whole Lua state (the vendored bin) can safely replace a *global* this
//! invasively — an embedder sharing a foreign interpreter's Lua state (the
//! `module`-feature `cdylib`) should decide for itself whether it wants this
//! too.

/// Replaces the global `require` with the pure-Lua one described above.
///
/// # Errors
///
/// Returns a Lua error if the shim itself fails to compile or run — never
/// for anything a config's own later `require` calls do, which surface as
/// ordinary Lua runtime errors instead.
pub fn install(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.load(SHIM).set_name("=(rsbar require shim)").exec()
}

const SHIM: &str = r#"
local loaded = package.loaded

local function candidate_files(name)
    local as_path = name:gsub("%.", "/")
    local files = {}
    for pattern in package.path:gmatch("[^;]+") do
        files[#files + 1] = pattern:gsub("%?", as_path)
    end
    return files
end

function require(name)
    local existing = loaded[name]
    if existing ~= nil then
        return existing
    end

    local tried = candidate_files(name)
    local path
    for _, candidate in ipairs(tried) do
        local handle = io.open(candidate, "r")
        if handle then
            handle:close()
            path = candidate
            break
        end
    end

    if not path then
        error(
            "module '" .. name .. "' not found:\n\tno file '"
                .. table.concat(tried, "'\n\tno file '") .. "'",
            2
        )
    end

    local chunk, load_err = loadfile(path)
    if not chunk then
        error(load_err, 0)
    end

    -- A plain Lua call, not the builtin `require`'s protected C call: this
    -- is what lets the chunk yield through an async `rsbar` call at its own
    -- top level.
    local result = chunk(name)
    if result == nil then
        result = true
    end
    loaded[name] = result
    return result
end
"#;
