-- Port of https://github.com/auscyber/dotfiles' sketchybar/init.lua onto
-- rsbar. Structured the same way, module for module:
--   bar.lua, default.lua, items/{init,left,right,menus,secondary}.lua
-- plus items/spaces.lua standing in for the `wm` module the original loads
-- (see items/spaces.lua and colors.lua for why that one can't be a literal
-- port). The gaps found while porting are called out as `TODO(rsbar):`
-- comments at the point they bite, in whichever file that is.
--
-- Run against a running `rsbard` with:
--   cargo run -p rsbar-lua --bin rsbar-lua -- crates/rsbar-lua/examples/ported/init.lua
--
-- rsbar-lua's runner (crates/rsbar-lua/src/bin/rsbar_lua.rs) loads exactly
-- the one file it is given and does not add that file's own directory to
-- `package.path` the way SketchyBar's `sketchybarrc` does for `CONFIG_DIR`
-- (see the fetched sketchybar/sketchybarrc). So this does the same thing by
-- hand, assuming the invocation above (repo root as the working directory).
package.path = package.path
	.. ";crates/rsbar-lua/examples/ported/?.lua"
	.. ";crates/rsbar-lua/examples/ported/?/init.lua"

-- Deliberately global, exactly as the original's `sbar = require("sketchybar")`
-- is: every other module below reaches for `rsbar.*` without requiring it
-- itself.
rsbar = require("rsbar")

rsbar.begin_config()

-- `require` only loads each module's setup function here -- `rsbar.*` is
-- async and LuaJIT cannot yield across `require()`'s internal `pcall` (see
-- bar.lua), so every actual `rsbar.add`/`rsbar.bar`/etc. call happens below,
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

rsbar.end_config()

-- The original ends with a handful of `sbar.exec("sketchybar --subscribe
-- ...")` calls wiring up volume/power/battery/mouse events, then
-- `sbar.exec("sketchybar --update")`. Those `--subscribe` calls have no
-- port because they are not needed: volume_changed/power_source_changed
-- are already native rsbar events any item can `:subscribe` to directly
-- (see items/right.lua), and `rsbar.update_all()` is the direct
-- equivalent of `--update`.
rsbar.update_all()

print("rsbar-lua: ported config loaded, entering the event loop")
rsbar.event_loop()
