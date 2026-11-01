# Changelog

All notable changes to unimouse are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0]

First release.

### Added

- Button remapping on macOS, Windows and Linux from one config file, with `mod` meaning ⌘ on macOS and Ctrl
  elsewhere.
- Gestures: long press, hold and drag in four directions, hold and scroll.
- Actions: back/forward, desktops and overviews, media keys, keyboard shortcuts, `open:` and `run:`.
- Per-app settings matched by app name, exe name or window class, with `|` aliases and `*` for versioned names.
- Smooth scrolling with flick acceleration, adjustable speed and reverse direction for mouse wheels only.
- Settings app (`unimouse --settings`): a drawing of your mouse with every button labelled, press-to-select,
  action palette with shortcut recorder, ready-made app setups, Practice mode, live activity, scroll test pad,
  undo and a setup check for missing permissions and conflicting mouse software.
- Start at login (`--install` / `--uninstall`) on all three platforms.
- Live config reload and single-instance protection.
