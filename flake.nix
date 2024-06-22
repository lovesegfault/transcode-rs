{
  inputs = {
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs = {
        flake-compat.follows = "flake-compat";
        nixpkgs-stable.follows = "nixpkgs";
        nixpkgs.follows = "nixpkgs";
      };
    };
    rust = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    utils.url = "github:numtide/flake-utils";
    ffmpeg-opt = {
      url = "github:lovesegfault/nix-ffmpeg-opt";
      inputs = {
        flake-compat.follows = "flake-compat";
        nixpkgs.follows = "nixpkgs";
        git-hooks.follows = "git-hooks";
        utils.follows = "utils";
      };
    };
  };

  outputs = inputs: with inputs;
    utils.lib.eachDefaultSystem (localSystem:
      let
        inherit (nixpkgs) lib;
        overlays = [
          rust.overlays.default
        ];

        pkgs = import nixpkgs {
          inherit localSystem overlays;
          config = { allowUnfree = true; };
        };

        inherit (pkgs.stdenv) buildPlatform hostPlatform;

        rustTarget = pkgs.rust.toRustTargetSpec hostPlatform;
        rustTargetEnv = lib.replaceStrings [ "-" ] [ "_" ] (lib.toUpper rustTarget);

        rustToolchain = pkgs.pkgsBuildHost.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
          targets = [ rustTarget ];
        };

        craneLib = ((crane.mkLib pkgs).overrideToolchain rustToolchain).overrideScope (_: _: {
          inherit (pkgs.pkgsLLVM) stdenv;
        });

        src = craneLib.cleanCargoSource (craneLib.path ./.);

        inherit (inputs.ffmpeg-opt.packages.${localSystem}) ffmpeg-optimized;

        buildVars = rec {
          FFMPEG_PATH = "${ffmpeg-optimized.bin}/bin/ffmpeg";
          FFPROBE_PATH = "${ffmpeg-optimized.bin}/bin/ffprobe";

          CFLAGS = "-flto -fuse-ld=lld"
            + lib.optionalString pkgs.stdenv.hostPlatform.isx86_64 " -march=x86-64-v3";
          CXXFLAGS = CFLAGS;

          CARGO_BUILD_TARGET = rustTarget;
          "CARGO_TARGET_${rustTargetEnv}_LINKER" = "clang";
          "CARGO_TARGET_${rustTargetEnv}_RUSTFLAGS" = "-Clink-arg=-fuse-ld=lld"
            + lib.optionalString pkgs.stdenv.hostPlatform.isx86_64 " -Ctarget-cpu=x86-64-v3";

        } // (lib.optionalAttrs (pkgs.stdenv.buildPlatform != pkgs.stdenv.hostPlatform) {
          "CARGO_TARGET_${rustTargetEnv}_RUNNER" = "qemu-${pkgs.stdenv.hostPlatform.qemuArch}";
        });

        commonArgs = buildVars // {
          inherit src;

          strictDeps = true;

          depsBuildBuild = with pkgs; lib.optionals (stdenv.buildPlatform != stdenv.hostPlatform) [
            qemu
          ];

          nativeBuildInputs = with pkgs; [
            pkg-config
            llvmPackages.clangUseLLVM
            llvmPackages.bintools
          ];

          buildInputs = with pkgs; lib.optionals stdenv.hostPlatform.isDarwin [
            libiconv
            darwin.apple_sdk.frameworks.Security
          ];

          propagatedBuildInputs = [ ffmpeg-optimized.bin ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        transcode-rs = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });
      in
      {
        apps = {
          default = self.apps.${buildPlatform.system}.transcode-rs;
          transcode-rs = utils.lib.mkApp {
            drv = self.packages.${buildPlatform.system}.transcode-rs;
          };
        };

        packages = {
          default = self.packages.${buildPlatform.system}.transcode-rs;
          inherit transcode-rs;
        };

        devShells.default = craneLib.devShell (buildVars // {
          checks = self.checks.${buildPlatform.system};
          packages = with pkgs; [
            cargo-edit
            cargo-machete
            rust-analyzer

            nil
            nix-tree
            nixpkgs-fmt
            statix
          ];
        });

        checks = {
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets";
          });

          rustfmt = craneLib.cargoFmt { inherit src; };

          pre-commit = git-hooks.lib.${buildPlatform.system}.run {
            src = ./.;
            hooks = {
              actionlint.enable = true;
              nixpkgs-fmt.enable = true;
              statix.enable = true;
              rustfmt.enable = true;
            };
            tools = {
              cargo = rustToolchain;
              rustfmt = rustToolchain;
            };
          };
        };
      }
    );

}
