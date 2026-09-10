{
  description = "coolabah — a macOS menu bar daemon, and a Lua host for its configs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";

    devenv.url = "github:cachix/devenv";

    # How devenv learns the working directory under a flake, where `./.` is a
    # store path and tells it nothing. `.envrc` overrides this input with a
    # file holding the real path; the default is empty, and an empty value
    # means "not in a devenv shell", which is exactly right for `nix build`.
    # See https://devenv.sh/guides/using-with-flakes/
    devenv-root = {
      url = "file+file:///dev/null";
      flake = false;
    };

    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.devenv.flakeModule ];

      # A window server daemon. There is no other platform for it to run on.
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
      ];

      flake = {
        homeManagerModules = rec {
          coolabah = import ./nix/hm-module.nix inputs.self;
          default = coolabah;
        };
      };

      perSystem =
        {
          pkgs,
          system,
          lib,
          ...
        }:
        let
          rustToolchain =
            (pkgs.extend inputs.rust-overlay.overlays.default).rust-bin.stable.latest.default;


          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

          coolabah = pkgs.callPackage ./nix/package.nix {
            inherit craneLib;
            src = inputs.self;
          };
        in
        {
          packages = {
            inherit coolabah;
            default = coolabah;
          };

          checks = {
            inherit (coolabah.passthru) clippy tests;
          };

          devenv.shells.default = {
            imports = [ ./devenv.nix ];
            devenv.root =
              let
                root = builtins.readFile inputs.devenv-root.outPath;
              in
              lib.mkIf (root != "") root;
          };
        };
    };
}
