{
  description = "Myrica browser development environment";

  nixConfig = {
    extra-substituters = [ "https://scarlet-rust-toolchain.cachix.org" ];
    extra-trusted-public-keys = [
      "scarlet-rust-toolchain.cachix.org-1:p+coBExi0nNTIvWF/oM9H9/1/GhwFtqGZ2Vs+4pYl6o="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    scarlet-rust-toolchain.url = "github:petitstrawberry/scarlet-rust-nix";
    scarlet-sdk = {
      url = "github:petitstrawberry/scarlet-sdk";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      scarlet-rust-toolchain,
      scarlet-sdk,
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs supportedSystems (system: f system);

      mkSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          rustToolchain = scarlet-rust-toolchain.packages.${system}.scarlet-rust-toolchain;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
          imageTools = [
            pkgs.coreutils
            pkgs.e2fsprogs
            pkgs.mtools
          ];

          cargo-scarlet-plugin-limine = rustPlatform.buildRustPackage {
            pname = "cargo-scarlet-plugin-limine";
            version = "0.1.0";
            src = scarlet-sdk;
            buildAndTestSubdir = "cargo-scarlet-plugin-limine";
            cargoLock.lockFile = "${scarlet-sdk}/Cargo.lock";
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postInstall = ''
              wrapProgram "$out/bin/cargo-scarlet-plugin-limine" \
                --prefix PATH : ${pkgs.lib.makeBinPath ([ pkgs.git ] ++ imageTools)}
            '';
          };

          cargoScarletRuntimeTools = [
            rustToolchain
            cargo-scarlet-plugin-limine
            pkgs.git
            pkgs.pkg-config
            pkgs.curl
          ]
          ++ imageTools;

          cargo-scarlet = rustPlatform.buildRustPackage {
            pname = "cargo-scarlet";
            version = "0.1.0";
            src = scarlet-sdk;
            buildAndTestSubdir = "cargo-scarlet";
            cargoLock.lockFile = "${scarlet-sdk}/Cargo.lock";
            nativeBuildInputs = [ pkgs.makeWrapper ];
            nativeCheckInputs = imageTools ++ [ pkgs.curl ];
            postInstall = ''
              wrapProgram "$out/bin/cargo-scarlet" \
                --prefix PATH : ${pkgs.lib.makeBinPath cargoScarletRuntimeTools} \
                --set CARGO_NET_GIT_FETCH_WITH_CLI true \
                --set SCARLET_CACHED_RUST_TOOLCHAIN ${rustToolchain} \
                --set SCARLET_RUST_TOOLCHAIN ${rustToolchain}
            '';
          };
        in
        {
          packages = {
            inherit cargo-scarlet cargo-scarlet-plugin-limine;
            default = cargo-scarlet;
          };

          devShell = pkgs.mkShell {
            packages = [
              rustToolchain
              cargo-scarlet
              cargo-scarlet-plugin-limine
              pkgs.git
              pkgs.pkg-config
              pkgs.curl
            ]
            ++ imageTools;

            shellHook = ''
              export CARGO_NET_GIT_FETCH_WITH_CLI=true
              export SCARLET_CACHED_RUST_TOOLCHAIN=${rustToolchain}
              export SCARLET_RUST_TOOLCHAIN=${rustToolchain}
            '';
          };
        };
    in
    {
      packages = forAllSystems (system: (mkSystem system).packages);
      devShells = forAllSystems (system: {
        default = (mkSystem system).devShell;
      });
    };
}
