{
  lib,
  rustPlatform,
  git,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "meshtop";
  inherit (cargoToml.workspace.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # The worktree and skills tests drive real git repos in a temp dir.
  nativeCheckInputs = [ git ];

  meta = {
    description = "Live resource usage and connections across your machines, in one terminal";
    homepage = "https://github.com/FormalSnake/meshtop";
    license = lib.licenses.mit;
    mainProgram = "meshtop";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
