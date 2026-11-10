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
              # eegg builds the zconn workspace crates from source
              # (path deps); their build.rs drives protoc.
              protobuf
            ];
            shellHook = ''
              # Dev default for eegg: the REAL core. eegg resolves its
              # core as --core flag > EEGG__CORE env > "zconned" on
              # PATH; the hook exports EEGG__CORE to a locally built
              # zconned when one exists in the shared target dir, so
              #   cargo build -p zconned --bin zconned && cargo run -p eegg-tui
              # is the full session. Without a built core the shell
              # still works — eegg falls back to a PATH lookup.
              # (fake-core stays the tests-only double, run explicitly
              # via --core.) Works for both in-repo and absolute
              # CARGO_TARGET_DIR layouts.
              for d in "''${CARGO_TARGET_DIR:-target}" "$PWD/target"; do
                case "$d" in
                  /*) t="$d" ;;
                  *) t="$PWD/$d" ;;
                esac
                if [ -x "$t/debug/zconned" ]; then
                  export EEGG__CORE="$t/debug/zconned"
                  break
                fi
              done
            '';
          };
        }
      );
    };
}
