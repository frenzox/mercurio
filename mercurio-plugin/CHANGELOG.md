# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/frenzox/mercurio/compare/mercurio-plugin-v0.2.0...mercurio-plugin-v0.3.0) - 2026-11-11

### Added

- *(auth)* [**breaking**] let a failed authentication tell the client why
- *(plugin)* [**breaking**] name the mount point of an expired session in WIT 0.2
- *(plugin)* [**breaking**] hand plugins their settings as a Config with get and require
- *(plugin)* [**breaking**] move the WebAssembly plugin interface to 0.2

### Fixed

- *(server)* [**breaking**] decide $CONTROL publishes with a hook of their own

### Other

- Merge branch 'chore/workspace-manifests'
- share one lint configuration across the workspace
- inherit shared package metadata from [workspace.package]
- *(plugin)* say which SDK releases speak which interface
- *(plugin)* describe interface 0.2 and what changes for 0.1 plugins

## [0.2.0](https://github.com/frenzox/mercurio/compare/mercurio-plugin-v0.1.1...mercurio-plugin-v0.2.0) - 2026-11-10

### Added

- *(server)* [**breaking**] answer $CONTROL requests only to the client that sent them

## [0.1.1](https://github.com/frenzox/mercurio/compare/mercurio-plugin-v0.1.0...mercurio-plugin-v0.1.1) - 2026-11-09

### Other

- *(plugin)* add a changelog for mercurio-plugin

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercurio-plugin-v0.1.0) - 2026-11-08

### Added

- First release: the SDK for writing mercuriod WASM plugins, with the `Plugin` trait and the `export_plugin!` macro
