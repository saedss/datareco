# Releasing DataReco

A release is a version tag on `main`. GitHub builds the Windows executables for it
and attaches them to a draft release, which you look over and publish.

## What a release contains

| File | What it is |
|---|---|
| `DataReco_<version>_x64-setup.exe` | Installer (per user, no administrator rights needed to install), with a Start menu entry and an uninstaller. Fetches the WebView2 runtime if the PC lacks it. |
| `DataReco-<version>-portable.exe` | The desktop app on its own: one file, nothing installed. |
| `datareco-<version>.exe` | The command-line tool: one file. |
| `SHA256SUMS.txt` | Checksums of the three, to check downloads against. |

All three include Microsoft's C runtime (`.cargo/config.toml`), so they need no
Visual C++ redistributable. They run on 64-bit Windows 10 and 11.

## Making a release

1. Set the new version in both places, the same in each:
   - `Cargo.toml`, under `[workspace.package]`
   - `apps/gui/package.json` (and run `npm install` in `apps/gui` to update the lock file)
2. Merge that to `main` through a pull request, as usual.
3. Tag `main` and push the tag:

   ```sh
   git checkout main
   git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```

4. The **Release** workflow builds the executables (about 15 minutes) and makes a
   draft release named "DataReco 0.2.0" with them attached. It stops with an
   error if the tag and the versions don't match.
5. On GitHub, open **Releases**, edit the draft (the notes are made from the pull
   requests since the last release), and publish it.

To build the executables without making a release, run the **Release** workflow
by hand from the Actions tab; they're attached to the run as an artifact.

## Building them locally

```sh
cd apps/gui
npm install
npx tauri build                      # app and installer
cd ../..
cargo build --release -p datareco-cli   # command-line tool
```

The files land in `target/release/datareco-gui.exe`,
`target/release/bundle/nsis/DataReco_<version>_x64-setup.exe` and
`target/release/datareco.exe`. The first `tauri build` downloads the NSIS
installer tools.

## Before the first public release

- **License.** The repository doesn't have one yet (see the README); without one,
  others have no right to use or share the program. The dependencies are under
  permissive licenses (MIT, Apache 2.0 and similar), whose notices should ship with
  the release once a license is chosen.
- **Code signing.** The executables aren't signed, so Windows SmartScreen shows
  "Windows protected your PC" until a download has built up reputation. Signing
  needs a code-signing certificate (or a service such as Azure Trusted Signing);
  the roadmap has it in Phase 11.
