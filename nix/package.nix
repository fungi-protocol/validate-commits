{
  perSystem =
    {
      craneLib,
      pkgs,
      self',
      ...
    }:
    let
      commonArgs = {
        src = craneLib.cleanCargoSource ../.;
        strictDeps = true;
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.zlib ];
        nativeCheckInputs = [ pkgs.git ];
      };
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      validate-commits = craneLib.buildPackage (
        commonArgs
        // {
          inherit cargoArtifacts;
          nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.makeWrapper ];
          postInstall = ''
            wrapProgram "$out/bin/validate-commits" \
              --prefix PATH : ${
                pkgs.lib.makeBinPath [
                  pkgs.nix
                  pkgs.git
                ]
              }
          '';
        }
      );
    in
    {
      checks.default = validate-commits;

      packages = {
        default = validate-commits;
        inherit validate-commits;
      };

      apps = {
        default = self'.apps.validate-commits;
        validate-commits = {
          type = "app";
          program = "${validate-commits}/bin/validate-commits";
          meta.description = "Validate a Nix flake across a range of Git commits";
        };
      };
    };
}
