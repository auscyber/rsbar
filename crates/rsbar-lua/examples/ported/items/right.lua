-- Ports sketchybar/items/right.lua.
--
-- Returns a setup function -- see bar.lua for why (LuaJIT/`require`/async).
local colors = require("colors")
local icons = require("icons")
local settings = require("settings")
local defaults = require("default")

-- rsbar-lua's runner never sets up `package.path`/`arg` relative to the
-- script it loaded (unlike SketchyBar's own `sketchybarrc`, which does this
-- itself before requiring anything) -- init.lua does the same thing by hand
-- for `require`, and this plugin path assumes the same "run from the repo
-- root" invocation the task's run command uses.
local plugin_dir = "crates/rsbar-lua/examples/ported/plugins"

return function()

-- Direct alias mirrors -- ItemPatch.alias ("Owner,Name") is exactly what the
-- original's `sbar.add("alias", "Owner,Name", {...})` does, just spelled as
-- an ordinary item whose name has to be rsbar-legal (no comma/space:
-- ItemName only allows letters, digits, `_`, `-`, `.` --
-- crates/protocol/src/lib.rs), with the mirrored owner/name pair moved into
-- `alias` instead of being the item's own name.
rsbar.add(
	"amphetamine",
	"right",
	defaults.with_defaults({ alias = "Amphetamine,Amphetamine" })
)

rsbar.add(
	"focus_modes",
	"right",
	defaults.with_defaults({
		alias = "Control Centre,FocusModes",
		label_color = colors.yellow,
	})
)
-- TODO(rsbar): the original's negative `padding_left = -15` (used to tuck
-- SketchyBar's own alias rendering closer to its neighbour) has no
-- equivalent guarantee here -- rsbar accepts negative padding, but there is
-- nothing that promises alias rendering lines up with SketchyBar's the same
-- way, since rsbar is not API-compatible with it.

-- Clock: the original polls via plugins/clock.sh; rsbar's own per-item timer
-- (`update_freq`) plus a `routine` subscription replaces the shell script
-- entirely -- exactly the idiom `examples/config.lua` demonstrates.
local clock = rsbar.add(
	"clock",
	"right",
	defaults.with_defaults({
		icon = { text = icons.clock },
		update_freq = 10,
		padding_right = 10,
		click_script = "open -a 'Notification Center'",
	})
)
clock:subscribe("routine", function()
	clock:set({ label = os.date("%d/%m %I:%M %p") })
end)

-- Volume: the original's volume.sh reacts to SketchyBar's `volume_change`
-- event and $INFO. rsbar has a native `volume_changed` event carrying
-- `volume` (crates/protocol/src/event.rs) -- subscribed directly, no script.
local volume = rsbar.add("volume", "right", defaults.with_defaults({}))

local function volume_icon(v)
	if v >= 60 then
		return "󰕾"
	elseif v >= 30 then
		return "󰖀"
	elseif v >= 1 then
		return "󰕿"
	else
		return "󰖁"
	end
end

volume:subscribe("volume_changed", function(event)
	volume:set({ icon = volume_icon(event.volume), label = event.volume .. "%" })
end)
-- TODO(rsbar): the original also drives a `sbar.add("slider", ...)` in a
-- `popup` opened by clicking this item, to set the volume back. `ItemPatch`
-- has no `popup` and there is no `slider`/`graph` item kind at all, and
-- there is no request to *change* the system volume (only to be told when
-- it changed) -- so the click-to-open-a-volume-slider popup has no port;
-- this item is read-only.

-- Power source: the original's power.sh reports AC wattage via `pmset`.
-- rsbar's native `power_source_changed` event only carries an AC/BATTERY/
-- UNKNOWN enum, not wattage (`PowerSource` in crates/protocol/src/event.rs)
-- -- there is no wattage source to port, so this approximates "on AC or
-- not" instead of showing watts.
local power = rsbar.add(
	"power",
	"right",
	defaults.with_defaults({ icon = { drawing = false } })
)
power:subscribe("power_source_changed", function(event)
	if event.power_source == "AC" then
		power:set({ label = "AC", drawing = true })
	else
		power:set({ drawing = false })
	end
end)
-- TODO(rsbar): no wattage figure is available to show even approximately.

-- Battery: no native battery-percentage source exists at all (see
-- crates/rsbar/src/sources -- there is brightness/volume/wifi/power/media/
-- spaces/workspace/displays/mouse, no battery), so this is the one item
-- that still needs a polling shell script, same shape as the original.
local battery = rsbar.add(
	"battery",
	"right",
	defaults.with_defaults({
		label = { drawing = false },
		update_freq = 120,
		script = plugin_dir .. "/battery.sh",
	})
)

-- Overflow: the original mirrors every other mirrorable menu-bar item into a
-- popup, discovered via `sketchybar --query default_menu_items`. rsbar's
-- `rsbar.query.menu_items()` (Query::MenuItems in crates/protocol/src/lib.rs)
-- is the direct native equivalent for *discovering* them.
--
-- TODO(rsbar): there is no `popup` to put the discovered aliases into --
-- creating one alias item per entry directly on the bar would just spill
-- every menu-bar icon across the bar instead of collapsing them, which is
-- not the nearest equivalent of a popup, it is a different, worse design.
-- So this only reports the count rsbar can see, in the item other rsbar
-- items already use to summarise something the popup would have expanded.
local overflow = rsbar.add(
	"overflow",
	"right",
	defaults.with_defaults({
		icon = { text = "...", font = defaults.font_string("Bold", 14.0) },
		label = { drawing = false },
		padding_left = 6,
		padding_right = 6,
	})
)

local ok, menu_items = pcall(function()
	return rsbar.query.menu_items()
end)
if ok then
	overflow:set({ label = "+" .. tostring(#menu_items) })
end

-- Visual "bracket": ItemPatch has no bracket/grouping request
-- (crates/protocol/src/lib.rs has none), so the nearest available stand-in
-- for the original's two `sbar.add("bracket", {...}, {...})` groups is
-- giving each member item the same flat `background_color` -- there is no
-- single shared background spanning them, or one shared corner radius.
local utils_background = colors.yellow
for _, item in ipairs({ clock, power, battery, volume }) do
	item:set({ background_color = utils_background })
end

end
