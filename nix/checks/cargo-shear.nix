{
  perSystem =
    {
      pkgs,
      craneLib,
      commonArgs,
      ...
    }:
    {
      checks.cargo-shear =
        pkgs.runCommand "cargo-shear"
          {
            inherit (commonArgs) src;
            cargoVendorDir = craneLib.vendorCargoDeps { inherit (commonArgs) src; };
            nativeBuildInputs = [
              pkgs.cargo
              pkgs.cargo-shear
            ];
          }
          ''
            export CARGO_HOME="$TMPDIR/cargo-home"
            export CARGO_NET_OFFLINE=true
            mkdir -p "$CARGO_HOME"
            cp "$cargoVendorDir/config.toml" "$CARGO_HOME/config.toml"
            cd "$src"
            cargo-shear
            mkdir -p "$out"
          '';
    };
}
