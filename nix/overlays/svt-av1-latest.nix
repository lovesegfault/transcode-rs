final: prev: {
  svt-av1 = prev.svt-av1.overrideAttrs (_: rec {
    version = "2.1.1";
    src = final.fetchFromGitLab {
      owner = "AOMediaCodec";
      repo = "SVT-AV1";
      rev = "v${version}";
      hash = "sha256-3rRIvSO4yIHzeRIQXGT460CMkIoHtlLJKSK/MhM3AHM=";
    };
  });
}
