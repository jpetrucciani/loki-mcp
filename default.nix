{ pkgs ? import
    (fetchTarball {
      name = "jpetrucciani-2026-06-09";
      url = "https://github.com/jpetrucciani/nix/archive/f45975ed809a3256547956e92d51ac6f56762e60.tar.gz";
      sha256 = "0wq35nwqn9zf1kw9n8h3kchszc9nwpjbz51qyprajh18bnzfcxv1";
    })
    { overlays = [ _rust ]; }
, _rust ? import
    (fetchTarball {
      name = "oxalica-2026-06-04";
      url = "https://github.com/oxalica/rust-overlay/archive/c30ca201c5093540cf792f6982f81ba1aa0f3514.tar.gz";
      sha256 = "1hyirpyana0h23byn15l96nzzgj0nbvsg4fxpzxwvc3mjdw7pwm0";
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
