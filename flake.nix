{
  description = "Dial local-first coding agent harness";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
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
