{
  lib,
  rustPlatform,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "meshtop";
  inherit (cargoToml.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  meta = {
    description = "Live resource usage and connections across your machines, in one terminal";
    homepage = "https://github.com/FormalSnake/meshtop";
    license = lib.licenses.mit;
    mainProgram = "meshtop";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
