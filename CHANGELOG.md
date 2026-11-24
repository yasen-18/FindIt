# Changelog

All notable changes to FindIt are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [1.0.0] - 2026-11-24

First release.

### Added

- Search-as-you-type over all local drives, with results ranked by exact name,
  prefix, word start, then anywhere (ties: shorter path, then newer).
- Query language: AND (space), OR (`|`), NOT (`!`), grouping (`< >`), exact
  phrases, `*` and `?` wildcards, `regex:`, `path:`, `ext:`, `size:`, `dm:`,
  `type:`, `file:`, `folder:`, `case:`, `word:` and `diacritics:`.
- Unicode-correct matching: case-insensitive for Latin, Cyrillic, Greek and other
  scripts, composed and decomposed accents treated as equal, accents ignored by
  default.
- Standard mode (no administrator rights): parallel folder walk with
  `FindFirstFileExW` and live updates with `ReadDirectoryChangesW`.
- Fast mode (optional, asks for administrator permission): NTFS master file table
  enumeration and the USN change journal, in a locked-down helper process.
- Snapshot of the index on disk (checksummed, written atomically) for a fast start.
- Virtualized results grid with sortable, resizable and reorderable columns,
  highlighted matches, shell icons and file type names.
- Filter chips, search history, preview pane for images and text.
- Open, Open with, Open containing folder, Copy, Copy path, Copy name, Rename,
  Delete to the Recycle Bin, Properties.
- Tray icon, global hotkey (Ctrl+Alt+Space by default), single instance, optional
  start with Windows.
- Light and dark themes following Windows, accent color, Mica on Windows 11,
  high-contrast support, full keyboard use and screen-reader names.
- First-run wizard, settings, manual update check.
- Portable mode (`portable.flag`), per-user installer, `build.ps1`, CI on GitHub
  Actions.
