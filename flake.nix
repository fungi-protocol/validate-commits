{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      flake.flakeModules.checks = import ./nix/checks;
      imports = [
        ./nix/apps/validate-commits-bootstrap
        ./nix/package.nix
        ./nix/toolchain.nix
        ./nix/treefmt.nix
        ./nix/checks/cargo-shear.nix
        ./nix/checks/cargo-sort.nix
        ./nix/tests/negative-tests.nix
      ];
    };
}
