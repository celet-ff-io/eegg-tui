{
  description = "eegg-tui";

  # No ref suffix: the machine registry redirects the bare github input
  # to the local nixpkgs mirror (offline-friendly; same shape as zconn).
  inputs.nixpkgs.url = "github:NixOS/nixpkgs";

  outputs =
    { self, ... }@inputs:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forEachSupportedSystem =
        f:
        inputs.nixpkgs.lib.genAttrs supportedSystems (
          system:
          f {
            inherit system;
            pkgs = import inputs.nixpkgs {
              inherit system;
            };
          }
        );
    in
    {
      devShells = forEachSupportedSystem (
        { pkgs, system }:
        let
          rustcSysroot = pkgs.symlinkJoin {
            name = "rustc-sysroot-with-completion";
            paths = [ pkgs.rustc.unwrapped ];
            postBuild = ''
              mkdir -p $out/share/zsh/site-functions
              install ${pkgs.cargo}/share/zsh/site-functions/_cargo $out/share/zsh/site-functions/
            '';
          };
        in
        {
          default = pkgs.mkShellNoCC {
            packages = with pkgs; [
              cargo
              (symlinkJoin {
                name = "rustc-with-completion";
                paths = [ rustc ];
                nativeBuildInputs = [ makeWrapper ];
                postBuild = ''
                  rm $out/bin/rustc
                  makeWrapper ${rustc}/bin/rustc $out/bin/rustc \
                    --add-flags "--sysroot=${rustcSysroot}"
                '';
              })
              rustc
              rust-analyzer
              rustfmt
              clippy
              # eegg builds the LPZ106 crates from source (path deps);
              # their build.rs drives protoc.
              protobuf
            ];
            shellHook = ''
              # Dev default for eegg: point it at the locally built fake
              # core, so shell testing is just
              #   cargo build && cargo run -p eegg-tui
              # Works for both in-repo and absolute CARGO_TARGET_DIR layouts;
              # override the env var manually for anything else.
              case "''${CARGO_TARGET_DIR:-target}" in
                /*) target_dir="''${CARGO_TARGET_DIR:-target}" ;;
                *) target_dir="$PWD/''${CARGO_TARGET_DIR:-target}" ;;
              esac
              export EEGG__CORE="$target_dir/debug/fake-core"
            '';
          };
        }
      );
    };
}
