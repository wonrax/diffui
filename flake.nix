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
                # Dynamic Tahoe icon: the actool-compiled asset catalog
                # (Assets.car + the .icns fallback). See `appIconAssets`.
                cp -R ${appIconAssets}/. "$app/Contents/Resources/"
              '';

            meta = {
              description = "Native GUI diff viewer for jj and git";
              homepage = "https://github.com/haiha/diffui";
              mainProgram = "diffui";
              platforms = lib.platforms.unix;
            };
          };

          # The Tahoe app icon. `actool` (ships only with full Xcode) compiles
          # the Icon Composer `macos/diffui.icon` into the layered asset catalog
          # (`Assets.car`) the system needs for the dynamic light/dark/tinted +
          # Liquid Glass treatment — a flat `.icns` can't express it. actool
          # isn't in nixpkgs and reads from `/Applications/Xcode.app`, so this
          # step is deliberately impure (`__noChroot`) and fails loudly if actool
          # is missing. Kept in its own derivation so the Rust build above stays
          # hermetic and cached, and so only this icon step is non-sandboxed.
          #
          # NOTE: the exact asset-catalog layout actool wants for a standalone
          # `.icon` is the one piece that needs confirming on a machine that has
          # Xcode — see `macos/README.md` for what to verify if it errors.
          appIconAssets = pkgs.runCommandLocal "diffui-app-icon" { __noChroot = true; } ''
            if ! /usr/bin/xcrun --find actool >/dev/null 2>&1; then
              echo "diffui: actool not found — the macOS app icon needs full Xcode." >&2
              echo "        Install Xcode, then: sudo xcode-select -s /Applications/Xcode.app" >&2
              exit 1
            fi

            catalog="$PWD/Assets.xcassets"
            mkdir -p "$catalog/AppIcon.icon"
            cp -R ${./macos/diffui.icon}/. "$catalog/AppIcon.icon/"
            printf '{ "info": { "author": "xcode", "version": 1 } }\n' > "$catalog/Contents.json"

            mkdir -p "$out"
            /usr/bin/xcrun actool "$catalog" \
              --compile "$out" \
              --app-icon AppIcon \
              --platform macosx \
              --minimum-deployment-target 11.0 \
              --output-partial-info-plist "$TMPDIR/icon-info.plist" \
              --errors --warnings --notices
          '';

          # macOS bundle metadata. `CFBundleIdentifier` is the stable identity
          # the OS keys permissions/preferences off of — change it if you fork.
          # `CFBundleIconName` names the `AppIcon` entry in the compiled
          # `Assets.car` (see `appIconAssets`); `CFBundleIconFile` is the legacy
          # `.icns` fallback actool also emits for pre-catalog macOS.
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
              <string>AppIcon</string>
              <key>CFBundleIconName</key>
              <string>AppIcon</string>
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
