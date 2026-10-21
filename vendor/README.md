# Vendored crates

## winit 0.30.13

Unmodified winit 0.30.13 from crates.io except for two macOS changes, applied
through `[patch.crates-io]` in the workspace `Cargo.toml`:

- `src/platform_impl/macos/ffi.rs`: the private `CGSMainConnectionID` and
  `CGSSetWindowBackgroundBlurRadius` declarations are removed.
- `src/platform_impl/macos/window_delegate.rs`: `set_blur` is a no-op.

Mac App Store review rejects binaries that reference private APIs, and NavDB
Studio never blurs its window. When updating winit, copy the new release here
and reapply both changes (`grep -rn CGS src/platform_impl/macos` must not find
`CGSSet…`/`CGSMain…`).
