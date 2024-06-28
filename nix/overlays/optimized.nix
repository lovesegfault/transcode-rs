final: prev:
let
  ffmpegConfig = {
    withFdkAac = true;
    withSvtav1 = true;
    withUnfree = true;

    # These cause infinite recursions, or depend on other ffmpeg
    # versions
    withSdl2 = false;
    withQuirc = false;
    withChromaprint = false;
    withOpenal = false;
    withFrei0r = false;

    # These balloon the closure size
    withCuda = false;
    withCudaLLVM = false;
    withSamba = false;
  };

  hostCflags = [
    "-march=znver2"
    "--param=l1-cache-line-size=64"
    "--param=l1-cache-size=32"
    "--param=l2-cache-size=512"
  ];

  optimizePackageClosure = pkg:
    let
      inherit (final) lib;

      appendFlags = new: old:
        if lib.isString old then lib.concatStringsSep " " ([ old ] ++ new)
        else if lib.isList old then lib.concatStringsSep " " (old ++ new)
        else (lib.concatStringsSep " " new);

      optCFlags = hostCflags ++ (lib.optionals (pkg.stdenv.cc.isGNU or false) [
        # These come from -O3
        "-fgcse-after-reload"
        "-floop-interchange"
        "-floop-unroll-and-jam"
        "-fpeel-loops"
        "-fpredictive-commoning"
        "-fsplit-loops"
        "-fsplit-paths"
        "-ftree-loop-distribution"
        "-ftree-partial-pre"
        "-funswitch-loops"
        "-fvect-cost-model=dynamic"
        "-fversion-loops-for-strides"
        "-fipa-cp-clone"
        # Graphite polyhedral optimizations
        "-fgraphite-identity"
        "-floop-nest-optimize"
        # Interprocedural pointer analysis
        "-fipa-pta"
      ]);

      appendOptimizedFlags = appendFlags optCFlags;

      brokenPkgNames = [
        "cracklib"
        "e2fsprogs"
        "glib"
        "gnutls"
        "sharutils"
      ];

      optimizedBuildInputs = p: lib.optionalAttrs (p ? buildInputs) {
        buildInputs = map optimizePackageClosure p.buildInputs;
      };

      optimizedPropagatedBuildInputs = p: lib.optionalAttrs (p ? propagatedBuildInputs) {
        propagatedBuildInputs = map optimizePackageClosure p.propagatedBuildInputs;
      };

      optimizedFlags = p:
        if p ? env then {
          env.NIX_CFLAGS_COMPILE = appendOptimizedFlags (p.env.NIX_CFLAGS_COMPILE or null);
        } else {
          NIX_CFLAGS_COMPILE = appendOptimizedFlags (p.NIX_CFLAGS_COMPILE or null);
        };
    in
    if (! (lib.isDerivation pkg)) then pkg
    else if (pkg ? pname && (builtins.elem pkg.pname brokenPkgNames)) then pkg
    else
      pkg.overrideAttrs (p:
        (optimizedFlags p)
        // (optimizedBuildInputs p)
        // (optimizedPropagatedBuildInputs p)
      )
  ;
in
{
  ffmpeg_7-full = (optimizePackageClosure prev.ffmpeg_7-full).override ffmpegConfig;
}
