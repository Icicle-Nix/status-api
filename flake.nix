{
  description = "status-api: on-demand machine status as JSON over HTTP";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = forAllSystems (system: {
        default = nixpkgs.legacyPackages.${system}.callPackage ./package.nix { };
      });

      # 소비자의 pkgs로 빌드한다 — services.status-api.package 기본값.
      nixosModules.default = ./nix/module.nix;

      checks.x86_64-linux =
        let
          pkgs = nixpkgs.legacyPackages.x86_64-linux;
        in
        {
          package = self.packages.x86_64-linux.default;
          vm = pkgs.testers.runNixOSTest (import ./nix/test.nix { inherit self; });
        };
    };
}
