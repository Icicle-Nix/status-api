{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.status-api;
  isPath = lib.hasPrefix "/" cfg.socket;
in
{
  options.services.status-api = {
    enable = lib.mkEnableOption "status-api, an on-demand machine status JSON endpoint";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "status-api package to run.";
    };

    socket = lib.mkOption {
      type = lib.types.str;
      default = "/run/status-api.sock";
      example = "127.0.0.1:8435";
      description = ''
        systemd `ListenStream=` of the activation socket. A path is a Unix
        socket (mode 0660, group `socketGroup`); anything else is TCP and
        reachable by whoever can reach that address.
      '';
    };

    socketGroup = lib.mkOption {
      type = lib.types.str;
      default = "root";
      example = "nginx";
      description = "Group owning the Unix socket — typically the reverse proxy's.";
    };

    disks = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "/" ];
      example = [
        "/"
        "/home"
        "/nix/store"
      ];
      description = "Paths whose filesystems are reported under `disks`.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.sockets.status-api = {
      description = "status-api socket";
      wantedBy = [ "sockets.target" ];
      listenStreams = [ cfg.socket ];
      socketConfig = {
        SocketMode = "0660";
        SocketGroup = cfg.socketGroup;
      };
    };

    systemd.services.status-api = {
      description = "status-api: on-demand machine status JSON";
      requires = [ "status-api.socket" ];
      after = [ "status-api.socket" ];
      # systemctl / journalctl
      path = [ config.systemd.package ];
      serviceConfig = {
        ExecStart = lib.escapeShellArgs (
          [ (lib.getExe cfg.package) ] ++ lib.concatMap (d: [ "--disk" d ]) cfg.disks
        );
        Restart = "on-failure";

        DynamicUser = true;
        # sshd journal (journalctl -u sshd).
        SupplementaryGroups = [ "systemd-journal" ];
        # Only for RAPL energy_uj, which is root-only (0400) since the
        # PLATYPUS side channel. See README for the trade-off.
        AmbientCapabilities = [ "CAP_DAC_READ_SEARCH" ];
        CapabilityBoundingSet = [ "CAP_DAC_READ_SEARCH" ];
        NoNewPrivileges = true;

        ProtectSystem = "strict";
        # Not "yes": statvfs on /home must see the real filesystem, not an
        # empty tmpfs.
        ProtectHome = "read-only";
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        # No PrivateNetwork: it would hide the host's interfaces in
        # /sys/class/net. The listening socket is inherited, so AF_UNIX
        # (systemctl's D-Bus / journal) is all the service ever creates.
        RestrictAddressFamilies = [ "AF_UNIX" ];
        IPAddressDeny = lib.mkIf isPath "any";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RemoveIPC = true;
        UMask = "0077";
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" ];
      };
    };
  };
}
