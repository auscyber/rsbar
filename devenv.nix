{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

let
  # miri only ships with nightly, and this project's toolchain comes from
  # nixpkgs -- there is no rustup here, so `cargo +nightly` means nothing.
  # A second toolchain, used by the `miri` script alone, keeps the build's
  # own rustc/cargo exactly where they were.
  rust = (pkgs.extend (import inputs.rust-overlay)).rust-bin;
  nightly = rust.selectLatestNightlyWith (
    toolchain: toolchain.default.override { extensions = [ "miri" "rust-src" ]; }
  );
in
{
  # https://devenv.sh/basics/

  # https://devenv.sh/packages/
  #
  # `cargo-flamegraph` installs a binary literally called `flamegraph`, which is
  # what `~/.claude/skills/flamegraph/scripts/flamegraph.sh` invokes -- so it
  # belongs in the shell rather than on an ad-hoc PATH.
  packages = [
    pkgs.git
    pkgs.cargo-flamegraph
  ];

  # https://devenv.sh/languages/
  # languages.rust.enable = true;

  # https://devenv.sh/processes/
  # processes.dev.exec = "${lib.getExe pkgs.watchexec} -n -- ls -la";

  # https://devenv.sh/services/
  # services.postgres.enable = true;

  # https://devenv.sh/scripts/
  languages.rust.enable = true;
  claude.code.enable = true;

  # `miri test -p rsbar-protocol` and friends. Its own target directory, so a
  # miri run never invalidates the ordinary build cache.
  scripts.miri.exec = ''
    export PATH=${nightly}/bin:$PATH
    export CARGO_TARGET_DIR=''${CARGO_TARGET_DIR:-target}/miri
    exec ${nightly}/bin/cargo miri "$@"
  '';

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
