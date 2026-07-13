# EveRusthing

EveRusthing is a native Windows file-search application written in Rust. It aims
to reproduce the core behavior of Everything 1.4.1.1032 while producing a
64-bit executable and using only Windows system APIs at the FFI boundary.

The project currently provides a usable Win32 desktop application, fast NTFS
indexing, persistent database caching, live filesystem updates, EFU file-list
searching, and a command-line interface. It is an independent implementation
and is not affiliated with or endorsed by voidtools.

## Current Features

- Native Win32 interface with search, sortable result columns, menus, status
  information, and an Everything-style Options dialog
- Direct NTFS Master File Table indexing
- Optional Windows service for indexing without running the GUI as administrator
- Persistent `EveRusthing.db` cache with atomic writes and integrity checks
- Incremental database refresh through the NTFS USN journal
- Live updates, with `ReadDirectoryChangesW` used when direct USN access is not
  available
- Everything File List (`.efu`) loading and console searching
- Case-sensitive, whole-word, path, wildcard, Boolean, extension, type, parent,
  file-reference, and size filters
- Background result sorting to keep the GUI responsive on large indexes
- High-DPI awareness and Windows common-controls visual styles

EveRusthing is still under development. Full Everything product compatibility,
including the complete command-line surface, INI behavior, database formats,
EFU behavior, and Everything SDK IPC, is not yet implemented.

## Requirements

- 64-bit Windows
- NTFS volumes for local filesystem indexing
- A current stable Rust toolchain with the MSVC target
- Visual Studio Build Tools with the C++ build tools and Windows SDK

Rust 2024 edition support requires Rust 1.85 or newer.

## Building

From a Developer PowerShell prompt using an x64 MSVC Rust toolchain:

```powershell
cargo build --release
```

The executable is written to:

```text
target\release\EveRusthing.exe
```

To build from a different host toolchain, install and select the explicit target
with `rustup target add x86_64-pc-windows-msvc` and
`cargo build --release --target x86_64-pc-windows-msvc`. That form writes the
executable under `target\x86_64-pc-windows-msvc\release` instead.

## Running the GUI

Start EveRusthing without an index argument:

```powershell
.\target\release\EveRusthing.exe
```

Before a first normal-privilege launch, install the service from an elevated
terminal. Place the executable in its final location first because the service
registration stores its absolute path:

```powershell
.\target\release\EveRusthing.exe -install-service
```

The service starts automatically with Windows and exposes its indexing
operations to authenticated local users through a named pipe; remote clients are
rejected. The GUI itself continues to run with the caller's normal privileges.
Alternatively, run the GUI as administrator when building the first index.
Without elevation, an installed service, or an existing valid database,
EveRusthing cannot create the initial local index.

The index is saved as `EveRusthing.db` beside the executable. On later launches,
EveRusthing loads that database and replays USN journal changes when possible.
If privileged refresh is unavailable, a valid cached database remains usable
and live changes are monitored with ordinary filesystem notifications.

GUI preferences are stored in `EveRusthing.ini` beside the executable.

## Command Line

Show the built-in help:

```powershell
.\target\release\EveRusthing.exe -?
```

Search the local NTFS index and print matching paths:

```powershell
.\target\release\EveRusthing.exe -local -search "ext:rs query"
```

Search an Everything File List:

```powershell
.\target\release\EveRusthing.exe -filelist "D:\Lists\files.efu" -search "*.pdf"
```

The filename may also be supplied without `-filelist`:

```powershell
.\target\release\EveRusthing.exe "D:\Lists\files.efu" -s "report"
```

Common options:

| Option | Description |
| --- | --- |
| `-local`, `-l` | Search the local NTFS index |
| `-filelist <file>` | Search an EFU file list |
| `-search <text>`, `-s <text>` | Set the search expression |
| `-case`, `-nocase` | Enable or disable case matching |
| `-matchpath`, `-nomatchpath` | Match against full paths or names |
| `-wholeword`, `-nowholeword` | Enable or disable whole-word matching |
| `-reindex` | Ignore the cached database and rebuild it |
| `-nodb` | Do not load or save `EveRusthing.db` |

`-filelist` and `-local` are mutually exclusive.

## Search Syntax

Whitespace is an implicit AND. Use `|` for OR, `!` for NOT, `<...>` for
grouping, and double quotes for terms containing spaces. OR binds more tightly
than implicit AND, matching Everything 1.4 behavior.

```text
report ext:pdf
ext:rs|ext:toml !path:target
<invoice|receipt> parent:"D:\Accounting"
```

Supported filters include:

| Filter | Meaning |
| --- | --- |
| `case:`, `nocase:` | Override case matching for a term |
| `wholeword:`, `ww:` | Match a complete word |
| `path:`, `name:`, `nopath:` | Select full-path or filename matching |
| `ext:rs;toml` | Match one of several extensions |
| `file:`, `folder:`, `dir:` | Match files or directories, optionally by text |
| `parent:<path>` | Match an exact parent path |
| `frn:<number>` | Match an NTFS file reference number |
| `size:<value>` | Match by byte size; comparisons are supported |

Text containing `*` or `?` is treated as a wildcard pattern. Size expressions
accept `=`, `>`, `>=`, `<`, and `<=`. Supported binary size suffixes are `b`,
`kb`/`k`, `mb`/`m`, `gb`/`g`, and `tb`/`t`.

Initial records obtained from the NTFS MFT do not always include file sizes, so
`size:` is most reliable with EFU data or records enriched by later filesystem
updates.

## Service Management

Service-management commands must be run from an elevated terminal:

```powershell
.\target\release\EveRusthing.exe -install-service
.\target\release\EveRusthing.exe -start-service
.\target\release\EveRusthing.exe -stop-service
.\target\release\EveRusthing.exe -uninstall-service
```

Use `-service-pipe-name <name>` to select a non-default pipe name. The default
is `EveRusthing Service`. A custom name must be supplied both when installing
the service and when starting the GUI or CLI client. To move the executable or
change the registered pipe name, uninstall and reinstall the service.

## Development

Run the standard checks with:

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Optional synthetic benchmarks are available as Cargo examples:

```powershell
cargo run --release --example query_bench -- 500000
cargo run --release --example index_bench
cargo run --release --example sort_bench
```

The main modules are organized as follows:

| Module | Responsibility |
| --- | --- |
| `gui` | Native window, controls, commands, and options dialog |
| `ntfs` | Volume discovery, MFT enumeration, and USN journal access |
| `index` | In-memory file hierarchy and snapshots |
| `query` | Search parsing, compilation, and matching |
| `database` | Persistent index snapshots and incremental checkpoints |
| `monitor` | Runtime index updates |
| `service` | Windows service and named-pipe client/server |
| `efu` | Everything File List parsing and writing |

## License

The crate metadata declares this project under the MIT license.
