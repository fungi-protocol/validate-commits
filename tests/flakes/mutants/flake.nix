{
  # validate-commits overrides this with the commit's diff. Without an
  # override it is a directory, which a real flake would treat as "run the
  # full mutation suite".
  inputs.mutants-diff = {
    url = "path:./mutants-diff";
    flake = false;
  };

  outputs =
    { self, mutants-diff }:
    {
      checks = builtins.listToAttrs (
        map
          (system: {
            name = system;
            value.mutants = builtins.derivation {
              name = "fixture-mutants";
              inherit system;
              builder = builtins.readFile ./builder;
              args = [
                "-c"
                (builtins.readFile ./mutants.sh)
              ];
              src = self;
              diff = mutants-diff;
              tools = builtins.readFile ./tools;
            };
          })
          [
            "aarch64-darwin"
            "aarch64-linux"
            "x86_64-darwin"
            "x86_64-linux"
          ]
      );
    };
}
