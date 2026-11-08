# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/frenzox/mercurio/compare/mercurio-packets-v0.1.0...mercurio-packets-v0.1.1) - 2026-11-08

### Other

- add a changelog to every crate

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercurio-packets-v0.1.0) - 2026-11-08

### Added

- *(client)* expose MQTT 5 session, will, publish and subscribe options
- support MQTT 5 subscription identifiers
- forward MQTT 5 message properties to subscribers
- *(packets)* support MQTT 3.x wire format for PUBLISH and (UN)SUBSCRIBE
- *(cli)* add will message options to subscriber command
- *(core,packets)* add no_std support for embedded systems
- *(client)* add mercurio-client library for MQTT connections
- *(packets)* add multi-version MQTT protocol support (3.1, 3.1.1, 5.0)
- *(auth)* implement extensible authentication framework
- *(session)* implement UNSUBSCRIBE packet handler

### Fixed

- *(packets)* send only the QoS in MQTT 3.x subscription options
- *(packets)* keep every subscription identifier in a PUBLISH
- reject MQTT 5 property values the spec rules out
- treat repeated MQTT 5 properties as protocol errors
- *(packets)* encode AUTH properties with their length prefix
- decode Request Response Information as itself
- handle QoS 2 acknowledgements for unknown packet identifiers
- *(packets)* reject reserved header flags and inconsistent CONNECT flags
- *(packets)* stop malformed packets from crashing or misframing
- *(packets)* accept acks and AUTH without optional fields
- *(packets)* encode DISCONNECT properties and MQTT 3.x form correctly
- *(packets)* reject reserved subscription option bits in MQTT 3.x
- *(packets)* send 0x80 for failed MQTT 3.x subscriptions
- *(packets)* consume remaining length when decoding ping packets
- clippy errors

### Other

- link to the documentation on GitHub Pages
- give every crate a README for its crates.io page
- add the metadata crates.io needs to publish each crate
- clippy fixes
- *(session)* add unit tests for UNSUBSCRIBE handler
- QoS2
- Update some dependencies
- Ignore target dir
- Restructure codebase
