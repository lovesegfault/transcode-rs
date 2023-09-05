{
  inputs = {
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs.flake-compat.follows = "flake-compat";
      inputs.flake-utils.follows = "utils";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust";
    };
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    pre-commit-hooks = {
      url = "github:cachix/pre-commit-hooks.nix";
      inputs.flake-compat.follows = "flake-compat";
      inputs.flake-utils.follows = "utils";
      inputs.nixpkgs-stable.follows = "nixpkgs";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust = {
      url = "github:oxalica/rust-overlay";
      inputs.flake-utils.follows = "utils";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    utils.url = "github:numtide/flake-utils";
  };

  outputs = inputs: with inputs;
    utils.lib.eachDefaultSystem (localSystem:
      let
        inherit (nixpkgs) lib;

        pkgs = import nixpkgs {
          inherit localSystem;
          # NOTE: Edit this to cross-compile, e.g.
          # crossSystem = "aarch64-darwin"
          # crossSystem = lib.systems.examples.aarch64-multiplatform;
          crossSystem = localSystem;
          overlays = [ rust.overlays.default ];
        };

        inherit (pkgs.stdenv) buildPlatform hostPlatform;

        rustTarget = pkgs.rust.toRustTargetSpec hostPlatform;
        rustTargetEnv = lib.replaceStrings [ "-" ] [ "_" ] (lib.toUpper rustTarget);

        rustToolchain = pkgs.pkgsBuildHost.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
          targets = [ rustTarget ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        commonExpr = args: with args; ({
          src = craneLib.cleanCargoSource (craneLib.path ./.);

          nativeBuildInputs = [ pkg-config ];

          buildInputs = [
          ] ++ (lib.optionals stdenv.hostPlatform.isDarwin [
            libiconv
            darwin.apple_sdk.frameworks.Security
          ]);

          propagatedBuildInputs = [
            ffmpeg_6-full
            mediainfo
          ];

          CARGO_BUILD_TARGET = rustTarget;
          "CARGO_TARGET_${rustTargetEnv}_LINKER" = "${stdenv.cc.targetPrefix}cc";
          MEDIAINFO_PATH = "${mediainfo}/bin/mediainfo";
          FFMPEG_PATH = "${ffmpeg_6-full}/bin/ffmpeg";

        } // (lib.optionalAttrs (stdenv.buildPlatform != stdenv.hostPlatform) {
          depsBuildBuild = [ qemu ];
          "CARGO_TARGET_${rustTargetEnv}_RUNNER" = "qemu-${stdenv.hostPlatform.qemuArch}";
        }) // (args.extraAttrs or { }));

        buildExpr = builder:
          { stdenv
          , lib
          , pkg-config
          , ffmpeg_6-full
          , mediainfo
          , libiconv
          , qemu
          , darwin ? null
          , extraAttrs ? { }
          }@args:

          builder ({
            cargoArtifacts = craneLib.buildDepsOnly (commonExpr args);
          } // (commonExpr args));
      in
      {
        apps = {
          default = self.apps.${buildPlatform.system}.transcoders;
          transcoders = utils.lib.mkApp {
            drv = self.packages.${buildPlatform.system}.transcoders;
          };
        };

        packages = {
          default = self.packages.${buildPlatform.system}.transcoders;
          transcoders = pkgs.callPackage (buildExpr craneLib.buildPackage) { };
        };

        devShells.default = pkgs.callPackage
          (
            { mkShell
            , callPackage
            , nil
            , nixpkgs-fmt
            , statix
            , rust-analyzer
            }:
            (callPackage (buildExpr mkShell) { }).overrideAttrs (env: {
              cargoArtifacts = null;
              depsBuildBuild = (env.depsBuildBuild or [ ]) ++ [
                nil
                nixpkgs-fmt
                statix
                rust-analyzer
                rustToolchain
              ];
              inherit (self.checks.${buildPlatform.system}.pre-commit) shellHook;
            })
          )
          { };

        checks = {
          audit = pkgs.callPackage (buildExpr craneLib.cargoAudit) {
            extraAttrs = { inherit advisory-db; };
          };
          clippy = pkgs.callPackage (buildExpr craneLib.cargoClippy) {
            extraAttrs.cargoClippyExtraArgs = "--all-targets";
          };
          doc = pkgs.callPackage (buildExpr craneLib.cargoDoc) { };
          rustfmt = pkgs.callPackage (buildExpr craneLib.cargoFmt) { };

          pre-commit = pre-commit-hooks.lib.${buildPlatform.system}.run {
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
