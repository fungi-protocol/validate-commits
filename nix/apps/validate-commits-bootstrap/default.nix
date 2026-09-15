{
  perSystem =
    { pkgs, ... }:
    let
      validate-commits-bootstrap = pkgs.writeShellApplication {
        name = "validate-commits-bootstrap";
        runtimeInputs = [
          pkgs.git
          pkgs.nix
        ];
        text = builtins.readFile ./validate-commits.sh;
      };
    in
    {
      packages.validate-commits-bootstrap = validate-commits-bootstrap;
      apps.validate-commits-bootstrap = {
        type = "app";
        program = "${validate-commits-bootstrap}/bin/validate-commits-bootstrap";
        meta.description = "Validate commits with the bootstrap shell script";
      };
    };
}
