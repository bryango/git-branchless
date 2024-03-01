{
  description = "git-branchless";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      lib = nixpkgs.lib;
      systems = [
        "aarch64-linux"
        "aarch64-darwin"
        "i686-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      foreachSystem = lib.genAttrs systems;
    in
    {
      overlays.default = (
        final: prev: {

          # reuse the definition from nixpkgs git-branchless
          git-branchless = prev.git-branchless.overrideAttrs (
            { meta, ... }:
            {
              name = "git-branchless";
              src = self;
              version = with builtins; (fromTOML (readFile ./git-branchless/Cargo.toml)).package.version;
              cargoDeps = final.rustPlatform.importCargoLock {
                lockFile = ./Cargo.lock;
              };

              # for `flake.nix` contributors: put additional local overrides here.
              # if the changes are also applicable to the `git-branchless` package
              # in nixpkgs, consider first improving the definition there, and then
              # update the `flake.lock` here.

              # Upstream has patch for https://github.com/arxanas/git-branchless/issues/1585 in it, this fails as we have updated already on master
              postPatch = "";

              # in case local overrides might confuse upstream maintainers,
              # we do not list them here:
              meta = (removeAttrs meta [ "maintainers" ]) // {
                # to correctly generate meta.position for back trace:
                inherit (meta) description;
              };
            }
          );

          # reuse the definition for git-branchless
          scm-diff-editor = final.git-branchless.overrideAttrs (
            finalAttrs: prevAttrs: {
              name = "scm-diff-editor";
              meta = prevAttrs.meta // {
                mainProgram = finalAttrs.name;
                description = "UI to interactively select changes, bundled in git-branchless";
              };

              buildAndTestSubdir = "scm-record";
              buildFeatures = [ "scm-diff-editor" ];

              # remove the git-branchless specific build commands
              postInstall = "";
              preCheck = "";
              checkFlags = "";
            }
          );
        }
      );

      devShells = foreachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system}.extend self.overlays.default;
        in
        {
          default = pkgs.mkShell {
            name = "git-branchless";
            packages = with pkgs; [
              nixfmt
              cargo
              cargo-insta
              rustc
              rustfmt
              rust-analyzer
              clippy
              git
            ];

            env = with pkgs; {
              # for developments, e.g. symbol lookup in std library
              RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
              # for testing
              TEST_GIT = "${git}/bin/git";
              TEST_GIT_EXEC_PATH = "${git}/libexec/git-core";
            };
          };
        }
      );

      packages = foreachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system}.extend self.overlays.default;
        in
        {
          inherit (pkgs)
            git-branchless
            scm-diff-editor
            ;
          default = pkgs.git-branchless;
        }
      );

      checks = foreachSystem (system: {
        git-branchless = self.packages.${system}.git-branchless.overrideAttrs (
          { preCheck, ... }:
          {
            cargoBuildType = "debug";
            cargoCheckType = "debug";
            preCheck = ''
              export RUST_BACKTRACE=1
            ''
            + preCheck;
          }
        );
      });
    };
}
