-- Ports sketchybar/default.lua, which calls `sbar.default({...})` to set a
-- config-wide style every later `sbar.add`/`:set` is implicitly layered on
-- top of.
--
-- TODO(rsbar): there is no `rsbar.default` / style inheritance at all --
-- `ItemPatch` (crates/protocol/src/lib.rs) has no notion of "unset" falling
-- back to a config default, only "not part of this patch" (`None`) meaning
-- "leave whatever is already on the item". So instead of a global call, this
-- module returns a table of the same default values plus a `with_defaults`
-- helper that every item in this port merges in explicitly, item by item.
local colors = require("colors")
local settings = require("settings")

-- rsbar's icon_font/label_font are one flat "Family:Style:Size" string
-- rather than SketchyBar's `{ family = ..., style = ..., size = ... }`
-- sub-table.
local function font_string(style_name, size)
	return settings.font.text .. ":" .. settings.font.style_map[style_name] .. ":" .. size
end

local defaults = {
	padding_left = settings.paddings,
	padding_right = settings.paddings,
	icon_font = font_string("Regular", 17.0),
	icon_color = colors.foreground,
	label_font = font_string("Bold", 14.0),
	label_color = colors.foreground,
}

-- TODO(rsbar): the original's `icon = { padding_left = 4, padding_right = 2 }`
-- and `label = { padding_right = 2 }` have no home here -- `ItemPatch` only
-- has one `padding_left`/`padding_right` pair for the whole item, not
-- separate padding around the icon and the label within it.

local function with_defaults(opts)
	opts = opts or {}
	for key, value in pairs(defaults) do
		if opts[key] == nil then
			opts[key] = value
		end
	end
	return opts
end

return { defaults = defaults, with_defaults = with_defaults, font_string = font_string }
