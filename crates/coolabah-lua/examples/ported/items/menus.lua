-- Ports sketchybar/items/menus.lua (the Apple-menu / live-menu-bar mirror).
--
-- The original populates up to 10 `menu.N` items with the frontmost app's
-- actual menu titles, by shelling out to a locally compiled helper
-- (helpers/menus/bin/menus, unrelated to SketchyBar or coolabah) and reading
-- its stdout back with SketchyBar's `sbar.exec(cmd, callback)`.
--
-- TODO(coolabah): coolabah-lua has no `sbar.exec`-equivalent at all -- `script`
-- and `click_script` (`ItemPatch`) are fire-and-forget shell commands with
-- no way to get their output back into Lua. Without that (and without the
-- helper binary, which is out of scope regardless), the live menu list
-- itself cannot be ported -- only the structural pieces below can: a
-- standing Apple-menu item, and the `swap_menus_and_spaces` custom event.
--
-- TODO(coolabah): the original also groups `/menu\..*/` under one shared
-- background via `sbar.add("bracket", {...})`. There is no bracket/grouping
-- request in `crates/protocol/src/lib.rs` -- `ItemPatch` colours one item at
-- a time, so a bracket's single shared background+corner-radius has no
-- equivalent; the nearest available thing is giving each item the same flat
-- `background_color`, which is what items/right.lua does for its groups.
--
-- Returns a setup function -- see bar.lua for why (LuaJIT/`require`/async).

return function()
	local apple_menu = coolabah.add("apple_menu", "left", {
		icon = { text = "" },
		label = { drawing = false },
		click_script = "coolabah trigger swap_menus_and_spaces",
	})

	-- A placeholder for the real, dynamically-populated `menu.1..10` items:
	-- present but hidden, same as the original's default state, standing in
	-- for "the live menu list" this port cannot fetch.
	local menu_placeholder = coolabah.add("menu.1", "left", {
		drawing = false,
		label = { text = "menu" },
	})

	local swap_watcher = coolabah.add("menu_watcher", "left", { drawing = false })

	local menus_visible = false
	swap_watcher:subscribe("swap_menus_and_spaces", function()
		menus_visible = not menus_visible
		menu_placeholder:set({ drawing = menus_visible })
	end)

	return { apple_menu = apple_menu, swap_watcher = swap_watcher }
end
