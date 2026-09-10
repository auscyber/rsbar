# coolabah

A macOS menu bar replacement, compatible with [SketchyBar](https://github.com/FelixKratz/SketchyBar)'s
configuration language and CLI, written in Rust.

It draws its own window-server windows rather than an `NSWindow`, which is what
lets a bar sit above the menu bar, on every space, and over fullscreen apps.
Menu bar items belonging to other applications can be mirrored into it —
including the ones macOS has hidden behind the notch.

> **Status.** Working, and in daily use by its author. The window-server APIs it
> depends on are private and undocumented; the notes in `crates/skylight` record
> what was verified against a real machine and what is inference.

## Install

### home-manager

The flake exports a module shaped after home-manager's own
`programs.sketchybar`, so an existing SketchyBar configuration keeps the option
names it already uses.

```nix
{
  inputs.coolabah.url = "github:auscyber/coolabah";

  # in your home-manager configuration:
  imports = [ inputs.coolabah.homeManagerModules.coolabah ];

  programs.coolabah = {
    enable = true;
    configType = "lua";
    config = {
      source = ./coolabah;   # a directory containing `coolabahrc`
      recursive = true;
    };
    extraPackages = [ pkgs.jq ];
  };
}
```

That installs the package, writes the configuration to
`~/.config/coolabah`, and registers a launchd agent that keeps the daemon running
and logs to `~/Library/Logs/coolabah/`.

The one place it diverges from `programs.sketchybar` is Lua. SketchyBar needs
SbarLua — a C module loaded into a system Lua, with `LUA_PATH` and `LUA_CPATH`
threaded through a wrapper to find it. coolabah ships its own interpreter,
`coolabah-lua`, with the API already built in, so a Lua config is a script with
`coolabah-lua` on its shebang and nothing to wire up.

### Without home-manager

```sh
nix build github:auscyber/coolabah
./result/bin/coolabah
```

## Configuration

coolabah reads `$COOLABAH_CONFIG`, or `~/.config/coolabah/coolabahrc` — an executable script
in whatever language its shebang names. Two are first-class:

**Lua**, through the bundled interpreter:

```lua
#!/usr/bin/env coolabah-lua
local sbar = require("coolabah")

sbar.bar({ height = 32, position = "top", color = 0xff181926 })

local clock = sbar.add("item", "clock", {
  position = "right",
  update_freq = 10,
  script = os.getenv("CONFIG_DIR") .. "/plugins/clock.sh",
})

clock:subscribe("system_woke", function(env)
  clock:set({ label = { string = os.date("%H:%M") } })
end)
```

**Shell**, driving the daemon through its CLI, exactly as SketchyBar's own
configurations do:

```sh
#!/usr/bin/env bash
coolabah --bar height=32 position=top
coolabah --add item clock right \
       --set clock update_freq=10 script="date '+%H:%M'"
coolabah --update
```

`coolabah` puts its own directory on the front of `PATH` for everything it spawns,
so a config and its plugin scripts can call `coolabah` by name without a wrapper
or a second copy on the system path.

### Mirroring other applications' menu bar items

```lua
sbar.add("alias", "Control Centre,Sound", { position = "right" })
```

`coolabah --query default_menu_items` lists what can be mirrored. Items macOS has
hidden — the overflow behind the notch — are included, which is the part no
other tool reaches.

## Development

The dev shell is [devenv](https://devenv.sh), wired in through
[flake-parts](https://flake.parts):

```sh
nix develop          # or `direnv allow`
cargo test --workspace
```

Everything goes through the shell — the toolchain, `cargo-flamegraph`, and a
second nightly toolchain used only by `miri`:

```sh
miri test -p coolabah-protocol
```

Packages are built with [crane](https://crane.dev):

```sh
nix build .#coolabah
nix flake check          # clippy at deny-warnings, plus the library tests
```

`nix flake check` runs the library tests only. The rest of the suite drives the
window server and the Accessibility API, neither of which exists inside a build
sandbox.

### Layout

| crate | what it is |
| --- | --- |
| `coolabah` | the daemon (`coolabah`) and its CLI |
| `coolabah-lua` | the Lua host (`coolabah-lua`) and the `coolabah` module a config requires |
| `coolabah-protocol` | the wire format the CLI, the Lua host and the daemon share |
| `skylight` | safe bindings to the private window-server, Accessibility and CoreText APIs |
| `*-macros` | the derives those two use |

`skylight` is the interesting half: private-framework bindings with the
verification notes kept beside them, an execution model that keeps
main-thread-only work on the main thread without blocking it, and a callback
layer that turns C callbacks into ordinary Rust streams.

## Profiling

The flamegraph helper wants a signed binary so the TCC grant survives a rebuild,
and the Accessibility grant settled before it records:

```sh
flamegraph.sh -C . --bin coolabah --sign --accessibility -d 45 -o after.svg
```

## Licence

MIT.
