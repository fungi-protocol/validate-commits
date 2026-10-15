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
      checks.tests = craneLib.cargoNextest (
        commonArgs
        // {
          inherit cargoArtifacts;
          nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ [ pkgs.unzip ];
          cargoNextestExtraArgs = "--user-config-file ${./nextest-record.toml}";
          preCheck = ''
            export NEXTEST_STATE_DIR="$TMPDIR/nextest-state"
            mkdir -p "$NEXTEST_STATE_DIR"
          '';
          postCheck = ''
            cargo nextest store export --no-pager \
              --user-config-file ${./nextest-record.toml} \
              --archive-file "$out/nextest-run.zip" latest
            unzip -tqq "$out/nextest-run.zip"
          '';
        }
      );
      # nextest does not run doctests.
      checks.doctests = craneLib.cargoDocTest (commonArgs // { inherit cargoArtifacts; });
    };
}
