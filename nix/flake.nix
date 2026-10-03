{
  description = "silver: a personal coding agent (HTTP API + embedded web UI)";

  # Rolling branch. The generated flake.lock pins the exact revision, so commit
  # it after the first `nix build` / `nix develop`.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  # This flake lives under nix/, so the repository root (which holds Cargo.toml
  # and Cargo.lock) comes in as a path input relative to this file. In a git
  # checkout "git+file:./.." is a faster alternative because it skips ignored
  # build outputs.
  inputs.silver-src = {
    url = "path:./..";
    flake = false;
  };

  outputs = { self, nixpkgs, silver-src }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem = system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          packages = import ./silver.nix {
            inherit (pkgs) lib rustPlatform buildNpmPackage importNpmLock nodejs pkg-config;
            src = silver-src;
          };
        in
        {
          packages = packages // {
            default = packages.silver;
          };
          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              nodejs
              pkg-config
              rustc
              rustfmt
            ];
          };
        };
    in
    {
      packages = nixpkgs.lib.genAttrs systems (system: (perSystem system).packages);
      devShells = nixpkgs.lib.genAttrs systems (system: (perSystem system).devShells);
    };
}
