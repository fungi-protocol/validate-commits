{
  perSystem =
    {
      pkgs,
      craneLib,
      commonArgs,
      cargoArtifacts,
      ...
    }:
    {
      checks.mutants = craneLib.mkCargoDerivation (
        commonArgs
        // {
          inherit cargoArtifacts;
          pnameSuffix = "-mutants";
          nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ [
            pkgs.cargo-mutants
            pkgs.cargo-nextest
          ];
          buildPhaseCargoCommand = "cargo mutants --workspace --in-place --test-tool nextest";
          installPhase = "mkdir -p $out";
        }
      );
    };
}
