# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/frenzox/mercurio/compare/mercurio-core-v0.1.0...mercurio-core-v0.1.1) - 2026-11-08

### Other

- add a changelog to every crate

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercurio-core-v0.1.0) - 2026-11-08

### Added

- stop sending retained messages whose publisher lost access
- support MQTT 5 shared subscriptions
- support MQTT 5 subscription identifiers
- honour the MQTT 5 Message Expiry Interval
- forward MQTT 5 message properties to subscribers
- *(core,packets)* add no_std support for embedded systems
- *(packets)* add multi-version MQTT protocol support (3.1, 3.1.1, 5.0)
- *(server)* integrate storage abstraction into broker
- *(auth)* implement extensible authentication framework
- *(topic)* add topic name and filter validation

### Fixed

- *(server)* deliver one copy to clients with overlapping subscriptions
- reject Response Topics containing wildcards
- reject MQTT 5 property values the spec rules out
- treat repeated MQTT 5 properties as protocol errors
- decode Request Response Information as itself
- *(packets)* stop malformed packets from crashing or misframing
- don't match $-prefixed topics with leading wildcards
- clippy errors

### Other

- link to the documentation on GitHub Pages
- give every crate a README for its crates.io page
- add the metadata crates.io needs to publish each crate
- code fmt
- *(message)* use Arc<str> for topic to enable cheap cloning
- QoS2
- Update some dependencies
- Remove manual Default impl for QoS
- Fix typo
- Restructure codebase
