//! A `require` that lets a module's own top level call an async `rsbar`
//! function — and that says which files it loaded, so they can be watched.
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
//! # The trampoline
//!
//! [`install`] replaces `require` with one written in Lua. What it is not is a
//! second, competing module system: it walks `package.preload` and then
//! `package.searchers` (`package.loaders` on `LuaJIT`) exactly as the real
//! `require` does, and every searcher already there keeps working. The only
//! difference is the last step. A searcher *returns* a loader; the built-in
//! `require` then calls it behind that C frame, and this calls it as a plain
//! Lua value instead — just another Lua stack frame, which yields through
//! exactly as the rest of the script does.
//!
//! That is what makes it a trampoline rather than a rewrite: the search is
//! whatever `package.searchers` says it is, and only the bounce off the loader
//! happens here.
//!
//! # Which files a config is made of
//!
//! [`install_tracked`] adds one searcher of its own at the front, written in
//! Rust. It resolves a name against `package.path` like the built-in Lua-file
//! searcher, but it hands the resolved path to a [`Modules`] on the way past.
//! A host therefore learns every file a config actually loaded, by loading it,
//! rather than by guessing from a directory listing — a `colors.lua` two
//! directories away is watched and a `notes.md` beside the config is not.
//!
//! ## Watches come and go with the config
//!
//! The set is generational, and deliberately the same shape the daemon uses
//! for items across a reload: [`Modules::begin`] marks every known file stale,
//! the config run re-`require`s whatever it still wants (which clears the
//! mark), and [`Modules::end`] hands back what is still stale — the files this
//! generation stopped using, whose watches the host should drop. Nothing is
//! torn down and rebuilt in between, so a watch on a file both generations use
//! is never interrupted, and a reload cannot miss a change that lands while it
//! is running.
//!
//! [`Modules::directories`] rather than [`Modules::paths`] is usually what a
//! watcher wants, for the reason `sources::config` already watches a directory
//! and not a file: an editor saves by writing a temporary file and renaming it
//! over the original, which replaces the inode a file watch is holding — the
//! watch survives and never fires again.
//!
//! # Native modules
//!
//! `package.loadlib` and the C searcher only work at all when the host `Lua`
//! was built with [`mlua::Lua::unsafe_new`] — mlua's safe [`mlua::Lua::new`]
//! stubs `package.loadlib` out and removes the C searchers, on purpose. A
//! config that guards its own native `require` in `pcall` (as `wm.lua`'s
//! `require("paneru")` does) degrades the same way either host chooses to run
//! it: a caught Lua error, not a crash. Walking `package.searchers` is what
//! makes that work without this module knowing anything about it.
//!
//! Loading the native module itself needs nothing special from *this*
//! binary on macOS: verified by building a trivial `-undefined
//! dynamic_lookup` module and `require`-ing it against this crate's vendored,
//! statically-linked `LuaJIT` — `ld64` already keeps a Rust binary's linked-in
//! C symbols (`lua_pushstring` and friends) visible to a `dlopen` caller's
//! flat-namespace lookup, in both debug and `lto = true` release builds, with
//! no `-export_dynamic` needed. `build.rs` still passes it, belt-and-suspenders
//! against a future strip/LTO change silently breaking that.
//!
//! Not wired into [`crate::api::install`] itself, since only a host that owns
//! the whole Lua state (the vendored bin, and the daemon) can safely replace a
//! *global* this invasively — an embedder sharing a foreign interpreter's Lua
//! state (the `module`-feature `cdylib`) should decide for itself whether it
//! wants this too.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use mlua::{Function, Lua, MultiValue, Table, Value};

/// One tracked module.
struct Entry {
    path: PathBuf,
    /// Whether the generation now running has asked for it.
    ///
    /// Cleared by [`Modules::begin`] and set by every resolution, so after a
    /// run the entries still clear are exactly what the new config dropped.
    /// The same mark, for the same reason, as the daemon's `Stale` on an item.
    fresh: bool,
}

/// Every file this Lua state's `require` has loaded.
///
/// Shared with whoever watches them: cloning is an [`Arc`] bump, and a clone
/// handed to a watcher thread sees what the Lua thread records. It outlives any
/// one `Lua` state on purpose — that is what lets a reload build a fresh state
/// without the watches flickering.
#[derive(Clone, Default)]
pub struct Modules(Arc<Mutex<HashMap<String, Entry>>>);

impl Modules {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `name` resolved to `path`.
    fn record(&self, name: &str, path: PathBuf) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), Entry { path, fresh: true });
    }

    /// Marks every known module stale, before a config run.
    ///
    /// Nothing is dropped here. The run might fail, and a broken edit should
    /// leave the watches that were working in place rather than blinding the
    /// daemon to the file the user is about to fix.
    pub fn begin(&self) {
        for entry in self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values_mut()
        {
            entry.fresh = false;
        }
    }

    /// Forgets every module the run just finished did not ask for, and says
    /// which files they were — the watches a host should now drop.
    ///
    /// Call this only after a run that *succeeded*. After a failed one, call
    /// [`Modules::keep`] instead: a config that died half way through has not
    /// stopped wanting the files it never reached.
    pub fn end(&self) -> Vec<PathBuf> {
        let mut known = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let dropped: Vec<String> = known
            .iter()
            .filter(|(_, entry)| !entry.fresh)
            .map(|(name, _)| name.clone())
            .collect();
        dropped
            .iter()
            .filter_map(|name| known.remove(name).map(|entry| entry.path))
            .collect()
    }

    /// Lifts the stale mark without dropping anything — what a failed run
    /// leaves behind.
    pub fn keep(&self) {
        for entry in self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values_mut()
        {
            entry.fresh = true;
        }
    }

    /// Every file currently loaded, in no particular order.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// The directories those files live in, deduplicated.
    ///
    /// What a watcher should actually watch — see the module docs on renames.
    /// A set rather than a list because a config's dozen `items/*.lua` are one
    /// directory, and registering it twelve times is twelve callbacks per save.
    #[must_use]
    pub fn directories(&self) -> BTreeSet<PathBuf> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter_map(|entry| entry.path.parent().map(Path::to_path_buf))
            .collect()
    }

    /// Whether `path` is one of the files a reload should react to.
    ///
    /// Compares the file name as well as the whole path, because an atomic
    /// rename is reported against the temporary file the editor used, not the
    /// target — the same comparison `sources::config` makes for the config
    /// itself.
    #[must_use]
    pub fn watches(&self, path: &Path) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|entry| {
                entry.path == path
                    || (entry.path.file_name() == path.file_name()
                        && entry.path.parent() == path.parent())
            })
    }

    /// How many modules are tracked. Only the tests ask.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Replaces the global `require` with the trampoline described above.
///
/// # Errors
///
/// Returns a Lua error if the shim itself fails to compile or run — never
/// for anything a config's own later `require` calls do, which surface as
/// ordinary Lua runtime errors instead.
pub fn install(lua: &Lua) -> mlua::Result<()> {
    lua.load(SHIM).set_name("=(rsbar require shim)").exec()
}

/// As [`install`], plus a searcher that records every file it resolves.
///
/// # Errors
///
/// Same as [`install`], plus a failure to reach `package.searchers` (or
/// `package.loaders`) at all, which would mean a host that stripped the
/// `package` library.
pub fn install_tracked(lua: &Lua) -> mlua::Result<Modules> {
    let modules = Modules::new();
    install_tracked_with(lua, modules.clone())?;
    Ok(modules)
}

/// As [`install_tracked`], recording into a [`Modules`] the caller already
/// has — which is how one set outlives the several `Lua` states a sequence of
/// reloads builds.
///
/// # Errors
///
/// Same as [`install_tracked`].
pub fn install_tracked_with(lua: &Lua, modules: Modules) -> mlua::Result<()> {
    install(lua)?;
    let searchers = searcher_table(lua)?;
    // At the front, so this is the searcher that resolves a plain Lua module
    // and therefore the one that gets to record it. `package.preload` is
    // consulted by the shim before the loop either way, so nothing is
    // displaced by going first.
    searchers.raw_insert(1, tracking_searcher(lua, modules)?)
}

/// `package.searchers`, or `package.loaders` on 5.1 and `LuaJIT`.
fn searcher_table(lua: &Lua) -> mlua::Result<Table> {
    let package: Table = lua.globals().get("package")?;
    if let Ok(searchers) = package.get::<Table>("searchers") {
        return Ok(searchers);
    }
    package.get::<Table>("loaders")
}

/// The searcher that resolves a module *and* remembers where it came from.
///
/// Returns the two values a searcher returns on a hit — the loader, and the
/// path as its extra argument — and on a miss the string real Lua expects, so
/// `require` moves on to the next searcher with this one's reason recorded.
fn tracking_searcher(lua: &Lua, modules: Modules) -> mlua::Result<Function> {
    lua.create_function(move |lua, name: String| {
        let Some(path) = resolve(lua, &name)? else {
            let reason = lua.create_string(format!("\n\tno rsbar-tracked file for '{name}'"))?;
            return Ok(MultiValue::from_iter([Value::String(reason)]));
        };

        let source = std::fs::read(&path).map_err(|err| {
            mlua::Error::RuntimeError(format!("cannot read {}: {err}", path.display()))
        })?;
        // `@` is Lua's own marker for "this name is a file", which is what
        // makes a traceback read `items/left.lua:12` rather than quoting the
        // whole chunk back at the reader.
        let loader = lua
            .load(source)
            .set_name(format!("@{}", path.display()))
            .into_function()?;

        // After the load, so a file that does not compile is a plain Lua
        // error and not also a watch on something that never worked.
        let recorded = lua.create_string(path.to_string_lossy().as_bytes())?;
        modules.record(&name, path);
        Ok(MultiValue::from_iter([
            Value::Function(loader),
            Value::String(recorded),
        ]))
    })
}

/// The first existing file `package.path` offers for `name`.
///
/// Read out of the live `package` table rather than cached, because a config
/// may extend `package.path` before requiring its own modules — which is
/// exactly what the host does for it.
fn resolve(lua: &Lua, name: &str) -> mlua::Result<Option<PathBuf>> {
    let package: Table = lua.globals().get("package")?;
    let template: String = package.get("path")?;
    let as_path = name.replace('.', "/");
    Ok(template
        .split(';')
        .filter(|pattern| !pattern.is_empty())
        .map(|pattern| PathBuf::from(pattern.replace('?', &as_path)))
        .find(|candidate| candidate.is_file()))
}

const SHIM: &str = r#"
local loaded = package.loaded
local preload = package.preload

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

    -- `searchers` on 5.2+, `loaders` on 5.1 and LuaJIT. Whatever is in it is
    -- what searches: a config that installs one of its own is honoured, and
    -- the built-in Lua-file and C searchers are still there behind ours.
    local searchers = package.searchers or package.loaders
    local tried = {}
    for index = 1, #searchers do
        local loader, extra = searchers[index](name)
        if type(loader) == "function" then
            -- A plain Lua call, not the builtin `require`'s protected C call:
            -- this is what lets the chunk yield through an async `rsbar` call
            -- at its own top level.
            return finish(name, loader(name, extra))
        elseif type(loader) == "string" then
            tried[#tried + 1] = loader
        end
    end

    error("module '" .. name .. "' not found:" .. table.concat(tried), 2)
end
"#;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use mlua::Lua;

    use super::{Modules, install, install_tracked};

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

    /// A state whose `package.path` is `dir`, with the trampoline installed.
    fn hosted(dir: &ScratchDir) -> Lua {
        let lua = Lua::new();
        install(&lua).unwrap();
        lua.load(format!("package.path = '{}/?.lua'", dir.path().display()))
            .exec()
            .unwrap();
        lua
    }

    /// As [`hosted`], plus the recording searcher.
    fn tracked(dir: &ScratchDir) -> (Lua, Modules) {
        let lua = Lua::new();
        let modules = install_tracked(&lua).unwrap();
        lua.load(format!("package.path = '{}/?.lua'", dir.path().display()))
            .exec()
            .unwrap();
        (lua, modules)
    }

    #[test]
    fn finds_a_module_on_package_path() {
        let dir = ScratchDir::new();
        write(&dir, "greeting.lua", "return { hello = 'world' }");

        let lua = hosted(&dir);
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

        let lua = hosted(&dir);
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

        let lua = hosted(&dir);
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

    /// A searcher a config installs itself is still consulted: the trampoline
    /// walks `package.searchers`, it does not replace it.
    #[test]
    fn a_searcher_the_config_adds_is_honoured() {
        let lua = Lua::new();
        install(&lua).unwrap();
        lua.load(
            r"
            local searchers = package.searchers or package.loaders
            searchers[#searchers + 1] = function(name)
                if name ~= 'invented' then return '\n\tnot mine' end
                return function() return { made_up = true } end
            end
            assert(require('invented').made_up)
            ",
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn a_tracked_require_records_the_file_it_loaded() {
        let dir = ScratchDir::new();
        write(&dir, "colors.lua", "return { red = 0xffff0000 }");

        let (lua, modules) = tracked(&dir);
        assert!(modules.is_empty(), "nothing is tracked before a require");

        lua.load("require('colors')").exec().unwrap();

        assert_eq!(modules.paths(), vec![dir.path().join("colors.lua")]);
        assert_eq!(
            modules.directories(),
            std::iter::once(dir.path().to_path_buf()).collect()
        );
        assert!(modules.watches(&dir.path().join("colors.lua")));
        assert!(!modules.watches(&dir.path().join("unrelated.lua")));
    }

    /// The module still works, tracked or not: the searcher hands back a real
    /// loader, and its return value is the module.
    #[test]
    fn a_tracked_module_returns_its_value_as_usual() {
        let dir = ScratchDir::new();
        write(&dir, "colors.lua", "return { red = 'crimson' }");

        let (lua, _modules) = tracked(&dir);
        let red: String = lua.load("return require('colors').red").eval().unwrap();
        assert_eq!(red, "crimson");
    }

    /// A traceback names the file, not the source text — which is what `@`
    /// on the chunk name buys.
    #[test]
    fn an_error_in_a_tracked_module_names_its_file() {
        let dir = ScratchDir::new();
        write(&dir, "broken.lua", "error('deliberate')");

        let (lua, _modules) = tracked(&dir);
        let err = lua
            .load("require('broken')")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(err.contains("broken.lua"), "{err}");
    }

    /// The generational sweep: what the new config stopped requiring is what
    /// stops being watched, and everything it still requires keeps its watch
    /// without interruption.
    #[test]
    fn a_module_the_next_generation_stops_requiring_is_swept() {
        let dir = ScratchDir::new();
        write(&dir, "kept.lua", "return {}");
        write(&dir, "dropped.lua", "return {}");

        let modules = Modules::new();

        // Generation one wants both.
        let lua = Lua::new();
        install(&lua).unwrap();
        super::searcher_table(&lua)
            .unwrap()
            .raw_insert(1, super::tracking_searcher(&lua, modules.clone()).unwrap())
            .unwrap();
        lua.load(format!("package.path = '{}/?.lua'", dir.path().display()))
            .exec()
            .unwrap();
        lua.load("require('kept'); require('dropped')")
            .exec()
            .unwrap();
        assert_eq!(modules.len(), 2);

        // Generation two is a fresh state, as a reload is, and wants one.
        modules.begin();
        let lua = Lua::new();
        install(&lua).unwrap();
        super::searcher_table(&lua)
            .unwrap()
            .raw_insert(1, super::tracking_searcher(&lua, modules.clone()).unwrap())
            .unwrap();
        lua.load(format!("package.path = '{}/?.lua'", dir.path().display()))
            .exec()
            .unwrap();
        lua.load("require('kept')").exec().unwrap();

        assert_eq!(
            modules.end(),
            vec![dir.path().join("dropped.lua")],
            "only what the new generation left out"
        );
        assert_eq!(modules.paths(), vec![dir.path().join("kept.lua")]);
    }

    /// A failed run has not stopped wanting the files it never reached, so
    /// nothing is swept and the watches that were working stand.
    #[test]
    fn a_failed_run_keeps_every_watch_it_had() {
        let dir = ScratchDir::new();
        write(&dir, "kept.lua", "return {}");

        let (lua, modules) = tracked(&dir);
        lua.load("require('kept')").exec().unwrap();

        modules.begin();
        modules.keep();
        assert!(modules.end().is_empty(), "a failed run drops nothing");
        assert_eq!(modules.len(), 1);
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
