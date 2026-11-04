{
  description = "Dial local-first coding agent harness";
  # Immutable nixos-unstable revision; update deliberately and regenerate flake.lock with Nix.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/1d060086d60cb76a40cf37109f979d88a5bd6396";
  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-darwin" "x86_64-darwin" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      packages = eachSystem (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "dial";
            # Single source of truth: the workspace version every crate in this source tree compiles with.
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
            src = self;
            # Nix removes .git from the source; carry the flake's commit into release metadata.
            env.DIAL_BUILD_REVISION = self.rev or (pkgs.lib.removeSuffix "-dirty" (self.dirtyRev or "unknown"));
            cargoLock.lockFile = ./Cargo.lock;
            # Only the `dial` binary package of the workspace is shipped.
            cargoBuildFlags = [ "-p" "dial" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];
            doCheck = false;
            meta = {
              description = "Local-first coding agent harness";
              homepage = "https://github.com/KiidxAtlas/dial";
              license = pkgs.lib.licenses.mit;
              mainProgram = "dial";
              platforms = systems;
            };
          };
        });
      apps = eachSystem (system: {
        default = { type = "app"; program = "${self.packages.${system}.default}/bin/dial"; };
      });
    };
}
