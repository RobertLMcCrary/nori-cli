# The prebuilt V8 static library and its Rust bindings, which the `v8` crate downloads while it
# builds. A Nix build has no network, so they are fetched here and handed to it. The version is
# the `v8` crate's in nori-rs/Cargo.lock; raise it and refresh the hashes together
# (`nix store prefetch-file <url>`).
{ fetchurl, stdenv }:
let
  version = "147.4.0";
  base = "https://github.com/denoland/rusty_v8/releases/download/v${version}";
  target = stdenv.hostPlatform.rust.rustcTarget;
  hashes = {
    aarch64-apple-darwin = {
      archive = "sha256-fnR0DD7woOj8DiaKJYYSPpg0D+lDVmjNwSiPrvtzYq4=";
      binding = "sha256-3iP6spGP5JfZL7+79u+1MbWKoVW0xsCt1T9X64UWEVQ=";
    };
    x86_64-apple-darwin = {
      archive = "sha256-+ppR8dMhVTSZL0PPar+DlKZ0K+E5N7WfdXXfBTYel+Y=";
      binding = "sha256-3iP6spGP5JfZL7+79u+1MbWKoVW0xsCt1T9X64UWEVQ=";
    };
    x86_64-unknown-linux-gnu = {
      archive = "sha256-Cd3vbFEZKv/wVBExoO+cAPgxhdI5HaqxgDgqOr82rJU=";
      binding = "sha256-Cd7niar3f7PV3OxVamLnijQsxKAvjPNuD0hUL3ZFvzk=";
    };
    aarch64-unknown-linux-gnu = {
      archive = "sha256-lMPw/eAFFAT8obaR8opJbXjbgw58+0maBEyxpeOllFU=";
      binding = "sha256-Cd7niar3f7PV3OxVamLnijQsxKAvjPNuD0hUL3ZFvzk=";
    };
  };
  pinned = hashes.${target} or (throw "no prebuilt V8 is pinned for ${target}");
in
{
  archive = fetchurl {
    name = "librusty_v8-${version}";
    url = "${base}/librusty_v8_release_${target}.a.gz";
    hash = pinned.archive;
  };
  binding = fetchurl {
    name = "src_binding-${version}";
    url = "${base}/src_binding_release_${target}.rs";
    hash = pinned.binding;
  };
}
