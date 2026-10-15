{
  perSystem =
    {
      craneLib,
      commonArgs,
      cargoArtifacts,
      ...
    }:
    {
      checks.doc = craneLib.cargoDoc (
        commonArgs
        // {
          inherit cargoArtifacts;
          cargoDocExtraArgs = "--no-deps --all-features";
          RUSTDOCFLAGS = "-D warnings";
        }
      );
    };
}
