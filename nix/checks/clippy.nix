{
  perSystem =
    {
      craneLib,
      commonArgs,
      cargoArtifacts,
      ...
    }:
    {
      checks.clippy = craneLib.cargoClippy (
        commonArgs
        // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings -D unused";
        }
      );
    };
}
