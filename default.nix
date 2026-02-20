{ pkgs ? import
    (fetchTarball {
      name = "jpetrucciani-2026-01-21";
      url = "https://github.com/jpetrucciani/nix/archive/0fa40e09f3d6b7fe29811caef876444be9fa2a1a.tar.gz";
      sha256 = "16np1a2482l1s82yyxwh8d6igqqz4plc03fa9hv4mfricg2qicyi";
    })
    { overlays = [ _rust ]; }
, _rust ? import
    (fetchTarball {
      name = "oxalica-2026-01-21";
      url = "https://github.com/oxalica/rust-overlay/archive/2ef5b3362af585a83bafd34e7fc9b1f388c2e5e2.tar.gz";
      sha256 = "138a0p83qzflw8wj4a7cainqanjmvjlincx8imr3yq1b924lg9cz";
    })
}:
let
  name = "loki-mcp";

  target = "x86_64-unknown-linux-musl";
  rust = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default.override {
    extensions = [ "rust-src" "rustc-dev" "rust-analyzer" ];
    targets = [ target ];
  });

  rustPlatform = pkgs.makeRustPlatform {
    cargo = rust;
    rustc = rust;
  };

  tools = with pkgs; {
    cli = [
      grafana-loki
      jfmt
    ];
    node = [ bun ];
    rust = [
      cargo-zigbuild
      rust
      pkg-config
    ];
    scripts = pkgs.lib.attrsets.attrValues scripts;
  };

  scripts = with pkgs; {
    build_static = writers.writeBashBin "build_static" ''
      cargo zigbuild --release --target "x86_64-unknown-linux-musl"
    '';
  };
  paths = pkgs.lib.flatten [ (builtins.attrValues tools) ];
  env = pkgs.buildEnv {
    inherit name paths; buildInputs = paths;
  };
  bin = rustPlatform.buildRustPackage (finalAttrs: {
    pname = name;
    version = "0.0.0";
    src = pkgs.hax.filterSrc { path = ./.; };
    cargoLock.lockFile = ./Cargo.lock;
    auditable = false;
    nativeBuildInputs = with pkgs; [
      cargo-zigbuild
    ];
    buildPhase = ''
      export HOME=$(mktemp -d)
      cargo zigbuild --release --target ${target}
    '';
    installPhase = ''
      mkdir -p $out/bin
      cp target/${target}/release/${name} $out/bin/
    '';
  });
in
(env.overrideAttrs (_: {
  inherit name;
  NIXUP = "0.0.10";
})) // { inherit bin scripts; }
