-- A minimal rsbar config: one static item, one ticking once a second.
-- Structured the way the reference SketchyBar config is (begin_config /
-- declare / end_config / event_loop):
--   https://raw.githubusercontent.com/auscyber/dotfiles/refs/heads/master/sketchybar/init.lua
--
-- Run with:
--   cargo run -p rsbar-lua --bin rsbar-lua -- crates/rsbar-lua/examples/config.lua

local rsbar = require("rsbar")

rsbar.begin_config()

rsbar.bar({
	height = 28,
	color = 0xff1e1e2e,
})

-- Static: set once, never touched again.
local label = rsbar.add("hello", "left", {
	label = { text = "hello, rsbar", color = "#ffffff" },
})

-- Ticking: the daemon's own per-item clock (`update_freq` seconds) fires a
-- "routine" event, pushed here over the subscriber's own Mach port and
-- delivered as an ordinary callback rather than a re-spawned shell script.
local clock = rsbar.add("clock", "right", {
	update_freq = 1,
	label = { color = 0xffa6e3a1 },
})

clock:subscribe("routine", function(event)
	-- The "lua:" prefix is only here so a screenshot/log can tell this
	-- came from the callback below, not from some other script still
	-- configured on a leftover "clock" item from an earlier run.
	clock:set({ label = "lua: " .. os.date("%H:%M:%S") })
end)

rsbar.end_config()

print("rsbar-lua: config loaded, entering the event loop")
rsbar.event_loop()
