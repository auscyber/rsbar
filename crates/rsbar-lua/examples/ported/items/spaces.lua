-- Approximates the workspaces widget the real config gets from its `wm`
-- Lua module (loaded via `paneru`, the user's window manager). `wm` is not
-- part of the `auscyber/dotfiles` sketchybar directory -- it is injected
-- onto the Lua path at runtime alongside `colors` (see colors.lua) -- so its
-- actual source is not fetchable, and this is a rebuild from what it must
-- do, not a literal port.
--
-- rsbar already has native events for exactly this idea: `space_changed`
-- (display, space) and `space_windows_changed` (space), both in
-- crates/protocol/src/event.rs. That makes this one of the closer
-- approximations in this port rather than a stub: real items, highlighted
-- from a real event.
--
-- TODO(rsbar): there is no way to *discover* how many spaces/workspaces
-- exist (no `Query::Spaces` in crates/protocol/src/lib.rs), so the count
-- below is a guess, not read from anything live.
-- TODO(rsbar): there is also no request to *switch* to a space -- `Request`
-- has no such verb -- so unlike the original, clicking one of these items
-- cannot jump there; a real config would have to shell out to its WM's own
-- CLI (unrelated to rsbar) from `click_script` instead.
-- Returns a setup function -- see bar.lua for why (LuaJIT/`require`/async).
local colors = require("colors")

return function()
	local space_count = 5
	local spaces = {}

	for i = 1, space_count do
		spaces[i] = rsbar.add("space." .. i, "left", {
			label = { text = tostring(i) },
			background_color = colors.transparent,
			padding_left = 4,
			padding_right = 4,
		})
	end

	for i, space in ipairs(spaces) do
		space:subscribe("space_changed", function(event)
			if event.space == i then
				space:set({ background_color = colors.background })
			else
				space:set({ background_color = colors.transparent })
			end
		end)
	end

	return spaces
end
