{
  perSystem =
    { config, pkgs, ... }:
    let
      join =
        name: names:
        pkgs.linkFarm name (
          map (check: {
            name = check;
            path = config.checks.${check};
          }) names
        );
    in
    {
      checks = {
        quick = join "quick-checks" [
          "tests"
          "doctests"
          "clippy"
        ];
        lint = join "lint-checks" [
          "cargo-shear"
          "cargo-sort"
          "clippy"
          "doc"
        ];
        nightly = join "nightly-checks" [
          "build"
          "tests"
          "doctests"
          "lint"
          "coverage"
        ];
      };
    };
}
