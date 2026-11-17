# Adds `pkgs.nori`, built from this checkout.
self: final: _prev: {
  nori = final.callPackage "${self}/nori-rs" {
    pkl = final.pkl;
    java = final.temurin-bin-25;
    opencode = final.opencode;
  };
}
