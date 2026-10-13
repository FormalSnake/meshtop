{
  description = "meshtop: live resource usage and connections across your machines";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        meshtop = pkgs.callPackage ./nix/package.nix { };
        default = meshtop;
      });

      overlays.default = final: _: { meshtop = final.callPackage ./nix/package.nix { }; };

      homeModules.default = import ./nix/hm-module.nix self;
      homeManagerModules.default = self.homeModules.default;

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt ];
        };
      });
    };
}
