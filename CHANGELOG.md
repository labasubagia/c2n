# Changelog

All notable changes to this project will be documented in this file.

## [0.1.2] - 2026-10-29

### Changed

- `img clear` now removes only downloaded images under `output/images/`, preserving the database, its WAL/SHM/journal files, `.gitkeep`, and other output entries.
- `img clear` and `db restore` are now dry runs by default; pass `--yes` to apply.

### Fixed

- Hardened image handling: paths are resolved under the data directory, `..` and symlink escapes are rejected, and PNG prompt writes are atomic (temp-file replace, fsync before rename).
- Notion page creation now reconciles ambiguous failures by querying the database for the created page instead of surfacing a spurious error.
- Notion data-source duplicate queries now paginate through all results rather than reading only the first page.
