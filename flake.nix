{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    nci = {
      url = "github:yusdacra/nix-cargo-integration";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {
    parts,
    nci,
    ...
  }:
    parts.lib.mkFlake {inherit inputs;} ({config, ...}: let
      outPkgsFn = pkgs: config.perSystem.${pkgs.system}.packages;
    in {
      systems = ["x86_64-linux"];
      imports = [
        nci.flakeModule
        inputs.home-manager.flakeModules.home-manager
      ];

      flake.homeModules = {
        duat = {
          config,
          pkgs,
          lib,
          ...
        }: let
          outPkgs = outPkgsFn pkgs;

          cfg = config.programs.duat;
        in
          with lib; {
            options = with types; {
              programs.duat = {
                enable = mkEnableOption "Duat Terminal Editor";
                package = mkOption {
                  type = package;
                  default = outPkgs.duat;
                };
                config = mkOption {
                  type = path;
                  default = ./config;
                };
              };
            };

            config = mkIf cfg.enable {
              home = {
                packages = [cfg.package];

                file.".config/duat/".source = cfg.config;
              };
            };
          };
      };

      perSystem = {
        pkgs,
        config,
        ...
      }: let
        crateOutputs = config.nci.outputs."duat";
      in {
        nci = {
          toolchainConfig = ./rust-toolchain.toml;

          projects.duat.path = ./.;

          crates = {
            duat = {
              drvConfig = {
                mkDerivation = {
                  buildInputs = [
                    (config.nci.toolchains.mkBuild pkgs)
                  ];
                };
              };
            };
            duat-core = {};
            duat-term = {};
            duat-utils = {};
          };
        };

        devShells.default = crateOutputs.devShell.overrideAttrs (old: {
          packages =
            (old.packages or [])
            ++ [
              # crateOutputs.packages.release
            ];
        });
        packages.default = crateOutputs.packages.release;
      };
    });
}
