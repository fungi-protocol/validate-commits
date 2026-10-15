{
  perSystem =
    {
      craneLib,
      commonArgs,
      cargoArtifacts,
      ...
    }:
    {
      checks.build = craneLib.buildPackage (
        commonArgs
        // {
          inherit cargoArtifacts;
          # The tests check runs the test suite separately.
          doCheck = false;
        }
      );
    };
}
