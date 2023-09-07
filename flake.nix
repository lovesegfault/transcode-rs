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

        optimizedOverlayForHost = { hostCFlags ? [ ], hostRustflags ? [ ], hostGoFlags ? { } }:
          final: prev:
            let
              inherit (prev.lib) concatStringsSep optionalAttrs pipe;

              appendFlags = new: old:
                with builtins;
                if isString old then concatStringsSep " " ([ old ] ++ new)
                else if isList old then concatStringsSep " " (old ++ new)
                else (concatStringsSep " " new);

              applyFlags = { cflags ? [ ], rustflags ? [ ], goflags ? { } }: pkg:
                pkg.overrideAttrs (old:
                  (optionalAttrs (cflags != [ ]) {
                    NIX_CFLAGS_COMPILE = appendFlags cflags (old.NIX_CFLAGS_COMPILE or null);
                    NIX_CFLAGS_LINK = appendFlags cflags (old.NIX_CFLAGS_LINK or null);
                  })
                  // (optionalAttrs (rustflags != [ ]) {
                    CARGO_BUILD_RUSTFLAGS = appendFlags rustflags (old.CARGO_BUILD_RUSTFLAGS or null);
                  })
                  // goflags
                );

              applyHost = applyFlags { cflags = hostCFlags; goflags = hostGoFlags; rustflags = hostRustflags; };
              applyGraphite = applyFlags { cflags = [ "-fgraphite-identity" "-floop-nest-optimize" ]; };
            in
            {
              ffmpeg = pipe (prev.ffmpeg.override { withFrei0r = false; }) [ applyHost applyGraphite ];
              x265 = pipe prev.x265 [ applyHost applyGraphite ];
            };

        skylakeOverlay = optimizedOverlayForHost {
          hostCFlags = [
            "-march=skylake"
            "-mabm"
            "--param=l1-cache-line-size=64"
            "--param=l1-cache-size=32"
            "--param=l2-cache-size=8192"
          ];
          hostGoFlags.GOAMD64 = "v3";
          hostRustflags = [ "-Ctarget-cpu=skylake" ];
        };

        config = { allowUnfree = true; };

        overlays = [
          rust.overlays.default
          (final: _: {
            ffmpeg = final.ffmpeg_6-headless.override {
              withBluray = true;
              withCelt = true;
              withCrystalhd = true;
              withFdkAac = true;
              withGlslang = !final.stdenv.isDarwin;
              withGsm = true;
              withLibplacebo = !final.stdenv.isDarwin;
              withOpencl = true;
              withOpenh264 = true;
              withOpenjpeg = true;
              withOpenmpt = true;
              withRav1e = true;
              withSvg = true;
              withSvtav1 = !final.stdenv.isAarch64;
              withVoAmrwbenc = true;
              withVulkan = !final.stdenv.isDarwin;
              withXml2 = true;
              withUnfree = true;
            };
          })
        ];

        pkgs = import nixpkgs {
          inherit localSystem overlays config;
          # NOTE: Edit this to cross-compile, e.g.
          # crossSystem = "aarch64-darwin"
          # crossSystem = lib.systems.examples.aarch64-multiplatform;
          crossSystem = localSystem;
        };
        skylakePkgs = import nixpkgs {
          inherit localSystem config;
          crossSystem = "x86_64-linux";
          overlays = overlays ++ [ skylakeOverlay ];
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

          nativeBuildInputs = [
            pkg-config
            llvmPackages_latest.clang
          ];

          buildInputs = [
            ffmpeg
          ] ++ (lib.optionals stdenv.hostPlatform.isDarwin [
            libiconv
            darwin.apple_sdk.frameworks.Security
          ]);

          propagatedBuildInputs = [
            ffmpeg
          ];

          FFMPEG_PATH = "${ffmpeg}/bin/ffmpeg";
          LIBCLANG_PATH = "${llvmPackages_latest.libclang.lib}/lib";

          CARGO_BUILD_TARGET = rustTarget;
          "CARGO_TARGET_${rustTargetEnv}_LINKER" = "${stdenv.cc.targetPrefix}cc";
        } // (lib.optionalAttrs (stdenv.buildPlatform != stdenv.hostPlatform) {
          depsBuildBuild = [ qemu ];
          "CARGO_TARGET_${rustTargetEnv}_RUNNER" = "qemu-${stdenv.hostPlatform.qemuArch}";
        }) // (args.extraAttrs or { }));

        buildExpr = builder:
          { stdenv
          , lib
          , pkg-config
          , ffmpeg
          , llvmPackages_latest
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
          transcodersSkylake = skylakePkgs.callPackage (buildExpr craneLib.buildPackage) { };
        };

        inherit pkgs;

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
