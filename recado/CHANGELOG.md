# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.3](https://github.com/frenzox/mercurio/compare/recado-v0.2.2...recado-v0.2.3) - 2026-11-09

### Fixed

- *(recado)* stop quietly when the output pipe closes

### Other

- match package files by glob instead of naming a version

## [0.2.2](https://github.com/frenzox/mercurio/compare/recado-v0.2.1...recado-v0.2.2) - 2026-11-09

### Added

- *(recado)* describe the contents of deb and rpm packages

### Other

- explain installing recado from the packages

## [0.2.1](https://github.com/frenzox/mercurio/compare/recado-v0.2.0...recado-v0.2.1) - 2026-11-09

### Other

- updated the following local packages: mercurio-client

## [0.2.0](https://github.com/frenzox/mercurio/compare/recado-v0.1.1...recado-v0.2.0) - 2026-11-08

### Other

- declare Rust 1.89 as the minimum supported version
- [**breaking**] mark the enums that may grow as non-exhaustive

## [0.1.1](https://github.com/frenzox/mercurio/compare/recado-v0.1.0...recado-v0.1.1) - 2026-11-08

### Other

- add a changelog to every crate

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/recado-v0.1.0) - 2026-11-08

### Added

- *(cli)* expose sessions, message properties and reconnection
- *(client)* reconnect and resume the session after losing the broker
- *(client)* expose MQTT 5 session, will, publish and subscribe options
- *(client)* add TLS support using rustls
- *(cli)* add will message options to subscriber command
- add mercuriod daemon and reorganize CLI tools

### Other

- link to the documentation on GitHub Pages
- give every crate a README for its crates.io page
- add the metadata crates.io needs to publish each crate
- rename the command-line client to recado
- clear all clippy warnings
