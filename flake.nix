{
  description = "rsbar — a macOS menu bar daemon, and a Lua host for its configs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";

    devenv.url = "github:cachix/devenv";

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
          rsbar = import ./nix/hm-module.nix inputs.self;
          default = rsbar;
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
            (import inputs.rust-overlay { inherit pkgs; }).rust-bin.stable.latest.default;

          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

          rsbar = pkgs.callPackage ./nix/package.nix {
            inherit craneLib;
            src = inputs.self;
          };
        in
        {
          packages = {
            inherit rsbar;
            default = rsbar;
          };

          checks = {
            inherit (rsbar.passthru) clippy tests;
          };

          devenv.shells.default = {
            imports = [ ./devenv.nix ];
            # `devenv.nix` reaches for `inputs.rust-overlay`; under flake-parts
            # the flake's own inputs are what it gets.
            _module.args.inputs = inputs;
          };
        };
    };
}
