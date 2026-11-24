{
  description = "BLE score-sync logger for the ESP-NOW timer (host CLI)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    { ... }@inputs:
    inputs.flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import inputs.nixpkgs { inherit system; };
        # Standard nixpkgs Rust toolchain: this is a host CLI, not firmware.
        craneLib = inputs.crane.mkLib pkgs;
        src = craneLib.cleanCargoSource ./.;

        # mingw-w64 cross toolchain (and Rust std) for 64-bit Windows.
        windowsPkgs = pkgs.pkgsCross.mingwW64;
        craneLibWindows = inputs.crane.mkLib windowsPkgs;

        # btleplug's BlueZ backend (dbus crate) needs the system D-Bus
        # library on Linux; other platforms use native frameworks. The
        # hostPlatform check drops these for the Windows cross build.
        commonArgsFor = p: {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = p.lib.optionals p.stdenv.hostPlatform.isLinux [ p.pkg-config ];
          buildInputs = p.lib.optionals p.stdenv.hostPlatform.isLinux [ p.dbus ];
        };
        commonArgs = commonArgsFor pkgs;
        windowsArgs = commonArgsFor windowsPkgs;

        # Deps-only derivations so dependency rebuilds are cached across
        # source edits.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        ble-logger = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });

        # Windows cross build of both binaries (bin/ble-logger.exe and
        # bin/player-card.exe): `nix build .#ble-logger-windows`.
        cargoArtifactsWindows = craneLibWindows.buildDepsOnly windowsArgs;
        ble-logger-windows = craneLibWindows.buildPackage (
          windowsArgs // { cargoArtifacts = cargoArtifactsWindows; }
        );

        treefmtconfig = inputs.treefmt-nix.lib.evalModule pkgs {
          projectRootFile = "flake.nix";
          programs = {
            nixfmt.enable = true;
            rustfmt = {
              enable = true;
              edition = "2021";
            };
            taplo.enable = true;
            mdformat = {
              enable = true;
              plugins = ps: [
                ps.mdformat-gfm
                ps.mdformat-frontmatter
              ];
              settings = {
                wrap = 88;
                end-of-line = "lf";
              };
            };
          };
        };
      in
      {
        packages = {
          default = ble-logger;
          inherit ble-logger ble-logger-windows;
        };

        apps = {
          default = {
            type = "app";
            program = "${ble-logger}/bin/ble-logger";
          };
          player-card = {
            type = "app";
            program = "${ble-logger}/bin/player-card";
          };
        };

        devShells.default = pkgs.mkShell {
          name = "ble-logger";
          inputsFrom = [ ble-logger ];
          packages = with pkgs; [
            cargo
            clippy
            rustc
            rustfmt
            rust-analyzer
            nil
            nixd
          ];
        };

        formatter = treefmtconfig.config.build.wrapper;
        checks = {
          formatting = treefmtconfig.config.build.check inputs.self;
          inherit ble-logger;
        };
      }
    );
}
