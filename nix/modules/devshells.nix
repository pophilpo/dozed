{ inputs, ... }:
{
  perSystem =
    { pkgs, system, ... }:
    let
      # NOTE: Duplicated because this is in a separate flake-parts partition
      # than ./packages.nix
      mkZed = import ../toolchain.nix { inherit inputs; };
      zed-editor = mkZed pkgs;
      zedBuildArgs = zed-editor.passthru.commonArgs;

      # mdBook pinned to 0.4.40 via a dedicated nixpkgs input, because the docs
      # rely on behavior that newer mdBook releases break (see
      # `crates/docs_preprocessor/Cargo.toml`).
      mdbook = (import inputs.nixpkgs-mdbook { inherit system; }).mdbook;

      rustBin = inputs.rust-overlay.lib.mkRustBin { } pkgs;
      rustToolchain = rustBin.fromRustupToolchainFile ../../rust-toolchain.toml;

      # Musl cross-compiler for building remote_server
      muslCross = pkgs.pkgsCross.musl64;

      # Cargo build timings wrapper script
      wrappedCargo = pkgs.writeShellApplication {
        name = "cargo";
        runtimeInputs = [ pkgs.nodejs ];
        text =
          let
            pathToCargoScript = ./. + "/../../script/cargo";
          in
          ''
            NIX_WRAPPER=1 CARGO=${rustToolchain}/bin/cargo ${pathToCargoScript} "$@"
          '';
      };
    in
    {
      devShells.default = (pkgs.mkShell.override { inherit (zed-editor) stdenv; }) {
        name = "zed-editor-dev";
        nativeBuildInputs = zedBuildArgs.nativeBuildInputs;
        buildInputs = zedBuildArgs.buildInputs;

        packages =
          with pkgs;
          [
            wrappedCargo # must be first, to shadow the `cargo` provided by `rustToolchain`
            rustToolchain # cargo, rustc, and rust-toolchain.toml components included
            cargo-nextest
            cargo-hakari
            cargo-machete
            cargo-zigbuild
            direnv
            # TODO: package protobuf-language-server for editing zed.proto
            # TODO: add other tools used in our scripts

            # `build.nix` adds this to the `zed-editor` wrapper (see `postFixup`)
            # we'll just put it on `$PATH`:
            nodejs_22
            zig

            # Documentation tooling: `nix develop -c mdbook build docs`
            mdbook

            # A11y testing infra
            gobject-introspection
            at-spi2-core
            (python3.withPackages (ps: [
              ps.pyatspi
              ps.pygobject3
            ]))
          ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [ accerciser ];

        env = {
          ZSTD_SYS_USE_PKG_CONFIG = true;
          FONTCONFIG_FILE = pkgs.makeFontsConf {
            fontDirectories = [
              "./assets/fonts/lilex"
              "./assets/fonts/ibm-plex-sans"
            ];
          };
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          NIX_LDFLAGS = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux "-rpath ${
            pkgs.lib.makeLibraryPath [
              pkgs.vulkan-loader
              pkgs.wayland
              pkgs.libva
            ]
          }";
          ZED_ZSTD_MUSL_LIB = "${pkgs.pkgsCross.musl64.pkgsStatic.zstd.out}/lib";
          CC_x86_64_unknown_linux_musl = "${muslCross.stdenv.cc}/bin/x86_64-unknown-linux-musl-gcc";
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
          NIX_CFLAGS_LINK = "-fuse-ld=lld";
        };
      };
    };
}
