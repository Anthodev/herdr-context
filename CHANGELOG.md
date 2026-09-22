# Changelog

## [0.19.7] - 2026-09-23

Restarting Herdr no longer leaves the dock's old shell behind: once the replacement dock is open and reconciled, herdr-context verifies Herdr's restored plain shell is not the live dock and closes it, keeping focus on a surviving terminal. Releases are now published with curated notes from the versioned `CHANGELOG.md`, CI and release workflows run on Avrea runners, and the historical release notes are backfilled.

### Fixes

- fix(dock): replace stale shell after startup restore ([0cd00ebb](https://github.com/Anthodev/herdr-context/commit/0cd00ebb))

**Full changelog**: [v0.19.6...v0.19.7](https://github.com/Anthodev/herdr-context/compare/v0.19.6...v0.19.7)

## [0.19.6] - 2026-09-11

Docks now come back on their own after a Herdr server restart: herdr-context restores the dock layout recorded in `docks.json`, reconciles the restored panes against the fresh session, and refreshes pane IDs without stealing focus — no more reopening your context dock by hand after every restart.

### Features

- feat: restore docks after Herdr server restarts (#60)

**Full changelog**: [v0.19.5...v0.19.6](https://github.com/Anthodev/herdr-context/compare/v0.19.5...v0.19.6)

## [0.19.5] - 2026-08-31

### Fixes

- fix(vcs): ignore structural Jujutsu tree entries (#58)

**Full changelog**: [v0.19.4...v0.19.5](https://github.com/Anthodev/herdr-context/compare/v0.19.4...v0.19.5)

## [0.19.4] - 2026-08-28

### Features

- feat(conversations): show Claude titles (#56)

**Full changelog**: [v0.19.3...v0.19.4](https://github.com/Anthodev/herdr-context/compare/v0.19.3...v0.19.4)

## [0.19.3] - 2026-08-28

### Fixes

- fix(conversations): accept current Claude metadata (#54)

**Full changelog**: [v0.19.2...v0.19.3](https://github.com/Anthodev/herdr-context/compare/v0.19.2...v0.19.3)

## [0.19.2] - 2026-08-25

### Fixes

- fix(context): consistent descendant VCS status and Claude session detection (#52)

**Full changelog**: [v0.19.1...v0.19.2](https://github.com/Anthodev/herdr-context/compare/v0.19.1...v0.19.2)

## [0.19.1] - 2026-08-24

### Fixes

- fix(context): correct Files metadata and history discovery (#50)

**Full changelog**: [v0.19.0...v0.19.1](https://github.com/Anthodev/herdr-context/compare/v0.19.0...v0.19.1)

## [0.19.0] - 2026-08-22

### Features

- feat(ui): redesign files tree presentation (#48)

**Full changelog**: [v0.18.0...v0.19.0](https://github.com/Anthodev/herdr-context/compare/v0.18.0...v0.19.0)

## [0.18.0] - 2026-08-22

### Changes

- chore(docs): updated README ([8ef506e](https://github.com/Anthodev/herdr-context/commit/8ef506e))
