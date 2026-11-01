# Contributing to unimouse

Thanks for helping. Bug reports, mouse reports, docs fixes and code are all welcome.

## Reporting a problem

Open an issue using the bug or mouse template. The most useful things to include:

- your OS and version, and your mouse model
- what the **Activity** tab in the settings app shows when you press the button
- the relevant part of your `config.toml`

## Building

You need [Rust](https://rustup.rs) 1.89 or newer.

```sh
cargo build
cargo test
cargo run -- --settings      # run it and open the settings app
```

On macOS, give your terminal Accessibility access to run it from source.

### Working on the settings app without a mouse

The settings app is one file, `src/ui.html`, served by `src/ui.rs`. To work on it without permissions or a real
mouse, run the preview. It serves the page with a throwaway config and pretends a button is pressed every few
seconds:

```sh
cargo test preview -- --ignored --nocapture
```

Open the printed URL. Restart the preview after editing `ui.html` (it's compiled into the binary).

### Checking the other platforms

CI builds and tests on macOS, Windows and Linux. To type-check another platform locally:

```sh
rustup target add x86_64-pc-windows-msvc x86_64-unknown-linux-gnu
cargo clippy --target x86_64-pc-windows-msvc
cargo clippy --target x86_64-unknown-linux-gnu
```

## Before you open a pull request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- Keep changes focused: one fix or feature per pull request.
- Add or update a test when you change behaviour in `engine.rs`, `scroll.rs` or `ui.rs`.
- Input code can't be fully tested in CI. In the pull request, say which OS and mouse you tried it on.
- Update `CHANGELOG.md` under *Unreleased* for anything users will notice.

## How the code is organised

| File | What it does |
|---|---|
| `src/engine.rs` | Config parsing, per-app profiles, gesture detection. Platform-independent |
| `src/scroll.rs` | Smooth-scroll animation. Platform-independent |
| `src/ui.rs`, `src/ui.html` | The settings app and its local API |
| `src/install.rs` | Start at login |
| `src/mac.rs`, `src/win.rs`, `src/linux.rs` | Reading input and sending output on each OS |

Platform files feed raw button, move and scroll events into the `Engine` and perform the actions it returns.
Anything that isn't OS-specific belongs in the engine, where it can be tested.

## Easy places to start

- **Add a ready-made app setup**: add an entry to `PACKS` in `src/ui.html`.
- **Add a mouse drawing**: add a layout to `LAYOUTS` in `src/ui.html`, and a name rule to `guessLayout`.
- **Add a conflicting app to the setup check**: add its process name to `CONFLICTS` in `src/ui.rs`.
- Anything on the roadmap in the README.

## License

By contributing, you agree that your contributions are dual licensed under MIT OR Apache-2.0, like the rest of
the project.
