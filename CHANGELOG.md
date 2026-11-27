# Changelog

All notable changes to this project are listed here ([Keep a Changelog](https://keepachangelog.com), [SemVer](https://semver.org)).

## [Unreleased]

- Gamepads: controllers, joysticks and wheels follow the keyboard and mouse to the machine you drive, with rumble played back on the real pad. Sticks and triggers rest when you leave; their position carries over when you cross.
- Protocol version 3: update every machine together.
- Fix touchpads becoming unresponsive after a handover by restoring per-finger tool type and other touch attributes, including inactive slots.
- Remote view: `edgehop view` (bind it to a key) shows the screen of the machine you drive, full screen, following its cursor across monitors. It opens on every crossing while on, over a separate encrypted video link (`[view]` config).
- Protocol version 2: update every machine together.

## [0.1.0] - 2026-11-10

- First release.
