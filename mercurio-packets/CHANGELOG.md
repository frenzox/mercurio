# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/frenzox/mercurio/compare/mercurio-packets-v0.2.1...mercurio-packets-v0.3.0) - 2026-11-11

### Added

- *(core)* [**breaking**] make encoding fallible and refuse what peers must reject
- *(packets)* derive Clone on every packet and add packet_type()

### Fixed

- *(packets)* accept DUP on PUBREL, SUBSCRIBE and UNSUBSCRIBE in MQTT 3.1
- *(packets)* [**breaking**] refuse reason codes their packet type does not list
- *(packets)* [**breaking**] refuse an MQTT 3.x SUBACK without return codes
- *(packets)* [**breaking**] refuse packet identifier 0 in every packet that carries one
- *(packets)* [**breaking**] refuse Authentication Data without Authentication Method
- *(core)* [**breaking**] require every Encoder to compute its encoded size
- *(packets)* [**breaking**] make the per-packet decoders safe to call on any input

### Other

- Merge branch 'chore/workspace-manifests'
- *(packets)* copy payloads and binary data as slices when encoding
- share one lint configuration across the workspace
- inherit shared package metadata from [workspace.package]
- declare shared third-party dependencies once for the workspace
- *(packets)* [**breaking**] keep property decoding helpers private
- *(core)* [**breaking**] check Variable Byte Integer and identifier ranges
- *(packets)* [**breaking**] pass the protocol version to every encode and decode
- *(packets)* [**breaking**] derive CONNECT flags from the payload
- *(core)* [**breaking**] replace QoS and RetainHandling Invalid with TryFrom
- *(core)* [**breaking**] rename Error::MQTTReasonCode to Error::Reason
- *(core)* [**breaking**] give reason code 0x00 a single variant
- *(packets)* decode every property list through one helper
- build the core and packets tests without std

## [0.2.1](https://github.com/frenzox/mercurio/compare/mercurio-packets-v0.2.0...mercurio-packets-v0.2.1) - 2026-11-10

### Added

- *(server)* ban clients by client id, user name or address

## [0.2.0](https://github.com/frenzox/mercurio/compare/mercurio-packets-v0.1.1...mercurio-packets-v0.2.0) - 2026-11-08

### Fixed

- *(packets)* write the will properties length from the payload encoder
- *(packets)* keep MQTT 3.1 as the version of acks parsed from it
- *(packets)* let callers set the properties of acknowledgements
- *(packets)* encode MQTT 3.x acknowledgements without a reason code
- *(packets)* reject packets the spec calls malformed
- *(packets)* refuse reserved MQTT 3.x CONNACK return codes
- *(packets)* refuse MQTT 3.x packets with MQTT 5 bodies
- *(packets)* read DISCONNECT reason 0x00 the same, sent or left out

### Other

- *(packets)* [**breaking**] take a shared slice in ControlPacket::check
- document the crates' APIs
- declare Rust 1.89 as the minimum supported version

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
