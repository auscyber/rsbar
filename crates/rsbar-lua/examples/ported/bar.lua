-- Ports sketchybar/bar.lua.
--
-- Returns a setup function rather than calling `rsbar.bar` at require time:
-- `rsbar.*` calls are async (they await the daemon over IPC), and LuaJIT
-- cannot yield across the `pcall` that `require()` runs a module's chunk
-- inside -- so every module in this port returns a plain function/table
-- from its `require`, and init.lua calls the setup functions itself, from
-- its own top-level chunk (not nested inside another `require`).
local colors = require("colors")

return function()
	rsbar.bar({
		-- SketchyBar's `position = "top"` is rsbar's `edge`.
		edge = "top",
		height = 40,
		color = colors.transparent,
		-- TODO(rsbar): `BarPatch` (crates/protocol/src/lib.rs) has no
		-- `display` field -- no way to say "span every display" vs. "just
		-- the main one".
		-- TODO(rsbar): `BarPatch` also has no bar-level `padding_right`/
		-- `padding_left` (end-of-bar padding); only items have padding.
	})
end
