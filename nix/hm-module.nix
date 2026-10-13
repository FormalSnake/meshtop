self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.meshtop;
  toml = pkgs.formats.toml { };
  clean = lib.filterAttrs (_: v: v != null && v != [ ]);

  host = lib.types.submodule {
    options = {
      name = lib.mkOption {
        type = lib.types.str;
        example = "g815";
        description = "Name shown in the TUI, and the ssh destination unless `ssh` is set.";
      };
      ssh = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "kyan@100.114.32.78";
        description = "ssh destination. Key auth has to work non-interactively (`BatchMode=yes`).";
      };
      command = lib.mkOption {
        type = lib.types.str;
        default = "${config.home.profileDirectory}/bin/meshtop";
        defaultText = lib.literalExpression ''"''${config.home.profileDirectory}/bin/meshtop"'';
        description = ''
          meshtop on the remote host. An absolute path, because a
          non-interactive ssh shell often has a minimal PATH. The default fits
          a remote that gets meshtop from this same module.
        '';
      };
      role = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "builder";
        description = "Free text shown next to the name.";
      };
      hostname = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "MacBook-Pro-2";
        description = ''
          The machine's own hostname when it differs from `name`. meshtop
          samples the machine it runs on in process, and finds it by comparing
          this (or `name`) with the local hostname.
        '';
      };
    };
  };

  rule = lib.types.submodule {
    options = {
      label = lib.mkOption {
        type = lib.types.str;
        description = "Label shown with the count. Rules sharing a label add up.";
      };
      name = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Exact process names.";
      };
      cmd = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Substring of the full command line.";
      };
      user = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Prefix of the owning user. With it set, distinct users are counted instead of processes.";
      };
    };
  };

  account = lib.types.submodule {
    options = {
      kind = lib.mkOption {
        type = lib.types.enum [
          "claude"
          "codex"
        ];
        description = "Claude Code or Codex.";
      };
      name = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Shown in the TUI. Defaults to the dir name, or the kind.";
      };
      dir = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "~/.claude-work";
        description = "The account's `CLAUDE_CONFIG_DIR` or `CODEX_HOME`. Defaults to `~/.claude` or `~/.codex`.";
      };
    };
  };
in
{
  options.programs.meshtop = {
    enable = lib.mkEnableOption "meshtop, a live view of resource usage across your machines";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "meshtop.packages.\${system}.default";
      description = "The meshtop package to install.";
    };

    interval = lib.mkOption {
      type = lib.types.number;
      default = 1;
      description = "Seconds between samples.";
    };

    history = lib.mkOption {
      type = lib.types.ints.positive;
      default = 120;
      description = "Samples kept for the history graphs.";
    };

    hosts = lib.mkOption {
      type = lib.types.listOf host;
      default = [ ];
      example = lib.literalExpression ''
        [
          { name = "macbook"; role = "dev"; hostname = "MacBook-Pro-2"; }
          { name = "g815"; role = "builder"; }
          { name = "e1504g"; role = "client"; }
        ]
      '';
      description = ''
        Machines to show, in order. Each one needs meshtop installed and ssh
        reachable from wherever the TUI runs. Empty shows only this machine.
      '';
    };

    activity = lib.mkOption {
      type = lib.types.nullOr (lib.types.listOf rule);
      default = null;
      example = lib.literalExpression ''
        [
          { label = "claude"; name = [ "claude" ]; }
          { label = "nix build"; user = "nixbld"; }
          { label = "xcodebuild"; name = [ "xcodebuild" ]; }
        ]
      '';
      description = ''
        Process counts shown per host. `null` keeps the built-in rules
        (claude, codex, nix builds, builds served, rustc, mosh). The agent on
        each host reads its own config, so set this the same everywhere.
      '';
    };

    ports = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        "1234" = "bluebubbles";
        "8384" = "syncthing ui";
      };
      description = "Labels for server ports in the map, on top of the built-in ones.";
    };

    usage = lib.mkOption {
      type = lib.types.listOf account;
      default = [ ];
      example = lib.literalExpression ''
        [
          { kind = "claude"; name = "personal"; }
          { kind = "claude"; name = "work"; dir = "~/.claude-work"; }
          { kind = "codex"; }
        ]
      '';
      description = ''
        Claude Code and Codex accounts whose plan usage the agents view
        shows, read on the machine the TUI runs on. Empty watches `~/.claude`
        and `~/.codex`. An account only shows while it is signed in.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."meshtop/config.toml".source = toml.generate "meshtop.toml" (
      clean {
        inherit (cfg) interval history ports;
        hosts = map clean cfg.hosts;
        usage = map clean cfg.usage;
        activity = if cfg.activity == null then null else map clean cfg.activity;
      }
    );
  };
}
