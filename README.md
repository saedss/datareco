# DataReco

A disk editor and data recovery tool: browse and edit raw disk sectors, find lost
partitions, and recover deleted or lost files from NTFS, FAT, exFAT and more.

> **Status: early development (Phase 0).** The project skeleton is in place; disk
> access arrives in Phase 1. See the [roadmap](docs/ROADMAP.md).

## Safety first

- **Read-only by default.** DataReco never writes to the disk you are recovering from
  unless you turn on write mode and confirm.
- **Recover to a different disk.** Saving recovered files to the source disk can
  overwrite the very data you are trying to get back, so DataReco blocks it.

## Building

DataReco targets Windows for now; Linux and macOS support comes in Phase 10.

Prerequisites:

- [Rust](https://rustup.rs) (stable)
- [Node.js](https://nodejs.org) 22 or newer
- Visual Studio Build Tools with the "Desktop development with C++" workload
- The WebView2 runtime (built into Windows 10 and 11)

Engine and command-line tool:

```sh
cargo build
cargo test
cargo run -p datareco-cli -- --help
```

Desktop app (from `apps/gui`):

```sh
npm install
npm run tauri dev      # run with live reload
npm run tauri build    # release build: target/release/datareco-gui(.exe)
```

Plain `cargo build` at the repository root skips the desktop app, because it
needs the frontend built first. Use the `npm run tauri` commands above for it.

## Testing

Unit and integration tests run with `cargo test`. Recovery features are tested
against generated disk images with known contents; see
[testdata/README.md](testdata/README.md) for how to build them.

## Repository layout

| Path | What's there |
|---|---|
| `crates/` | The engine: disk access, partitions, file systems, scanning, imaging, RAID, templates |
| `apps/cli/` | `datareco` command-line tool |
| `apps/gui/` | Desktop app (Tauri + React) |
| `testdata/` | Test disk-image generator |
| `docs/ROADMAP.md` | Phases and what each one delivers |

## License

Not chosen yet.
