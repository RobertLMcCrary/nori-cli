{
  lib,
  rustPlatform,
  stdenv,
  cmake,
  pkg-config,
  dbus,
  zstd,
  sqlite,
  fetchurl,
  fetchFromGitHub,
  makeWrapper,
  writeShellScriptBin,
  pkl ? null,
  java ? null,
  opencode ? null,
  nodejs ? null,
  geminiCoreEntry ? null,
  nativeGemini ? false,
  # Run Codex inside Nori. It links all of codex-core, so turning it off gives a much smaller and
  # faster build, at the cost of needing an external Codex adapter.
  nativeCodex ? true,
  ...
}:
let
  # Codex ships the optional Code Mode host in its own workspace. Keep this
  # pinned to the exact revision used by Cargo.lock so the host protocol and
  # runtime match the embedded Codex crates.
  codexSource = "${fetchFromGitHub {
    owner = "openai";
    repo = "codex";
    rev = "d32063f96c2dff1dc96af839b3952abb6acfeda4";
    hash = "sha256-p8Vw0Vu8ufPY35k/RkcKE6HOTMJta2J9W33ei/7in+A=";
  }}/codex-rs";
  rustyV8Source = fetchFromGitHub {
    owner = "openai-oss-forks";
    repo = "rusty_v8";
    rev = "90a65f887a0c0f048f3784afbcb320b373c8a2e0";
    hash = "sha256-BB4a6uIcuZFmbkbXzdKOPq2vTu6pfZivPkHliOHnG3c=";
  };
  # Codex publishes a prebuilt V8 archive/binding pair for Darwin arm64. This
  # avoids compiling V8 in the Nix sandbox (the other targets retain Cargo's
  # normal source-build path until their release pair is pinned here).
  v8Artifacts = lib.optionalAttrs (stdenv.hostPlatform.isDarwin && stdenv.hostPlatform.isAarch64) {
    archive = fetchurl {
      url = "https://github.com/openai/codex/releases/download/rusty-v8-v152.2.0/librusty_v8_ptrcomp_sandbox_release_aarch64-apple-darwin.a.gz";
      hash = "sha256-/qua4DhL66qZwdAAZURiydQhI9WHGFx4dqru99dv90k=";
    };
    binding = fetchurl {
      url = "https://github.com/openai/codex/releases/download/rusty-v8-v152.2.0/src_binding_ptrcomp_sandbox_release_aarch64-apple-darwin.rs";
      hash = "sha256-udVlLrylFesr9MLuSmVRslZxF7Wqcg9DDSAyYW3X47U=";
    };
  };
  codexHostLock = builtins.toFile "Cargo.lock" (
    builtins.replaceStrings
      [ "source = \"git+https://github.com/openai-oss-forks/rusty_v8.git?rev=90a65f887a0c0f048f3784afbcb320b373c8a2e0#90a65f887a0c0f048f3784afbcb320b373c8a2e0\"\n" ]
      [ "" ]
      (builtins.readFile "${codexSource}/Cargo.lock")
  );
  codeModeHost = if nativeCodex then rustPlatform.buildRustPackage {
    pname = "codex-code-mode-host";
    version = "0.169.0";
    src = codexSource;
    cargoLock = {
      lockFile = codexHostLock;
      allowBuiltinFetchGit = true;
    };
    cargoBuildFlags = [ "-p" "codex-code-mode-host" ];
    postPatch = ''
      cp -R ${rustyV8Source} rusty-v8-source
      substituteInPlace Cargo.toml \
        --replace-fail 'v8 = { git = "https://github.com/openai-oss-forks/rusty_v8.git", rev = "90a65f887a0c0f048f3784afbcb320b373c8a2e0" }' 'v8 = { path = "rusty-v8-source" }'
      cp ${codexHostLock} Cargo.lock
    '';
    nativeBuildInputs = [ pkg-config cmake ];
    buildInputs = [ zstd sqlite ] ++ lib.optionals stdenv.hostPlatform.isLinux [ dbus ];
    env = {
      AWS_LC_SYS_CMAKE_BUILDER = "1";
    } // lib.optionalAttrs (v8Artifacts != { }) {
      RUSTY_V8_ARCHIVE = v8Artifacts.archive;
      RUSTY_V8_SRC_BINDING_PATH = v8Artifacts.binding;
    };
    preBuild = ''
      export AWS_LC_SYS_USE_SYSTEM=0
    '';
    doCheck = false;
  } else null;
  pklRuntime = if pkl != null && java != null then writeShellScriptBin "pkl" ''
    unset _JAVA_OPTIONS
    exec ${java}/bin/java --enable-native-access=ALL-UNNAMED --sun-misc-unsafe-memory-access=allow -Dpolyglotimpl.AttachLibraryFailureAction=ignore -jar ${pkl}/opt/pkl/jpkl.jar "$@"
  '' else pkl;
in
assert !nativeGemini || (nodejs != null && geminiCoreEntry != null);
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
  ] ++ lib.optionals nativeGemini [ "--features" "native-gemini" ];
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
    makeWrapper
  ];
  # zstd-sys probes pkg-config for libzstd before it falls back to its bundled copy, so the
  # library is provided here.
  # sqlite is linked by name (-lsqlite3); the Nix SDK on macOS does not provide it.
  buildInputs = [
    zstd
    sqlite
  ] ++ lib.optionals stdenv.hostPlatform.isLinux [ dbus ];
  doCheck = false;

  postInstall = lib.optionalString nativeCodex ''
    install -Dm755 ${codeModeHost}/bin/codex-code-mode-host $out/bin/codex-code-mode-host
  '' + lib.optionalString (pklRuntime != null || opencode != null || nativeGemini) ''
    mv $out/bin/nori $out/bin/.nori-real
    makeWrapper $out/bin/.nori-real $out/bin/nori \
      --prefix PATH : ${lib.makeBinPath (lib.optional (pklRuntime != null) pklRuntime ++ lib.optional (opencode != null) opencode ++ lib.optional nativeGemini nodejs)} \
      ${lib.optionalString nativeGemini "--set-default NORI_GEMINI_CORE_MODULE ${geminiCoreEntry}"}
  '';

  passthru = {
    inherit codeModeHost pklRuntime;
  };

  meta = {
    description = "Nori command-line interface, an agent-agnostic terminal UI over ACP";
    homepage = "https://github.com/tilework-tech/nori-cli";
    license = lib.licenses.asl20;
    mainProgram = "nori";
  };
}
