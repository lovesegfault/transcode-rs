final: prev:
let
  pgoData = final.fetchzip {
    # c.f. https://gitlab.com/AOMediaCodec/SVT-AV1/-/blob/master/Build/obj1fastdownloader.cmake?ref_type=heads
    url = "https://media.xiph.org/video/derf/objective-1-fast.tar.gz";
    hash = "sha256-cKY0HEoyzrXKMdRGlM5xzsEK6aWDUMkdIj/TT8t0WGs=";
  };
in
{
  svt-av1 = prev.svt-av1.overrideAttrs (old: {
    cmakeFlags = (old.cmakeFlags or [ ]) ++ [
      "-DSVT_AV1_PGO=ON"
      "-DSVT_AV1_PGO_CUSTOM_VIDEOS=${pgoData}"
    ];
  });
}
