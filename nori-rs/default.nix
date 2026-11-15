{
  lib,
  rustPlatform,
  stdenv,
  pkg-config,
  dbus,
  ...
}:
rustPlatform.buildRustPackage {
  pname = "nori";
  version = "0.0.0";
  src = lib.cleanSource ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
    # The only git dependency is the crossterm fork pinned in Cargo.toml.
    allowBuiltinFetchGit = true;
  };
  cargoBuildFlags = [
    "--bin"
    "nori"
  ];

  nativeBuildInputs = [ pkg-config ];
  # TLS is rustls over ring and regexes are fancy-regex, so the only system
  # library left is libdbus for the Linux keyring.
  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [ dbus ];
  doCheck = false;

  meta = {
    description = "Nori command-line interface, an agent-agnostic terminal UI over ACP";
    homepage = "https://github.com/tilework-tech/nori-cli";
    license = lib.licenses.asl20;
    mainProgram = "nori";
  };
}
