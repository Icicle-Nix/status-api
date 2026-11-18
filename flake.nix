{
  description = "status-api: on-demand machine status as JSON over HTTP";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
  };

  outputs =
    { flake-parts, ... }@inputs:
    flake-parts.lib.mkFlake { inherit inputs; } (
      { self, ... }:
      {
        systems = [ "x86_64-linux" ];

        perSystem =
          { pkgs, system, ... }:
          {
            packages.default = pkgs.callPackage ./package.nix { };

            checks = {
              package = self.packages.${system}.default;
              vm = pkgs.testers.runNixOSTest (import ./nix/test.nix { inherit self; });
            };
          };

        # 소비자의 pkgs로 빌드한다 — services.status-api.package 기본값.
        flake.nixosModules.default = ./nix/module.nix;
      }
    );
}
