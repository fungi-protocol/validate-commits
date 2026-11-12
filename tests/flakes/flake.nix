{
  outputs =
    { self }:
    {
      checks = builtins.listToAttrs (
        map
          (system: {
            name = system;
            value.validation = builtins.derivation {
              name = "fixture-check";
              inherit system;
              builder = builtins.readFile ./builder;
              args = [
                "-c"
                (builtins.readFile ./script)
              ];
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
