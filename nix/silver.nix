# Derivations for silver: the web UI (apps/web) and the Rust binary that embeds it.
#
# Imported from flake.nix:
#   import ./silver.nix { inherit (pkgs) lib rustPlatform buildNpmPackage importNpmLock nodejs pkg-config openssl; src = ...; }
#
# `src` is the repository root. The flake lives under nix/, so the root arrives
# as a flake input; `cargoLock.lockFile` and importNpmLock read Cargo.lock and
# apps/web/package-lock.json, keeping both builds offline and reproducible.
{
  lib,
  rustPlatform,
  buildNpmPackage,
  importNpmLock,
  nodejs,
  pkg-config,
  openssl,
  src,
  version ? "0.1.0",
}:

let
  # Keep build outputs and local tool caches out of the source hash.
  cleanSrc = lib.cleanSourceWith {
    src = src;
    filter =
      path: type:
      let
        base = builtins.baseNameOf (toString path);
        pruned = type == "directory" && (
          base == ".cargo-home"
          || base == "ref"
          || base == "node_modules"
          || base == "dist"
          || lib.hasPrefix "target" base
        );
      in
      !pruned;
  };

  web = buildNpmPackage {
    pname = "silver-web";
    inherit version nodejs;
    src = cleanSrc + "/apps/web";
    npmDeps = importNpmLock { npmRoot = cleanSrc + "/apps/web"; };
    npmConfigHook = importNpmLock.npmConfigHook;
    installPhase = ''
      runHook preInstall
      cp -r dist $out
      runHook postInstall
    '';
  };

  silver = rustPlatform.buildRustPackage {
    pname = "silver";
    inherit version;
    src = cleanSrc;
    cargoLock = {
      lockFile = cleanSrc + "/Cargo.lock";
    };
    cargoBuildFlags = [ "-p" "silver" ];
    cargoInstallFlags = [ "-p" "silver" ];
    # rust-embed compiles apps/web/dist into the binary.
    preBuild = ''
      cp -r ${web} apps/web/dist
      chmod -R u+w apps/web/dist
    '';
    # reqwest keeps its default native-tls feature enabled alongside rustls, so
    # system OpenSSL is required at build time.
    nativeBuildInputs = [ pkg-config ];
    buildInputs = [ openssl ];
    strictDeps = true;
    # The test suite binds TCP ports and needs a writable HOME. The scored,
    # network-free scenarios run separately via scripts/run_evals.sh.
    doCheck = false;
    meta = {
      description = "silver: a personal coding agent (HTTP API and embedded web UI)";
      license = lib.licenses.unlicense;
      platforms = lib.platforms.unix;
      mainProgram = "silver";
    };
  };
in
{
  inherit silver web;
}
