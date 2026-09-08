# A home-manager module for rsbar, shaped after home-manager's own
# `programs.sketchybar` so a config can move between the two with the option
# names it already knows.
#
# The one real difference is the Lua story. `sketchybar` needs SbarLua, a
# separate C module loaded into a system Lua, and the module has to thread
# `LUA_PATH`/`LUA_CPATH` through a wrapper to make that work. rsbar ships its
# own interpreter, `rsbar-lua`, with the API already in it -- so a Lua config
# here is a script with `rsbar-lua` on its shebang and nothing to wire up.
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    literalExpression
    mkEnableOption
    mkOption
    types
    ;

  cfg = config.programs.rsbar;
in
{
  options.programs.rsbar = {
    enable = mkEnableOption "rsbar";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.rsbar;
      defaultText = literalExpression "rsbar.packages.\${system}.rsbar";
      description = "The rsbar package to use.";
    };

    finalPackage = mkOption {
      type = types.package;
      readOnly = true;
      internal = true;
      description = "Resulting customised rsbar package.";
    };

    configType = mkOption {
      type = types.enum [
        "bash"
        "lua"
      ];
      default = "lua";
      description = ''
        Which interpreter the generated `rsbarrc` is given.

        `lua` puts `rsbar-lua` on the shebang, which is this project's own
        interpreter with the `rsbar` module built in -- no `LUA_PATH` to set and
        nothing to install alongside. `bash` writes a shell script that drives
        the bar through the `rsbard` CLI, the way `sketchybar`'s own configs do.
      '';
    };

    config = mkOption {
      type = types.nullOr (lib.hm.types.sourceFileOrLines ".config/rsbar" "rsbarrc");
      default = null;
      example = literalExpression ''
        # A directory, which is what any real config is:
        {
          source = ./rsbar;
          recursive = true;
        }
      '';
      description = ''
        The rsbar configuration: a string of Lua (or shell, per
        {option}`programs.rsbar.configType`), or an attribute set with `source`
        pointing at a directory and `recursive = true`.

        A directory must contain `rsbarrc`, which is the entry point rsbar runs.
      '';
    };

    extraPackages = mkOption {
      type = with types; listOf package;
      default = [ ];
      example = literalExpression "[ pkgs.jq ]";
      description = "Extra packages to put on `PATH` for rsbar and the scripts it runs.";
    };

    includeSystemPath = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Whether to append the usual system directories to the wrapper's `PATH`,
        so a config's scripts can reach `/usr/bin/osascript` and friends.
      '';
    };

    service = {
      enable = mkEnableOption "the rsbar launchd agent" // {
        default = true;
      };

      errorLogFile = mkOption {
        type = with types; nullOr (either path str);
        default = "${config.home.homeDirectory}/Library/Logs/rsbar/rsbar.err.log";
        defaultText = literalExpression "\${config.home.homeDirectory}/Library/Logs/rsbar/rsbar.err.log";
        description = "Absolute path to log all stderr output to.";
      };

      outLogFile = mkOption {
        type = with types; nullOr (either path str);
        default = "${config.home.homeDirectory}/Library/Logs/rsbar/rsbar.out.log";
        defaultText = literalExpression "\${config.home.homeDirectory}/Library/Logs/rsbar/rsbar.out.log";
        description = "Absolute path to log all stdout output to.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      (lib.hm.assertions.assertPlatform "programs.rsbar" pkgs lib.platforms.darwin)
    ];

    programs.rsbar.finalPackage =
      let
        # `rsbard` puts its own directory on the front of `PATH` for everything
        # it spawns, so a config calling `rsbard` resolves without help. This
        # wrapper is for what the config wants *besides* that.
        pathPackages = [ cfg.package ] ++ cfg.extraPackages;

        wrapperArgs = lib.flatten [
          [
            "--prefix"
            "PATH"
            ":"
            (lib.makeBinPath pathPackages)
          ]
          (lib.optional cfg.includeSystemPath [
            "--suffix"
            "PATH"
            ":"
            "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin"
          ])
        ];
      in
      pkgs.symlinkJoin {
        name = "rsbar-${cfg.package.version or "0"}";
        paths = [ cfg.package ];
        nativeBuildInputs = [ pkgs.makeWrapper ];
        postBuild = ''
          wrapProgram $out/bin/rsbard ${lib.escapeShellArgs wrapperArgs}
        '';
        inherit (cfg.package) meta;
      };

    home.packages = [ cfg.finalPackage ];

    launchd.agents.rsbar = {
      inherit (cfg.service) enable;
      config = {
        Program = lib.getExe cfg.finalPackage;
        # The bar draws, so it wants a foreground scheduling band rather than a
        # background one.
        ProcessType = "Interactive";
        KeepAlive = true;
        RunAtLoad = true;
        StandardErrorPath = cfg.service.errorLogFile;
        StandardOutPath = cfg.service.outLogFile;
      };
    };

    xdg.configFile = lib.mkIf (cfg.config != null) (
      if cfg.config.source != null && cfg.config.recursive then
        { "rsbar" = { inherit (cfg.config) source recursive; }; }
      else if cfg.config.source != null then
        { "rsbar/rsbarrc".source = cfg.config.source; }
      else
        {
          "rsbar/rsbarrc".source = pkgs.writeTextFile {
            name = "rsbarrc";
            executable = true;
            text =
              if cfg.configType == "lua" then
                ''
                  #!${lib.getBin cfg.finalPackage}/bin/rsbar-lua
                  -- Generated by home-manager
                  ${cfg.config.text}
                ''
              else
                ''
                  #!${pkgs.runtimeShell}
                  # Generated by home-manager
                  ${cfg.config.text}
                '';
          };
        }
    );
  };
}
