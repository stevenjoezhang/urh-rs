# Releasing / packaging the urh desktop app

Packaging is exercised **locally** (CI only compile-checks the shell; see
`.github/workflows/ci.yml` → `gui` matrix job for the cross-platform
regression gates). Signing requires certificates this project does not own;
unsigned artifacts are the default and every signing step below is opt-in.

## Prerequisites

- Rust stable (MSRV for the workspace: 1.87)
- Node 22 + npm (frontend build)
- Linux: `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev
  librsvg2-dev` (+ `appstream` for AppImage metadata)
- Windows: WebView2 (preinstalled on CI runners; bundled bootstrapper
  otherwise) and the MSVC toolchain
- macOS: Xcode command-line tools (WebKit is a system framework)

## Build a bundle

From `apps/desktop`:

```sh
npm install            # once
npm run tauri build    # frontend build + cargo release build + bundle
```

Per-platform outputs land in `apps/desktop/src-tauri/target/release/bundle/`:

| Platform | Formats | Path |
|---|---|---|
| Linux | deb, rpm, AppImage | `bundle/deb/`, `bundle/rpm/`, `bundle/appimage/` |
| Windows | MSI, NSIS installer | `bundle/msi/`, `bundle/nsis/` |
| macOS | app, dmg | `bundle/macos/`, `bundle/dmg/` |

`bundle.targets` is `"all"` in `tauri.conf.json`; restrict with
`npm run tauri build -- --bundles deb` etc. Version metadata comes from
`tauri.conf.json` (`version`); keep it in sync with the `urh` crate version
when cutting a release.

## Signing (opt-in, requires your own certificates)

- **macOS**: `npm run tauri build -- --signing-identity <identity>` (codesign)
  and set `APPLE_CERTIFICATE` / `APPLE_ID` / `APPLE_PASSWORD` (or an app
  password) for notarization via Tauri's environment-variable flow.
- **Windows**: set `TAURI_SIGNING_PRIVATE_KEY` for updater signatures and
  pass `--windows-certificate-subject`/set `CARGO` env vars pointing at an
  Authenticode certificate for MSI/NSIS signing.
- **Linux**: none (AppImage/deb are unsigned by default; users verify via
  checksums you publish).

No signing keys are stored in this repository.

## Cross-platform regression

CI (`gui` job matrix: ubuntu / windows / macos) compile-checks the Tauri
shell (`cargo check -p urh-desktop`) on all three platforms, and runs the
frontend test suite plus `cargo test -p urh-app` on the ubuntu leg. Bundling
itself is not exercised in CI — run a local `npm run tauri build` per
platform (or on the three CI runner OSes manually) before publishing
release artifacts.

## Checklist before a release

1. `cargo test` green at the workspace root (default members = the `urh`
   crate) and `cargo test -p urh-app` green.
2. `cd apps/desktop && npm test && npm run build` green.
3. `cargo clippy -p urh-app --all-targets` introduces no new warnings.
4. Version bumped in **both** `Cargo.toml` (root package) and
   `apps/desktop/src-tauri/tauri.conf.json`.
5. `npm run tauri build` succeeds on each target platform you ship.
6. Tag `vX.Y.Z` and attach the bundle artifacts to the GitHub release;
   publish the crates.io crate separately (`cargo publish` from a clean
   worktree — see the repo README for the mirror caveat).
