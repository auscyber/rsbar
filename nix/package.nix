{
  lib,
  stdenv,
  craneLib,
  src,
  installShellFiles,
  pkg-config,
}:

let
  common = {
    src = craneLib.cleanCargoSource src;
    strictDeps = true;

    nativeBuildInputs = [
      pkg-config
      installShellFiles
    ];

    # `rsbar-lua` compiles Lua itself (mlua's `vendored` feature), so the C
    # toolchain has to actually be there rather than assumed.
    buildInputs = [ ];

    # The daemon links `SkyLight`, which Apple ships only under
    # `PrivateFrameworks` -- no SDK puts that on the default search path.
    NIX_LDFLAGS = "-F/System/Library/PrivateFrameworks";

    meta = {
      description = "A macOS menu bar daemon, SketchyBar-compatible";
      homepage = "https://github.com/auscyber/rsbar";
      license = lib.licenses.mit;
      platforms = lib.platforms.darwin;
      mainProgram = "rsbard";
    };
  };

  # Dependencies built once and reused, so editing this workspace does not
  # rebuild the tree below it.
  cargoArtifacts = craneLib.buildDepsOnly (common // { pname = "rsbar-deps"; });
in
craneLib.buildPackage (
  common
  // {
    pname = "rsbar";
    inherit (craneLib.crateNameFromCargoToml { cargoToml = src + "/crates/rsbar/Cargo.toml"; })
      version
      ;
    inherit cargoArtifacts;

    # `rsbar-lua`'s binary is behind `vendored`, which is what supplies the Lua
    # it embeds; without the feature the target does not build at all.
    cargoExtraArgs = "--workspace --features rsbar-lua/vendored";

    # The suite drives the window server and the Accessibility API, neither of
    # which exists in a sandbox. `nix flake check` runs the library tests
    # through `passthru.tests` instead.
    doCheck = false;

    passthru = {
      clippy = craneLib.cargoClippy (
        common
        // {
          pname = "rsbar-clippy";
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
        }
      );

      tests = craneLib.cargoTest (
        common
        // {
          pname = "rsbar-tests";
          inherit cargoArtifacts;
          cargoTestExtraArgs = "--workspace --lib";
        }
      );
    };
  }
)
