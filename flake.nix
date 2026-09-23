{
  description = "iwe-plus: a fork of iwe (github.com/iwe-org/iwe) with transactions, schema validation, checkers and the knowledge-compositor commit hooks";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      workspace = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package;
      upstream = nixpkgs.lib.trim (builtins.readFile ./UPSTREAM_VERSION);
    in
    {
      packages = forAllSystems (pkgs: rec {
        # iwe, iwec and iwes. Tests stay out of the build: many drive the
        # built binaries, spawn servers or race locks, and run in CI instead.
        iwe-plus = pkgs.rustPlatform.buildRustPackage {
          pname = "iwe-plus";
          inherit (workspace) version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "iwe" "-p" "iwec" "-p" "iwes" ];
          doCheck = false;
          env.IWE_GIT_SHA = self.shortRev or self.dirtyShortRev or "unknown";
          meta = {
            description = "iwe-plus ${workspace.version} (upstream iwe ${upstream})";
            homepage = "https://github.com/yuriikonovaliuk/iwe";
            license = pkgs.lib.licenses.asl20;
            mainProgram = "iwe";
          };
        };
        default = iwe-plus;
      });

      # The source tree, for flakes that build against a workspace member
      # (knowledge-compositor path-depends on crates/iwe-lock).
      lib.src = self;

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
