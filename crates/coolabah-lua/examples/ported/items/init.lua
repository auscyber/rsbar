-- Ports sketchybar/items/init.lua.
--
-- Each `require` below only loads a setup function -- none of them run yet
-- (see bar.lua for why: LuaJIT cannot yield across `require()`'s internal
-- `pcall`, so no `coolabah.*` async call can happen during loading). This
-- module returns its own setup function, which init.lua calls directly from
-- its own top-level chunk once every module has finished loading.
local menus = require("items.menus")
local left = require("items.left")
local right = require("items.right")
local secondary = require("items.secondary")

return function()
	menus()
	left()
	right()
	secondary()
end
