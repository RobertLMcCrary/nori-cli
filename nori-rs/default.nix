{
  lib,
  rustPlatform,
  stdenv,
  callPackage,
  pkg-config,
  dbus,
  # Run Codex inside Nori. It links all of codex-core, including V8, so turning it off gives a much
  # smaller and faster build, at the cost of needing an external Codex adapter.
  nativeCodex ? true,
  ...
}:
let
  v8 = callPackage ../nix/v8.nix { };
in
rustPlatform.buildRustPackage {
  pname = "nori";
  version = "0.0.0";
  src = lib.cleanSource ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
    # The git dependencies are the crossterm and tungstenite forks pinned in Cargo.toml, and the
    # upstream Codex crates native Codex is built from.
    allowBuiltinFetchGit = true;
  };
  cargoBuildFlags = [
    "--bin"
    "nori"
  ];
  buildNoDefaultFeatures = !nativeCodex;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [ dbus ];
  doCheck = false;

  env = lib.optionalAttrs nativeCodex {
    RUSTY_V8_ARCHIVE = v8.archive;
    RUSTY_V8_SRC_BINDING_PATH = v8.binding;
  };

  meta = {
    description = "Nori command-line interface, an agent-agnostic terminal UI over ACP";
    homepage = "https://github.com/tilework-tech/nori-cli";
    license = lib.licenses.asl20;
    mainProgram = "nori";
  };
}
