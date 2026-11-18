{ lib, rustPlatform }:
rustPlatform.buildRustPackage {
  pname = "status-api";
  version = (lib.importTOML ./Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "On-demand machine status (power, load, memory, disks, network, sshd) as JSON";
    homepage = "https://github.com/Icicle-Nix/status-api";
    platforms = lib.platforms.linux;
    mainProgram = "status-api";
  };
}
