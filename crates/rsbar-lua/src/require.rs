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
//! Searches `package.preload`, then `package.path` (a Lua-file module,
//! `loadfile` plus a plain call as above), then `package.cpath` (a native
//! module: `package.loadlib(file, "luaopen_<name>")`, then a plain call on
//! the loader it returns). That is the real `require`'s search order minus
//! its fourth, "all in one" loader, which nothing in a `SketchyBar` config
//! uses.
//!
//! `package.loadlib`/native loading only works at all when the host `Lua`
//! was built with [`mlua::Lua::unsafe_new`] — mlua's safe [`mlua::Lua::new`]
//! stubs `package.loadlib` out and removes the C searchers, on purpose. A
//! config that guards its own native `require` in `pcall` (as `wm.lua`'s
//! `require("paneru")` does) degrades the same way either host chooses to
//! run it: a caught Lua error, not a crash.
//!
//! Loading the native module itself needs nothing special from *this*
//! binary on macOS: verified by building a trivial `-undefined
//! dynamic_lookup` module and `require`-ing it against the vendored,
//! statically-linked `LuaJIT` here — `ld64` already keeps a Rust binary's
//! linked-in C symbols (`lua_pushstring` and friends) visible to a `dlopen`
//! caller's flat-namespace lookup, in both debug and `lto = true` release
//! builds, with no `-export_dynamic` needed. `build.rs` still passes it,
//! belt-and-suspenders against a future strip/LTO change silently breaking
//! that.
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
local preload = package.preload

local function candidate_files(path_var, name)
    local as_path = name:gsub("%.", "/")
    local files = {}
    for pattern in path_var:gmatch("[^;]+") do
        files[#files + 1] = pattern:gsub("%?", as_path)
    end
    return files
end

local function find_existing(candidates)
    for _, candidate in ipairs(candidates) do
        local handle = io.open(candidate, "r")
        if handle then
            handle:close()
            return candidate
        end
    end
    return nil
end

-- Real Lua's own convention for a C module's entry point: the last
-- dot-separated component of its name, hyphens folded to underscores.
local function c_init_name(name)
    local last = name:match("[^.]+$") or name
    return "luaopen_" .. last:gsub("%-", "_")
end

local function finish(name, result)
    if result == nil then
        result = true
    end
    loaded[name] = result
    return result
end

function require(name)
    local existing = loaded[name]
    if existing ~= nil then
        return existing
    end

    local preloaded = preload[name]
    if preloaded ~= nil then
        return finish(name, preloaded(name))
    end

    local lua_tried = candidate_files(package.path, name)
    local lua_file = find_existing(lua_tried)
    if lua_file then
        local chunk, load_err = loadfile(lua_file)
        if not chunk then
            error(load_err, 0)
        end
        -- A plain Lua call, not the builtin `require`'s protected C call:
        -- this is what lets the chunk yield through an async `rsbar` call
        -- at its own top level.
        return finish(name, chunk(name))
    end

    local c_tried = candidate_files(package.cpath, name)
    local c_file = find_existing(c_tried)
    if c_file then
        local opener, load_err = package.loadlib(c_file, c_init_name(name))
        if not opener then
            error(load_err, 0)
        end
        return finish(name, opener(name))
    end

    local tried = {}
    for _, f in ipairs(lua_tried) do
        tried[#tried + 1] = "no file '" .. f .. "'"
    end
    for _, f in ipairs(c_tried) do
        tried[#tried + 1] = "no file '" .. f .. "'"
    end
    error(
        "module '" .. name .. "' not found:\n\t" .. table.concat(tried, "\n\t"),
        2
    )
end
"#;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use mlua::Lua;

    use super::install;

    /// A fresh, empty directory under the target dir, cleaned up on drop —
    /// there is no `tempfile` dependency in this workspace to reach for.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("rsbar-lua-require-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(dir: &ScratchDir, name: &str, contents: &str) {
        std::fs::write(dir.path().join(name), contents).unwrap();
    }

    #[test]
    fn finds_a_module_on_package_path() {
        let dir = ScratchDir::new();
        write(&dir, "greeting.lua", "return { hello = 'world' }");

        let lua = Lua::new();
        install(&lua).unwrap();
        lua.load(format!("package.path = '{}/?.lua'", dir.path().display()))
            .exec()
            .unwrap();

        let hello: String = lua.load("return require('greeting').hello").eval().unwrap();
        assert_eq!(hello, "world");
    }

    #[test]
    fn a_second_require_returns_the_cached_module_without_reloading() {
        let dir = ScratchDir::new();
        // A file that errors on a second load proves `loaded` is consulted.
        write(
            &dir,
            "once.lua",
            "if package.loaded.once then error('reloaded') end\nreturn { n = 1 }",
        );

        let lua = Lua::new();
        install(&lua).unwrap();
        lua.load(format!("package.path = '{}/?.lua'", dir.path().display()))
            .exec()
            .unwrap();

        lua.load("require('once'); return require('once')")
            .exec()
            .unwrap();
    }

    #[test]
    fn package_preload_is_consulted_before_touching_disk() {
        let lua = Lua::new();
        install(&lua).unwrap();
        lua.load(
            r"
            package.preload['synthetic'] = function() return { ok = true } end
            assert(require('synthetic').ok)
            ",
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn a_dotted_name_maps_to_a_nested_directory() {
        let dir = ScratchDir::new();
        std::fs::create_dir_all(dir.path().join("items")).unwrap();
        std::fs::write(
            dir.path().join("items").join("left.lua"),
            "return { side = 'left' }",
        )
        .unwrap();

        let lua = Lua::new();
        install(&lua).unwrap();
        lua.load(format!("package.path = '{}/?.lua'", dir.path().display()))
            .exec()
            .unwrap();

        let side: String = lua
            .load("return require('items.left').side")
            .eval()
            .unwrap();
        assert_eq!(side, "left");
    }

    #[test]
    fn a_missing_module_names_every_candidate_it_tried() {
        let lua = Lua::new();
        install(&lua).unwrap();
        lua.load("package.path = '/does/not/exist/?.lua'")
            .exec()
            .unwrap();

        let err = lua
            .load("return require('nowhere')")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("nowhere"), "{err}");
        assert!(err.contains("/does/not/exist/nowhere.lua"), "{err}");
    }

    /// The one test that actually exercises native-module loading: compiles
    /// a trivial `-undefined dynamic_lookup` Lua C module, then `require`s
    /// it against this crate's own vendored `LuaJIT`. `cc` is guaranteed
    /// present — this crate cannot build at all without a C compiler to
    /// build `LuaJIT` itself.
    ///
    /// Declares the handful of Lua C API functions it calls itself, by
    /// signature, rather than including the vendored `lua.h`/`lauxlib.h`:
    /// `mlua-sys` does not publish its copy's include directory to a
    /// downstream crate's own build (no `cargo:include=`), and hand-written
    /// `extern` declarations of a stable, documented C ABI are simpler than
    /// re-vendoring `LuaJIT` a second time here just to find them.
    #[test]
    fn requires_a_native_module_through_package_cpath() {
        let dir = ScratchDir::new();
        let source = dir.path().join("trivial.c");
        std::fs::write(
            &source,
            r#"
            typedef struct lua_State lua_State;
            typedef int (*lua_CFunction)(lua_State *L);

            extern int lua_pushstring(lua_State *L, const char *s);
            extern void lua_pushcclosure(lua_State *L, lua_CFunction fn, int n);
            extern void lua_createtable(lua_State *L, int narr, int nrec);
            extern void lua_setfield(lua_State *L, int idx, const char *k);

            static int trivial_hello(lua_State *L) {
                lua_pushstring(L, "hello from native module");
                return 1;
            }

            int luaopen_trivial(lua_State *L) {
                lua_createtable(L, 0, 1);
                lua_pushcclosure(L, trivial_hello, 0);
                lua_setfield(L, -2, "hello");
                return 1;
            }
            "#,
        )
        .unwrap();

        let dylib = dir.path().join("trivial.dylib");
        let status = std::process::Command::new("cc")
            .args(["-std=c99", "-dynamiclib", "-undefined", "dynamic_lookup"])
            .arg(&source)
            .arg("-o")
            .arg(&dylib)
            .status()
            .expect("cc must be available to build this crate at all");
        assert!(status.success(), "cc failed to build the trivial module");

        // Safety: single-threaded test, and the whole point is to exercise
        // the native-module path `Lua::new()` deliberately disables.
        let lua = unsafe { Lua::unsafe_new() };
        install(&lua).unwrap();
        lua.load(format!(
            "package.cpath = '{}/?.dylib'",
            dir.path().display()
        ))
        .exec()
        .unwrap();

        let greeting: String = lua
            .load("return require('trivial').hello()")
            .eval()
            .unwrap();
        assert_eq!(greeting, "hello from native module");
    }
}
