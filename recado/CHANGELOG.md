# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
