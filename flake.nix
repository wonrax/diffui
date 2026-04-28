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
        inherit (pkgs) lib;

        linuxRuntimeLibs = with pkgs; [
          libx11
          libxcb
          libxcursor
          libxkbcommon
          wayland
          vulkan-loader
        ];

        runtimeFonts = with pkgs; [
          fontconfig
          cascadia-code
        ];

        darwinBuildInputs = [ pkgs.apple-sdk_15 ];
      in
      {
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
              ]
              ++ runtimeFonts
              ++ linuxRuntimeLibs
            );

          buildInputs = lib.optionals pkgs.stdenv.isDarwin darwinBuildInputs;

          RUST_BACKTRACE = "1";

          shellHook =
            lib.optionalString pkgs.stdenv.isLinux ''
              diffui_runtime_libs="${pkgs.lib.makeLibraryPath linuxRuntimeLibs}"
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

              unset diffui_runtime_libs existing_ld_library_path existing_xdg_data_dirs
            ''
            + ''
              export ICED_BACKEND="''${ICED_BACKEND:-wgpu}"
              export WGPU_POWER_PREF="''${WGPU_POWER_PREF:-none}"
            '';
        };

        formatter = pkgs.nixfmt;
      }
    );
}
