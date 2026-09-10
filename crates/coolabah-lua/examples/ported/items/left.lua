-- Ports sketchybar/items/left.lua (chevron + front_app).
--
-- Returns a setup function -- see bar.lua for why (LuaJIT/`require`/async).
local icons = require("icons")
local defaults = require("default")

return function()
	-- The original's `chevron` swaps position with `front_app` once a WM
	-- module reports the last space item (`reorder_left_items`), and toggles
	-- the menu bar via a custom `swap_menus_and_spaces` event.
	--
	-- TODO(coolabah): there is no `--move` / re-ordering request in `Request`
	-- (crates/protocol/src/lib.rs) -- an item's place in its position bucket
	-- is fixed by insertion, so `reorder_left_items` has no port.
	local chevron = coolabah.add(
		"chevron",
		"left",
		defaults.with_defaults({
			icon = { text = icons.chevron },
			label = { drawing = false },
			padding_left = 0,
			padding_right = 0,
			-- Custom events are real in coolabah (`Kind::Custom`): this fires
			-- the same one items/menus.lua subscribes to.
			click_script = "coolabah trigger swap_menus_and_spaces",
		})
	)

	-- TODO(coolabah): `ItemPatch.label` has no `drawing` flag distinct from the
	-- item's own `drawing` -- `label = { drawing = false }` above only
	-- affects coolabah in that no `label` text was ever set (an unset label
	-- already renders as nothing), it does not toggle visibility
	-- independently of the icon the way SketchyBar's does.

	local front_app = coolabah.add(
		"front_app",
		"left",
		defaults.with_defaults({
			icon = { drawing = false },
			click_script = "coolabah trigger swap_menus_and_spaces",
		})
	)

	-- The original's `front_app.sh` plugin is a no-op stub -- the real title
	-- comes from the `wm` module's own event. coolabah already has a native
	-- `front_app_switched` event carrying `app`
	-- (crates/protocol/src/event.rs), so this subscribes to it directly
	-- instead of shelling out at all.
	front_app:subscribe("front_app_switched", function(event)
		front_app:set({ label = event.app })
	end)

	return { chevron = chevron, front_app = front_app }
end
