# Verification record for version 1.0.0

Every acceptance criterion from the specification (section 13), with what was
actually checked, how, and what is still open. "Automated" means a test or the
benchmark covers it and runs in CI.

Test machine: Windows 11 (build 26300), 12 hardware threads, NVMe SSD, NTFS
system drive with about 180 GB and roughly 770,000 files and folders in use.
Rust 1.100, Node 26. App tests used the release build in portable mode.

## Measurements on the real system drive

| What | Result | Target |
|------|--------|--------|
| Search, end to end from the window (typical queries) | 5 to 10 ms | < 50 ms |
| Search, one letter (474,000 matches) | 29 ms | < 50 ms |
| `*.dll` wildcard / regex | 29 ms / 19 ms | < 50 ms / < 200 ms |
| Standard mode, full walk of C: (768,833 entries, warm cache) | 5.9 s | < 6 min |
| Fast mode, names of ~727,000 entries searchable | a few seconds after the UAC prompt | < 60 s |
| New file searchable (fast mode, USN journal) | ~0.33 s | < 1 s |
| Moved + renamed file searchable (fast mode) | ~0.45 s | < 1 s |
| Launch to visible window, from snapshot (logged by the app, 4 runs) | 0.53 to 0.56 s | < 1.5 s |
| Snapshot load (768,850 entries, in parallel with the window) | 0.23 to 0.24 s | |
| Snapshot save on exit | 0.12 to 0.5 s, 78 to 81 MB | < 3 s, < 150 MB |
| Idle CPU over 60 s with 731k entries | 0.06 % of the machine (0.75 % of one core) | < 0.5 % |
| Private memory with 731k entries | 196 MB (helper: 3 MB) | < 300 MB at 1M |

## Acceptance criteria

| # | Criterion | Status | Evidence |
|---|-----------|--------|----------|
| 1 | Installer and portable exe run on clean Windows 10 and 11 without admin or extra runtime | **Partly verified** | The installer is per-user (`installMode: currentUser`, no elevation) and the exe is self-contained (12 MB). Built and run on Windows 11. Not yet run on a clean Windows 10 22H2 VM. WebView2 ships with Windows 11 and updated Windows 10; the installer bootstraps it if missing. |
| 2 | First results appear while the index is still building | **Verified** | Release build on C: in both modes: results listed while the status bar showed "Indexing C:" and "Partial results". In fast mode the names appear together once the file table has been read (seconds). |
| 3 | 1,000,000-file benchmark meets section 7 | **Verified (automated)** | `cargo bench -p findit-core --bench search`: substring p95 38 ms, regex p95 33 ms, path p95 40 ms, snapshot load 0.45 s, save 0.18 s, 81 MB, 90 MB index memory. |
| 4 | Create, rename, move, delete reflected within 1 s | **Verified (automated + manual)** | `crates/findit-indexing/tests/scan_watch.rs` (standard mode, real files); fast mode measured at 0.33 s and 0.45 s on C:. |
| 5 | Cyrillic, Greek, CJK and emoji match case-insensitively | **Verified (automated + manual)** | Query tests; integration test; a file named `finditqa-Документ-Έγγραφο-報告-🎉.txt` was found on C: by `finditqa`, `ДОКУМЕНТ` and `報告`. |
| 6 | Every operator has a passing test | **Verified (automated)** | 60+ query tests in `crates/findit-core/src/query.rs`. |
| 7 | Paths longer than 260 characters open correctly | **Partly verified** | Indexing a 400+ character path is tested. Opening uses the 8.3 short path or the `\\?\` form; Explorer itself cannot open every long path. |
| 8 | OneDrive placeholders listed without download | **By design, not field-tested** | Folders are read with `FindFirstFileExW` / `FileIdBothDirectoryInfo`, which never open files; preview refuses files with recall attributes. Not tested against a live OneDrive account. |
| 9 | Fast mode shows UAC, helper accepts only the current user, declining falls back | **Verified** | UAC prompt shown and accepted in two manual runs; enumeration, size/date fill and journal updates worked. Pipe security tested (`pipe_accepts_only_the_expected_client`). The helper exited with the app. The decline path is the `LaunchError::Declined` branch (falls back to standard mode). |
| 10 | No network traffic except the update check | **Verified by design** | The only network code is the `fetch` behind Settings → About → Check for updates; the CSP allows only `api.github.com`. |
| 11 | Delete goes to the Recycle Bin after confirmation | **Verified** | Confirmation dialog in the UI; a test file was moved to the Recycle Bin (seen in the shell's Recycle Bin listing). Rename refuses invalid names and never overwrites an existing file. |
| 12 | Keyboard only, screen-reader names, 150%/200% DPI, light and dark | **Partly verified** | All actions have shortcuts; the grid uses ARIA grid roles. Light, dark, high contrast and 200% scale checked by emulation in the real WebView2. Not yet checked with Narrator. |
| 13 | Uninstall removes the app and asks about index and settings | **By design** | The NSIS uninstaller's "Delete the application data" checkbox removes `%LOCALAPPDATA%\FindIt` (hook in `src-tauri/windows/hooks.nsh`, verified to compile into the installer). |
| 14 | After 1 hour idle: CPU < 0.5%, RAM < 300 MB | **Partly verified** | Measured over 60 s, not 1 hour: 0.06 % CPU, 196 MB. Idle work is event driven. |
| 15 | README, LICENSE, CHANGELOG, build.ps1, passing CI | **Verified** | In the repository; `build.ps1` produced the release files locally; CI runs on every push. |

## Bugs found during this verification and fixed

- Choosing fast mode in the welcome wizard also started a standard walk of the same
  drive; the two raced. Each drive now has its own scan control and fast mode
  cancels the walk.
- In fast mode, files that are hard links (for example `C:\Windows\System32\cmd.exe`)
  were listed only under one folder. Extra names are now added from the folder listings.
- A `settings.json` saved with a UTF-8 byte order mark was rejected. It is accepted now.
- The window waited for the first search before appearing; it now appears first
  (about 0.55 s after start).
- The "shortcut already in use" warning at startup was sent before the window
  listened for it. It is now shown when the window opens and in Settings.
- During heavy indexing, the live refresh could cancel a search that was still
  running. Refreshes now wait for the running search.
