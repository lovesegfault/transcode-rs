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
    pre-commit-hooks = {
      url = "github:cachix/pre-commit-hooks.nix";
      inputs = {
        flake-compat.follows = "flake-compat";
        flake-utils.follows = "utils";
        nixpkgs-stable.follows = "nixpkgs";
        nixpkgs.follows = "nixpkgs";
      };
    };
    rust = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        flake-utils.follows = "utils";
        nixpkgs.follows = "nixpkgs";
      };
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
              svt-av1 = pipe prev.svt-av1 [ applyHost applyGraphite ];
            };

        x86-64-v3Opt = optimizedOverlayForHost {
          hostCFlags = [ "-march=x86-64-v3" ];
          hostGoFlags.GOAMD64 = "v3";
          hostRustflags = [ "-Ctarget-cpu=x86-64-v3" ];
        };

        ffmpegConfig = final: prev: {
          ffmpeg = prev.ffmpeg_6-headless.override {
            withBluray = true;
            withCelt = true;
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
        };

        config = { allowUnfree = true; };

        overlays = [
          rust.overlays.default
          ffmpegConfig
        ];

        pkgs = import nixpkgs {
          inherit localSystem config;
          overlays = overlays
            ++ (lib.optional (localSystem == "x86_64-linux") x86-64-v3Opt);
        };

        inherit (pkgs.stdenv) buildPlatform hostPlatform;

        rustTarget = pkgs.rust.toRustTargetSpec hostPlatform;
        rustTargetEnv = lib.replaceStrings [ "-" ] [ "_" ] (lib.toUpper rustTarget);

        rustToolchain = pkgs.pkgsBuildHost.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
          targets = [ rustTarget ];
        };

        craneLib = ((crane.mkLib pkgs).overrideToolchain rustToolchain).overrideScope' (_: _: {
          inherit (pkgs.pkgsLLVM) stdenv;
        });

        src = craneLib.cleanCargoSource (craneLib.path ./.);

        buildVars = rec {
          FFMPEG_PATH = "${pkgs.ffmpeg.bin}/bin/ffmpeg";
          FFPROBE_PATH = "${pkgs.ffmpeg.bin}/bin/ffprobe";

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

          propagatedBuildInputs = with pkgs; [ ffmpeg.bin ];
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
            nil
            nixpkgs-fmt
            statix
            rust-analyzer
            cargo-machete
            cargo-edit
          ];
        });

        checks = {
          audit = craneLib.cargoAudit { inherit src advisory-db; };

          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets";
          });

          rustfmt = craneLib.cargoFmt { inherit src; };

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
