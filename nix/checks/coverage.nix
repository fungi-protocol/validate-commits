{
  perSystem =
    {
      pkgs,
      craneLib,
      commonArgs,
      cargoArtifacts,
      ...
    }:
    let
      collection = craneLib.mkCargoDerivation (
        commonArgs
        // {
          inherit cargoArtifacts;
          pnameSuffix = "-coverage-collect";
          nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ [
            pkgs.cargo-llvm-cov
            pkgs.cargo-nextest
          ];
          buildPhaseCargoCommand = ''
            cargo llvm-cov nextest --workspace --no-report --remap-path-prefix
            mkdir -p "$out"
            cargo llvm-cov report --lcov --output-path "$out/coverage.lcov"
            test -s "$out/coverage.lcov"
          '';
          installPhase = "true";
        }
      );
    in
    {
      checks.coverage = pkgs.runCommand "coverage" { nativeBuildInputs = [ pkgs.lcov ]; } ''
        mkdir -p "$out"
        cp ${collection}/coverage.lcov "$out/coverage.lcov"
        lcov --summary "$out/coverage.lcov" --fail-under-lines 100
      '';
    };
}
