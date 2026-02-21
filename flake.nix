{
  description = "Expose local web servers on the internet";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages = {
          tinytun = pkgs.callPackage ./package.nix { };
          default = self.packages.${system}.tinytun;
        };
      }
    );
}
