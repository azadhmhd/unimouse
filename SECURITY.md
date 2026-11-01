# Security

## Reporting a vulnerability

Please report security problems privately through GitHub: open the repository's **Security** tab and choose
**Report a vulnerability**. Don't open a public issue for them.

## What unimouse can do on your computer

unimouse needs powerful access to work, so it's worth knowing exactly what it does:

- **It sees all mouse input.** On macOS through an Accessibility event tap, on Windows through a low-level mouse
  hook, on Linux by reading `/dev/input` and writing `/dev/uinput`. It doesn't read the keyboard, except that on
  macOS and Windows it checks whether a modifier key is held while you scroll.
- **It sends keyboard, mouse and scroll events** for the actions you configure.
- **It runs commands you configure.** `run:` actions run with your user's permissions. Only put commands you trust
  in your config, and don't use a config file from someone you don't trust.
- **It makes no network connections.** The only socket is the settings app's local server.

## The settings app

The settings app can change your config, including `run:` commands, so it's locked down:

- it listens only on `127.0.0.1` (port 47613), never on your network
- every API request needs a random token that changes each time unimouse starts; the token is only in the URL
  that `unimouse --settings` opens, and in a file only your user can read
- requests must carry the exact `Host` header, which blocks DNS-rebinding attacks from websites
- the page can only open a fixed list of places (Accessibility settings, the config folder), not arbitrary URLs
  or paths

Tests for these guards are in `src/ui.rs`.
