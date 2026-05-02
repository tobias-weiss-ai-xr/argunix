{ lib, naersk }:

naersk.buildPackage {
  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.fileFilter (
      file:
      file.hasExt "rs" || file.hasExt "sql" || file.name == "Cargo.toml" || file.name == "Cargo.lock"
    ) ./..;
  };
}
