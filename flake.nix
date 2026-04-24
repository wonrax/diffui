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
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };

        icedComet = pkgs.rustPlatform.buildRustPackage {
          pname = "iced_comet";
          version = "0.14.0-3f75f32";

          src = pkgs.fetchFromGitHub {
            owner = "iced-rs";
            repo = "comet";
            rev = "3f75f3240edc1719df584810337bc7df010327d8";
            hash = "sha256-lwo0O8aivR4PqRZxFiiWEX7l6gNXsD0Oibn7s45XY+8=";
          };

          cargoHash = "sha256-UGCLJwCyLH5/QjvnI/HQtR04cEaenz167e78LtwSzsQ=";

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];
        };

        runtimeLibs = with pkgs; [
          libx11
          libxcb
          libxcursor
          libxkbcommon
          wayland
          vulkan-loader
        ];

        runtimeFonts = with pkgs; [
          dejavu_fonts
          fontconfig
          noto-fonts
          noto-fonts-color-emoji
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          packages =
            with pkgs;
            [
              cargo
              cargo-flamegraph
              clippy
              icedComet
              nixfmt
              perf
              pkg-config
              rust-analyzer
              rustc
              rustfmt
            ]
            ++ runtimeFonts
            ++ runtimeLibs;

          RUST_BACKTRACE = "1";

          shellHook = ''
            diffui_runtime_libs="${pkgs.lib.makeLibraryPath runtimeLibs}"
            existing_ld_library_path="''${LD_LIBRARY_PATH#:}"
            existing_xdg_data_dirs="''${XDG_DATA_DIRS#:}"

            if [ -n "$existing_ld_library_path" ]; then
              export LD_LIBRARY_PATH="$diffui_runtime_libs:$existing_ld_library_path"
            else
              export LD_LIBRARY_PATH="$diffui_runtime_libs"
            fi

            if [ -n "$existing_xdg_data_dirs" ]; then
              export XDG_DATA_DIRS="${pkgs.lib.makeSearchPath "share" runtimeFonts}:$existing_xdg_data_dirs"
            else
              export XDG_DATA_DIRS="${pkgs.lib.makeSearchPath "share" runtimeFonts}"
            fi

            export FONTCONFIG_FILE="${pkgs.fontconfig.out}/etc/fonts/fonts.conf"
            export FONTCONFIG_PATH="${pkgs.fontconfig.out}/etc/fonts"
            export ICED_BACKEND="''${ICED_BACKEND:-wgpu}"
            export WGPU_POWER_PREF="''${WGPU_POWER_PREF:-none}"

            unset diffui_runtime_libs existing_ld_library_path existing_xdg_data_dirs
          '';
        };

        formatter = pkgs.nixfmt;
      }
    );
}
