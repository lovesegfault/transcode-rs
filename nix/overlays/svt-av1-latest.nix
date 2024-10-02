final: prev: {
  svt-av1 = prev.svt-av1.overrideAttrs (_: rec {
    version = "2.3.0-rc1";
    src = final.fetchFromGitLab {
      owner = "AOMediaCodec";
      repo = "SVT-AV1";
      rev = "v${version}";
      hash = "sha256-jrfnUcDTbrf3wWs0D57ueeLmndhpOQChM7gBB14MzcQ=";
    };
  });
}
