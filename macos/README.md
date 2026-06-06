# macOS app icon

`diffui.icon` is the [Icon Composer](https://developer.apple.com/icon-composer/)
source for the app icon. It carries the layered artwork plus the per-appearance
(light / dark / tinted / clear) and Liquid Glass material info that macOS Tahoe
renders dynamically.

## How it gets into the app

Two separate paths, by design:

- **Packaged app (`nix build`).** The flake compiles `diffui.icon` with Apple's
  `actool` into a layered asset catalog (`Assets.car`) inside
  `Diffui.app/Contents/Resources`, and `Info.plist` points at it via
  `CFBundleIconName = AppIcon`. This is the only way to get the *dynamic*
  light/dark/tinted + Liquid Glass icon — a flat `.icns` can't express it.
  `actool` ships only with **full Xcode**, so the `diffui-app-icon` derivation
  is intentionally impure (`__noChroot`) and **hard-fails if Xcode/actool is
  missing**. Install Xcode and `sudo xcode-select -s /Applications/Xcode.app`.

- **Unbundled run (`cargo run`, bare `target/debug/diffui`).** No `.app` around
  the binary, so the dock would show the generic terminal icon. `chrome.rs`
  sets a *static* dock icon at runtime from the embedded `../assets/icon.png`,
  but only when not inside a bundle (so it never flattens the dynamic packaged
  icon). `assets/icon.png` is a 512px padded render of the macOS appearance.

## Regenerating `assets/icon.png` (the dev/unbundled icon)

`assets/icon.png` is a static fallback rendered from `diffui.icon`. Icon
Composer's CLI (`ictool`) renders the full-bleed art, so we add the ~20% macOS
gutter ourselves (the dock shows this image as-is, with no system padding):

```sh
ICT="/Applications/Icon Composer.app/Contents/Executables/ictool"
"$ICT" macos/diffui.icon --export-image --output-file /tmp/icon-full.png \
  --platform macOS --rendition Default --width 1024 --height 1024 --scale 1
# then composite /tmp/icon-full.png at ~0.805 scale, centered, onto a
# transparent 1024 canvas and downscale to 512 -> assets/icon.png
# (any compositor works; we used a small Swift/CoreGraphics helper).
```

## To verify once Xcode is installed

The flake's `appIconAssets` step builds a minimal `Assets.xcassets` containing
`AppIcon.icon` and runs `actool --app-icon AppIcon --platform macosx`. The piece
that needs confirming on a real Xcode box is whether `actool` accepts the
standalone `.icon` laid out that way (vs. needing a different catalog wrapper).
If `nix build` errors in `diffui-app-icon`, check the actool message and adjust
the catalog layout in `flake.nix` accordingly. Everything downstream
(`Assets.car` copied to Resources, `CFBundleIconName = AppIcon`) is standard.
