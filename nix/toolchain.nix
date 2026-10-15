{ inputs, ... }:
{
  perSystem =
    { system, ... }:
    let
      pkgs = import inputs.nixpkgs {
        inherit system;
        overlays = [ (import inputs.rust-overlay) ];
      };
      rustToolchain = pkgs.rust-bin.stable.latest.default;
    in
    {
      _module.args = {
        inherit pkgs rustToolchain;
        craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;
      };
    };
}
