{
  perSystem =
    {
      config,
      pkgs,
      craneLib,
      commonArgs,
      ...
    }:
    let
      cargoShearSource = pkgs.applyPatches {
        name = "cargo-shear-negative-source";
        inherit (commonArgs) src;
        patches = [ ./fixtures/cargo-shear.patch ];
      };
      cargoSortSource = pkgs.applyPatches {
        name = "cargo-sort-negative-source";
        inherit (commonArgs) src;
        patches = [ ./fixtures/cargo-sort.patch ];
      };
    in
    {
      checks.cargo-shear-negative = pkgs.testers.testBuildFailure' {
        drv = config.checks.cargo-shear.overrideAttrs (_: {
          src = cargoShearSource;
          cargoVendorDir = craneLib.vendorCargoDeps { src = cargoShearSource; };
        });
        expectedBuilderExitCode = 1;
        expectedBuilderLogEntries = [ "unused dependency `libc`" ];
      };
      checks.cargo-sort-negative = pkgs.testers.testBuildFailure' {
        drv = config.checks.cargo-sort.overrideAttrs (_: {
          src = cargoSortSource;
        });
        expectedBuilderExitCode = 1;
        expectedBuilderLogEntries = [ "Some Cargo.toml files are not sorted or formatted" ];
      };
    };
}
