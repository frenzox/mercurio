# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.2](https://github.com/frenzox/mercurio/compare/recado-v0.3.1...recado-v0.3.2) - 2026-11-13

### Other

- updated the following local packages: mercurio-client

## [0.3.1](https://github.com/frenzox/mercurio/compare/recado-v0.3.0...recado-v0.3.1) - 2026-11-11

### Other

- updated the following local packages: mercurio-client

## [0.3.0](https://github.com/frenzox/mercurio/compare/recado-v0.2.4...recado-v0.3.0) - 2026-11-11

### Added

- *(server)* [**breaking**] start brokers through Server::builder
- *(cli)* log in with SCRAM-SHA-256 in recado and mercurioctl
- *(client)* make subscriptions again after a reconnect without session
- *(cli)* add --ws and --ws-path to the shared connect arguments
- *(client)* trust the OS certificate store and take a rustls config
- *(client)* [**breaking**] mark public structs and struct-like variants non_exhaustive
- *(cli)* print shell completion scripts with a completions command
- *(cli)* log in with a client certificate using --cert and --key
- *(recado)* give pub the will options sub has
- *(recado)* publish one message per line of standard input with pub -l
- *(recado)* add sub -C/--count and -W/--timeout for scripts
- *(recado)* take correlation data as hex with --correlation-data-hex
- *(recado)* print messages as JSON lines with sub --format json
- *(cli)* read the login password from a variable or a prompt in both tools

### Fixed

- *(recado)* report why the client gave up reconnecting
- *(recado)* [**breaking**] refuse -c without -i for every protocol version
- *(recado)* report arguments the client refuses as usage errors
- *(recado)* make up a client id where MQTT 3 refuses an empty one
- *(recado)* disconnect cleanly on SIGTERM in sub
- *(recado)* publish and print binary payloads byte for byte
- *(recado)* [**breaking**] refuse unknown --protocol-version values
- *(recado)* [**breaking**] fail on refused subscriptions and lost connections
- *(recado)* send log output to standard error
- *(client)* [**breaking**] turn TLS on when a CA or client certificate is set

### Other

- share one lint configuration across the workspace
- inherit shared package metadata from [workspace.package]
- declare shared third-party dependencies once for the workspace
- drop the unused tracing dependency from recado
- move recado to base64 0.23
- Merge branch 'master' into refactor/codec-api
- describe SCRAM-SHA-256 logins
- *(core)* [**breaking**] replace QoS and RetainHandling Invalid with TryFrom
- *(recado,mercurioctl)* drop direct mercurio-core and bytes dependencies
- *(recado)* give pub and sub their own modules and argument structs

## [0.2.4](https://github.com/frenzox/mercurio/compare/recado-v0.2.3...recado-v0.2.4) - 2026-11-10

### Added

- *(recado)* connect through Unix sockets with --unix

### Other

- point the READMEs at mercurioctl and the admin guide

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
