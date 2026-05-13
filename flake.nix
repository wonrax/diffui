{
  description = "Native GUI diff viewer for jj and git";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      perSystem = flake-utils.lib.eachDefaultSystem (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          inherit (pkgs) lib;

          linuxRuntimeLibs = with pkgs; [
            libx11
            libxcb
            libxcursor
            libxkbcommon
            wayland
            vulkan-loader
          ];

          darwinBuildInputs = [ pkgs.apple-sdk_15 ];

          diffui = pkgs.rustPlatform.buildRustPackage {
            pname = "diffui";
            version = "0.1.0";

            src = lib.cleanSourceWith {
              src = ./.;
              filter =
                path: type:
                let
                  base = baseNameOf path;
                in
                base != "target" && base != "result" && base != ".direnv";
            };

            cargoLock = {
              lockFile = ./Cargo.lock;
              # iced is pulled from git in Cargo.toml; let nix fetch by the
              # commits already pinned in Cargo.lock instead of forcing us to
              # maintain an outputHashes table.
              allowBuiltinFetchGit = true;
            };

            nativeBuildInputs = [ pkgs.pkg-config ] ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.makeWrapper ];

            buildInputs =
              lib.optionals pkgs.stdenv.isLinux (linuxRuntimeLibs ++ [ pkgs.fontconfig ])
              ++ lib.optionals pkgs.stdenv.isDarwin darwinBuildInputs;

            doCheck = false;

            postInstall = lib.optionalString pkgs.stdenv.isLinux ''
              wrapProgram $out/bin/diffui \
                --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath linuxRuntimeLibs}"
            '';

            meta = {
              description = "Native GUI diff viewer for jj and git";
              homepage = "https://github.com/haiha/diffui";
              mainProgram = "diffui";
              platforms = lib.platforms.unix;
            };
          };
        in
        {
          packages = {
            default = diffui;
            diffui = diffui;
          };

          apps.default = {
            type = "app";
            program = "${diffui}/bin/diffui";
          };

          devShells.default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                cargo
                clippy
                nixfmt
                rust-analyzer
                rustc
                rustfmt
              ]
              ++ lib.optionals stdenv.isLinux (
                [
                  pkg-config
                  fontconfig
                ]
                ++ linuxRuntimeLibs
              );

            buildInputs = lib.optionals pkgs.stdenv.isDarwin darwinBuildInputs;

            RUST_BACKTRACE = "1";

            shellHook =
              lib.optionalString pkgs.stdenv.isLinux ''
                diffui_runtime_libs="${pkgs.lib.makeLibraryPath linuxRuntimeLibs}"
                existing_ld_library_path="''${LD_LIBRARY_PATH#:}"

                if [ -n "$existing_ld_library_path" ]; then
                  export LD_LIBRARY_PATH="$diffui_runtime_libs:$existing_ld_library_path"
                else
                  export LD_LIBRARY_PATH="$diffui_runtime_libs"
                fi

                unset diffui_runtime_libs existing_ld_library_path
              ''
              + ''
                export ICED_BACKEND="''${ICED_BACKEND:-wgpu}"
                export WGPU_POWER_PREF="''${WGPU_POWER_PREF:-none}"
              '';
          };

          formatter = pkgs.nixfmt;
        }
      );
    in
    perSystem
    // {
      overlays.default = final: prev: {
        diffui = self.packages.${final.system}.default;
      };
    };
}
