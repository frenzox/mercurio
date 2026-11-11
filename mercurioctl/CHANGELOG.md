# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/frenzox/mercurio/compare/mercurioctl-v0.2.0...mercurioctl-v0.2.1) - 2026-11-11

### Other

- updated the following local packages: mercurio-client

## [0.2.0](https://github.com/frenzox/mercurio/compare/mercurioctl-v0.1.0...mercurioctl-v0.2.0) - 2026-11-11

### Added

- *(server)* [**breaking**] start brokers through Server::builder
- *(admin)* filter listClients by mount point
- *(mercurioctl)* name the mount point of a client
- *(server)* report the plugin interface a WASM plugin was built for
- *(mercuriod)* configure bridge sessions, remote state and counters
- *(client)* add a request/response helper and use it in mercurioctl
- *(dynsec)* add modifyClient, modifyGroup and modifyRole
- *(dynsec)* [**breaking**] check roles and groups in mosquitto's priority order
- *(cli)* add --ws and --ws-path to the shared connect arguments
- *(server)* [**breaking**] count messages the broker drops, by cause and per client
- *(client)* trust the OS certificate store and take a rustls config
- *(client)* [**breaking**] mark public structs and struct-like variants non_exhaustive
- *(mercurioctl)* [**breaking**] print failures as JSON on standard error with --json
- *(mercurioctl)* [**breaking**] confirm deleting retained messages by filter and sessions
- *(mercurioctl)* read new client passwords from a file or standard input
- *(cli)* print shell completion scripts with a completions command
- *(cli)* log in with a client certificate using --cert and --key
- *(cli)* read the login password from a variable or a prompt in both tools

### Fixed

- *(mercurioctl)* spot a refused admin socket by its error kind
- *(client)* [**breaking**] turn TLS on when a CA or client certificate is set

### Other

- Merge branch 'feat/mount-point-client-ids'
- share one lint configuration across the workspace
- inherit shared package metadata from [workspace.package]
- declare shared third-party dependencies once for the workspace
- Merge branch 'feat/scram'
- Merge branch 'feat/plugin-interface-0.2'
- describe SCRAM-SHA-256 logins
- Merge branch 'feat/client-features'
- Merge branch 'feat/auth-backends'
- Merge branch 'feat/observability'
- *(recado,mercurioctl)* drop direct mercurio-core and bytes dependencies

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercurioctl-v0.1.0) - 2026-11-10

### Added

- *(mercurioctl)* describe the contents of deb and rpm packages
- *(mercurioctl)* use the admin socket and MERCURIOCTL_* variables by default
- *(mercurioctl)* manage dynamic security with mosquitto_ctrl's commands
- *(mercurioctl)* show answers as tables unless --json is given
- *(mercurioctl)* run broker admin commands from the command line

### Other

- point the READMEs at mercurioctl and the admin guide
