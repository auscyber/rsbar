{
  lib,
  craneLib,
  src,
  pkg-config,
  luajit,
}:

let
  common = {
    src = craneLib.cleanCargoSource src;
    strictDeps = true;

    nativeBuildInputs = [ pkg-config ];

    # Plain `--workspace`: linking LuaJIT rather than vendoring it is the
    # default now, so there is nothing to override here.
    cargoExtraArgs = "--workspace";

    # LuaJIT from outside rather than mlua's `vendored`, which cannot work
    # here: mlua copies LuaJIT's source into `OUT_DIR` preserving the read-only
    # permissions the nix store gives it, and LuaJIT's own makefile then tries
    # to write into that tree -- "Cannot copy 'luajit_relver.txt': Permission
    # denied". pkg-config finds this one, and it is the same interpreter a
    # vendored build would have produced.
    buildInputs = [ luajit ];

    # The daemon links `SkyLight`, which Apple ships only under
    # `PrivateFrameworks` -- no SDK puts that on the default search path.
    NIX_LDFLAGS = "-F/System/Library/PrivateFrameworks";

    meta = {
      description = "A macOS menu bar daemon, SketchyBar-compatible";
      homepage = "https://github.com/auscyber/coolabah";
      license = lib.licenses.mit;
      platforms = lib.platforms.darwin;
      mainProgram = "coolabah";
    };
  };

  # Dependencies built once and reused, so editing this workspace does not
  # rebuild the tree below it.
  cargoArtifacts = craneLib.buildDepsOnly (common // { pname = "coolabah-deps"; });
in
craneLib.buildPackage (
  common
  // {
    pname = "coolabah";
    # From the workspace, not from `crates/coolabah/Cargo.toml`: every crate here
    # takes `version.workspace = true`, so the member manifest has no version
    # of its own for `crateNameFromCargoToml` to find.
    version = (lib.importTOML (src + "/Cargo.toml")).workspace.package.version;
    inherit cargoArtifacts;


    # The suite drives the window server and the Accessibility API, neither of
    # which exists in a sandbox. `nix flake check` runs the library tests
    # through `passthru.tests` instead.
    doCheck = false;

    passthru = {
      clippy = craneLib.cargoClippy (
        common
        // {
          pname = "coolabah-clippy";
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
        }
      );

      tests = craneLib.cargoTest (
        common
        // {
          pname = "coolabah-tests";
          inherit cargoArtifacts;
          cargoTestExtraArgs = "--workspace --lib";
        }
      );
    };
  }
)
