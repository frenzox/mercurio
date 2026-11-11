# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercurio-scram-v0.1.0) - 2026-11-11

### Added

- *(scram)* add a shared SCRAM-SHA-256 crate

### Other

- *(server)* drop rand for getrandom and std's RandomState
- share one lint configuration across the workspace
- inherit shared package metadata from [workspace.package]
- declare shared third-party dependencies once for the workspace
