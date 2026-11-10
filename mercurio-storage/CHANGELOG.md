# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.3](https://github.com/frenzox/mercurio/compare/mercurio-storage-v0.3.2...mercurio-storage-v0.3.3) - 2026-11-10

### Added

- *(storage)* keep broker bans in PostgreSQL
- *(storage)* keep broker bans in the memory and SQLite stores
- *(server)* split starting the broker from running it, with an Admin handle

## [0.3.2](https://github.com/frenzox/mercurio/compare/mercurio-storage-v0.3.1...mercurio-storage-v0.3.2) - 2026-11-09

### Other

- *(storage)* document the SQLite and PostgreSQL backends on docs.rs

## [0.3.1](https://github.com/frenzox/mercurio/compare/mercurio-storage-v0.3.0...mercurio-storage-v0.3.1) - 2026-11-09

### Fixed

- *(storage)* bump rusqlite to pick up the patched bundled SQLite

## [0.3.0](https://github.com/frenzox/mercurio/compare/mercurio-storage-v0.2.0...mercurio-storage-v0.3.0) - 2026-11-08

### Fixed

- *(server)* resume a session only for the user and listener it belongs to

### Other

- [**breaking**] remove API the crates never used
- *(storage)* point stores overriding apply to apply_each
- declare Rust 1.89 as the minimum supported version
- *(storage)* [**breaking**] keep database libraries' types out of the API

## [0.2.0](https://github.com/frenzox/mercurio/compare/mercurio-storage-v0.1.0...mercurio-storage-v0.2.0) - 2026-11-08

### Added

- *(storage)* [**breaking**] apply changes in batches, fenced on PostgreSQL
- *(storage)* add a PostgreSQL backend

### Fixed

- [**breaking**] drop the std feature from crates that always need std
- *(storage)* build the postgres feature without tokio's macros

### Other

- document storage durability, failover timings and PgBouncer
- *(storage)* share column encodings between SQL backends
- add a changelog to every crate

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercurio-storage-v0.1.0) - 2026-11-08

### Added

- stop sending retained messages whose publisher lost access
- publish mosquitto's traffic, load and client statistics in $SYS
- *(storage)* add a store for messages queued for offline clients
- *(server)* run on any storage backend, expiring sessions across restarts
- support MQTT 5 subscription identifiers
- support MQTT 5 subscription options
- honour the MQTT 5 Message Expiry Interval
- forward MQTT 5 message properties to subscribers
- *(core,packets)* add no_std support for embedded systems
- *(storage)* add SQLite persistent storage backend
- *(storage)* add storage abstraction layer

### Fixed

- *(server)* deliver one copy to clients with overlapping subscriptions
- *(storage)* report unreadable message properties instead of dropping them
- *(storage)* remove expired retained messages instead of skipping them
- keep will messages accurate across reconnects and restarts
- *(server)* resend unacknowledged messages when a session resumes
- don't match $-prefixed topics with leading wildcards
- *(server)* persist the granted QoS of each subscription

### Other

- link to the documentation on GitHub Pages
- give every crate a README for its crates.io page
- add the metadata crates.io needs to publish each crate
- dual-license under MIT or Apache-2.0
