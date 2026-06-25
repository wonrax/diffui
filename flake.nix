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

          version = "0.1.0";

          # `build.rs` fetches the Lucide icon font over the network, which the
          # Nix build sandbox forbids. Fetch it here instead as a fixed-output
          # derivation (the one thing allowed network access, since its hash is
          # pinned up front) and feed the build the local file through the
          # `LUCIDE_TTF_PATH` escape hatch build.rs already honors. Keep the url
          # and hash in lockstep with the LUCIDE_* constants in build.rs — the
          # hash is the SRI form of LUCIDE_SHA256.
          lucideTtf = pkgs.fetchurl {
            url = "https://cdn.jsdelivr.net/npm/lucide-static@1.21.0/font/lucide.ttf";
            hash = "sha256-681MVdcC81+rEC+MNLwYwjOOFzaEcO1YqsSfj7Kl1HY=";
          };

          diffui = pkgs.rustPlatform.buildRustPackage {
            pname = "diffui";
            inherit version;

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

            # Point build.rs at the sandbox-fetched font instead of the network.
            LUCIDE_TTF_PATH = lucideTtf;

            # Linux: wrap for the runtime libs. macOS: assemble a real
            # `Diffui.app` around the binary and keep `bin/diffui` as a symlink
            # into it, so this one package serves the GUI (Spotlight/Dock), the
            # CLI, and `nix run` off a single binary — the convention alacritty /
            # kitty / wezterm follow.
            postInstall =
              lib.optionalString pkgs.stdenv.isLinux ''
                wrapProgram $out/bin/diffui \
                  --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath linuxRuntimeLibs}"
              ''
              + lib.optionalString pkgs.stdenv.isDarwin ''
                app="$out/Applications/Diffui.app"
                mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
                cp ${infoPlist} "$app/Contents/Info.plist"
                mv "$out/bin/diffui" "$app/Contents/MacOS/diffui"
                ln -s "../Applications/Diffui.app/Contents/MacOS/diffui" "$out/bin/diffui"
                ${lib.optionalString (builtins.pathExists ./macos/diffui.icns) ''
                  cp ${./macos/diffui.icns} "$app/Contents/Resources/diffui.icns"
                ''}
              '';

            meta = {
              description = "Native GUI diff viewer for jj and git";
              homepage = "https://github.com/haiha/diffui";
              mainProgram = "diffui";
              platforms = lib.platforms.unix;
            };
          };

          # macOS bundle metadata. `CFBundleIdentifier` is the stable identity
          # the OS keys permissions/preferences off of — change it if you fork.
          # `CFBundleIconFile` points at Resources/diffui.icns, which is copied
          # in only if you drop a `macos/diffui.icns` into the repo (the bundle
          # builds fine without one — macOS just shows a generic icon).
          infoPlist = pkgs.writeText "diffui-Info.plist" ''
            <?xml version="1.0" encoding="UTF-8"?>
            <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
            <plist version="1.0">
            <dict>
              <key>CFBundleDevelopmentRegion</key>
              <string>en</string>
              <key>CFBundleDisplayName</key>
              <string>Diffui</string>
              <key>CFBundleExecutable</key>
              <string>diffui</string>
              <key>CFBundleIconFile</key>
              <string>diffui</string>
              <key>CFBundleIdentifier</key>
              <string>com.haiha.diffui</string>
              <key>CFBundleInfoDictionaryVersion</key>
              <string>6.0</string>
              <key>CFBundleName</key>
              <string>Diffui</string>
              <key>CFBundlePackageType</key>
              <string>APPL</string>
              <key>CFBundleShortVersionString</key>
              <string>${version}</string>
              <key>CFBundleVersion</key>
              <string>${version}</string>
              <key>LSApplicationCategoryType</key>
              <string>public.app-category.developer-tools</string>
              <key>LSMinimumSystemVersion</key>
              <string>11.0</string>
              <key>NSHighResolutionCapable</key>
              <true/>
              <key>NSPrincipalClass</key>
              <string>NSApplication</string>
              <key>NSSupportsAutomaticGraphicsSwitching</key>
              <true/>
            </dict>
            </plist>
          '';
        in
        {
          # One package: on macOS it ships `Applications/Diffui.app` *and*
          # `bin/diffui`; on Linux just the wrapped binary. So `home.packages =
          # [ diffui ]` gets you both the Spotlight-launchable app and the CLI.
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
