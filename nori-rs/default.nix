{
  lib,
  rustPlatform,
  stdenv,
  cmake,
  pkg-config,
  dbus,
  zstd,
  sqlite,
  # Run Codex inside Nori. It links all of codex-core, so turning it off gives a much smaller and
  # faster build, at the cost of needing an external Codex adapter.
  nativeCodex ? true,
  ...
}:
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

  # aws-lc-sys (through rustls) builds AWS-LC from source. The cmake builder matches nixpkgs' own
  # override for this crate.
  env.AWS_LC_SYS_CMAKE_BUILDER = "1";
  # The build shell sets AWS_LC_SYS_USE_SYSTEM=1, which makes aws-lc-sys demand a system AWS-LC
  # that nixpkgs does not provide. The derivation env value of 0 does not reach cargo, so the
  # variable is exported again in the build shell.
  preBuild = ''
    export AWS_LC_SYS_USE_SYSTEM=0
  '';

  # mxc-sdk's build script checks the `windows` version in the first Cargo.lock above it, which in
  # the vendor directory is Nori's (windows 0.58 from Codex's Windows sandbox). Its bindings are only
  # used on Windows, and without the generation record the check is skipped.
  postPatch = ''
    chmod -R u+w ../cargo-vendor-dir/mxc-sdk-1.0.0/build/isolation_session_bindings
    rm -f ../cargo-vendor-dir/mxc-sdk-1.0.0/build/isolation_session_bindings/GENERATION_INFO.toml
  '';

  nativeBuildInputs = [
    pkg-config
    cmake
  ];
  # zstd-sys probes pkg-config for libzstd before it falls back to its bundled copy, so the
  # library is provided here.
  # sqlite is linked by name (-lsqlite3); the Nix SDK on macOS does not provide it.
  buildInputs = [
    zstd
    sqlite
  ] ++ lib.optionals stdenv.hostPlatform.isLinux [ dbus ];
  doCheck = false;

  meta = {
    description = "Nori command-line interface, an agent-agnostic terminal UI over ACP";
    homepage = "https://github.com/tilework-tech/nori-cli";
    license = lib.licenses.asl20;
    mainProgram = "nori";
  };
}
