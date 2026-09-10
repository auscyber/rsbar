-- Port of https://github.com/auscyber/dotfiles' sketchybar/init.lua onto
-- coolabah. Structured the same way, module for module:
--   bar.lua, default.lua, items/{init,left,right,menus,secondary}.lua
-- plus items/spaces.lua standing in for the `wm` module the original loads
-- (see items/spaces.lua and colors.lua for why that one can't be a literal
-- port). The gaps found while porting are called out as `TODO(coolabah):`
-- comments at the point they bite, in whichever file that is.
--
-- Run against a running `coolabah` with:
--   cargo run -p coolabah-lua --bin coolabah-lua -- crates/coolabah-lua/examples/ported/init.lua
--
-- coolabah-lua's runner (crates/coolabah-lua/src/bin/coolabah_lua.rs) loads exactly
-- the one file it is given and does not add that file's own directory to
-- `package.path` the way SketchyBar's `sketchybarrc` does for `CONFIG_DIR`
-- (see the fetched sketchybar/sketchybarrc). So this does the same thing by
-- hand, assuming the invocation above (repo root as the working directory).
package.path = package.path
	.. ";crates/coolabah-lua/examples/ported/?.lua"
	.. ";crates/coolabah-lua/examples/ported/?/init.lua"

-- Deliberately global, exactly as the original's `sbar = require("sketchybar")`
-- is: every other module below reaches for `coolabah.*` without requiring it
-- itself.
coolabah = require("coolabah")

coolabah.begin_config()

-- `require` only loads each module's setup function here -- `coolabah.*` is
-- async and LuaJIT cannot yield across `require()`'s internal `pcall` (see
-- bar.lua), so every actual `coolabah.add`/`coolabah.bar`/etc. call happens below,
-- from this top-level chunk, once loading is done.
local setup_bar = require("bar")
require("default") -- pure helpers, no side effect to run
local setup_items = require("items")

setup_bar()
setup_items()

-- Window-manager-specific bar integration -- see items/spaces.lua.
local loaded, setup_spaces = pcall(require, "items.spaces")
if loaded then
	setup_spaces()
end

coolabah.end_config()

-- The original ends with a handful of `sbar.exec("sketchybar --subscribe
-- ...")` calls wiring up volume/power/battery/mouse events, then
-- `sbar.exec("sketchybar --update")`. Those `--subscribe` calls have no
-- port because they are not needed: volume_changed/power_source_changed
-- are already native coolabah events any item can `:subscribe` to directly
-- (see items/right.lua), and `coolabah.update_all()` is the direct
-- equivalent of `--update`.
coolabah.update_all()

print("coolabah-lua: ported config loaded, entering the event loop")
coolabah.event_loop()
