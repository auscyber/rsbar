# A home-manager module for coolabah, shaped after home-manager's own
# `programs.sketchybar` so a config can move between the two with the option
# names it already knows.
#
# The one real difference is the Lua story. `sketchybar` needs SbarLua, a
# separate C module loaded into a system Lua, and the module has to thread
# `LUA_PATH`/`LUA_CPATH` through a wrapper to make that work. coolabah ships its
# own interpreter, `coolabah-lua`, with the API already in it -- so a Lua config
# here is a script with `coolabah-lua` on its shebang and nothing to wire up.
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

  cfg = config.programs.coolabah;
in
{
  options.programs.coolabah = {
    enable = mkEnableOption "coolabah";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.coolabah;
      defaultText = literalExpression "coolabah.packages.\${system}.coolabah";
      description = "The coolabah package to use.";
    };

    finalPackage = mkOption {
      type = types.package;
      readOnly = true;
      internal = true;
      description = "Resulting customised coolabah package.";
    };

    configType = mkOption {
      type = types.enum [
        "bash"
        "lua"
      ];
      default = "lua";
      description = ''
        Which interpreter the generated `coolabahrc` is given.

        `lua` puts `coolabah-lua` on the shebang, which is this project's own
        interpreter with the `coolabah` module built in -- no `LUA_PATH` to set and
        nothing to install alongside. `bash` writes a shell script that drives
        the bar through the `coolabah` CLI, the way `sketchybar`'s own configs do.
      '';
    };

    config = mkOption {
      type = types.nullOr (lib.hm.types.sourceFileOrLines ".config/coolabah" "coolabahrc");
      default = null;
      example = literalExpression ''
        # A directory, which is what any real config is:
        {
          source = ./coolabah;
          recursive = true;
        }
      '';
      description = ''
        The coolabah configuration: a string of Lua (or shell, per
        {option}`programs.coolabah.configType`), or an attribute set with `source`
        pointing at a directory and `recursive = true`.

        A directory must contain `coolabahrc`, which is the entry point coolabah runs.
      '';
    };

    extraPackages = mkOption {
      type = with types; listOf package;
      default = [ ];
      example = literalExpression "[ pkgs.jq ]";
      description = "Extra packages to put on `PATH` for coolabah and the scripts it runs.";
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
      enable = mkEnableOption "the coolabah launchd agent" // {
        default = true;
      };

      errorLogFile = mkOption {
        type = with types; nullOr (either path str);
        default = "${config.home.homeDirectory}/Library/Logs/coolabah/coolabah.err.log";
        defaultText = literalExpression "\${config.home.homeDirectory}/Library/Logs/coolabah/coolabah.err.log";
        description = "Absolute path to log all stderr output to.";
      };

      outLogFile = mkOption {
        type = with types; nullOr (either path str);
        default = "${config.home.homeDirectory}/Library/Logs/coolabah/coolabah.out.log";
        defaultText = literalExpression "\${config.home.homeDirectory}/Library/Logs/coolabah/coolabah.out.log";
        description = "Absolute path to log all stdout output to.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      (lib.hm.assertions.assertPlatform "programs.coolabah" pkgs lib.platforms.darwin)
    ];

    programs.coolabah.finalPackage =
      let
        # `coolabah` puts its own directory on the front of `PATH` for everything
        # it spawns, so a config calling `coolabah` resolves without help. This
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
        name = "coolabah-${cfg.package.version or "0"}";
        paths = [ cfg.package ];
        nativeBuildInputs = [ pkgs.makeWrapper ];
        postBuild = ''
          wrapProgram $out/bin/coolabah ${lib.escapeShellArgs wrapperArgs}
        '';
        inherit (cfg.package) meta;
      };

    home.packages = [ cfg.finalPackage ];

    launchd.agents.coolabah = {
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
        { "coolabah" = { inherit (cfg.config) source recursive; }; }
      else if cfg.config.source != null then
        { "coolabah/coolabahrc".source = cfg.config.source; }
      else
        {
          "coolabah/coolabahrc".source = pkgs.writeTextFile {
            name = "coolabahrc";
            executable = true;
            text =
              if cfg.configType == "lua" then
                ''
                  #!${lib.getBin cfg.finalPackage}/bin/coolabah-lua
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
