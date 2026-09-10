-- The real dotfiles config never ships a `colors.lua` of its own: it is
-- injected onto the Lua `package.path` at runtime by `paneru` (the user's
-- window manager), alongside `wm`. Neither is fetchable from the
-- `auscyber/dotfiles` repo, so this is a local stand-in with the same keys
-- the rest of the port reaches for (`transparent`, `foreground`, `yellow`,
-- `background`, `black`). Values are plain ARGB, the form
-- `coolabah`'s colour parser already accepts.
return {
	transparent = 0x00000000,
	background = 0xff1e1e2e,
	foreground = 0xffcdd6f4,
	black = 0xff181825,
	yellow = 0xfff9e2af,
}
