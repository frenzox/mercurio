# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.1](https://github.com/frenzox/mercurio/compare/mercurio-client-v0.6.0...mercurio-client-v0.6.1) - 2026-11-13

### Fixed

- *(client)* say the broker wants TLS when it answers plain MQTT with TLS

### Other

- Merge branch 'docs/scram-channel-binding'
- describe SCRAM channel binding in the READMEs

## [0.6.0](https://github.com/frenzox/mercurio/compare/mercurio-client-v0.5.0...mercurio-client-v0.6.0) - 2026-11-11

### Added

- *(client)* authenticate with SCRAM-SHA-256-PLUS
- *(client)* [**breaking**] tell authenticators about the TLS channel

### Fixed

- *(client)* log a failed TLS exporter and fix the channel doc

## [0.5.0](https://github.com/frenzox/mercurio/compare/mercurio-client-v0.4.1...mercurio-client-v0.5.0) - 2026-11-11

### Added

- *(server)* [**breaking**] start brokers through Server::builder
- *(auth)* [**breaking**] let a failed authentication tell the client why
- *(core)* [**breaking**] make encoding fallible and refuse what peers must reject
- *(cli)* log in with SCRAM-SHA-256 in recado and mercurioctl
- *(client)* add a SCRAM-SHA-256 authenticator
- *(client)* add a request/response helper and use it in mercurioctl
- *(client)* follow Server References with a hop limit
- *(client)* make subscriptions again after a reconnect without session
- *(client)* send repeated topics as outbound topic aliases
- *(cli)* add --ws and --ws-path to the shared connect arguments
- *(client)* send DISCONNECT with a reason and session expiry
- *(client)* [**breaking**] expose the client id and what CONNACK settled
- *(client)* add a WebSocket transport behind a websocket feature
- *(client)* [**breaking**] let the connector open the stream asynchronously
- *(client)* trust the OS certificate store and take a rustls config
- *(client)* put TLS behind a default-on tls feature
- *(client)* [**breaking**] mark public structs and struct-like variants non_exhaustive
- *(client)* re-export the foreign types its public API uses
- *(client)* [**breaking**] keep error sources and tell transient errors apart
- *(client)* [**breaking**] take connect_timeout as a Duration
- *(client)* start publishes without waiting for the previous one
- *(client)* [**breaking**] add manual acknowledgement of received messages
- *(client)* [**breaking**] back off exponentially with jitter between reconnections
- *(client)* end with a Stopped event when reconnecting gives up
- *(cli)* log in with a client certificate using --cert and --key
- *(cli)* read the login password from a variable or a prompt in both tools

### Fixed

- *(mercuriod)* check bridges with --check and before binding
- *(server)* name the crypto provider for the client certificate verifier
- *(server)* [**breaking**] let enhanced authentication send final data on success
- *(server)* set TCP_NODELAY, keepalive and TCP_USER_TIMEOUT on accept
- *(client)* [**breaking**] cap inbound packets at 1 MiB and refuse them from the header
- *(client)* [**breaking**] validate topics, filters and subscription ids before sending
- *(client)* refuse a CONNACK with Session Present after a clean start
- *(client)* check Maximum QoS and Retain Available for queued publishes
- *(client)* fail a resent publish the broker's new size limit refuses
- *(client)* keep reconnecting through transient CONNACK refusals
- *(client)* [**breaking**] bound every socket write and treat a stalled one as lost
- *(recado)* make up a client id where MQTT 3 refuses an empty one
- *(client)* do not time out the broker for a stall of the client itself
- *(client)* fail a pending re-authentication when the connection drops
- *(client)* [**breaking**] turn TLS on when a CA or client certificate is set
- *(client)* keep the event loop running while the event queue is full
- *(client)* disconnect when the last client handle is dropped

### Other

- Merge branch 'chore/workspace-manifests'
- share one lint configuration across the workspace
- inherit shared package metadata from [workspace.package]
- declare shared third-party dependencies once for the workspace
- depend on webpki-roots 1 in the client and storage
- move mercurio-client and mercurio-server to thiserror 2
- Merge branch 'master' into refactor/codec-api
- *(core)* [**breaking**] check Variable Byte Integer and identifier ranges
- *(packets)* [**breaking**] pass the protocol version to every encode and decode
- *(packets)* [**breaking**] derive CONNECT flags from the payload
- *(core)* [**breaking**] replace QoS and RetainHandling Invalid with TryFrom
- *(core)* [**breaking**] give reason code 0x00 a single variant
- Merge branch 'feat/client-features'
- Merge branch 'feat/client-api'
- Merge branch 'fix/auth-admin'
- *(client)* drop the unused tokio-util dependency
- Merge branch 'fix/client-reconnect-bridge'
- *(client)* list backoff and manual acknowledgement among the features
- *(client)* format the tests added for the reconnect and limit fixes

## [0.4.1](https://github.com/frenzox/mercurio/compare/mercurio-client-v0.4.0...mercurio-client-v0.4.1) - 2026-11-10

### Added

- *(client)* share connection arguments behind a clap feature
- *(client)* connect to brokers over Unix sockets

### Other

- point the READMEs at mercurioctl and the admin guide

## [0.4.0](https://github.com/frenzox/mercurio/compare/mercurio-client-v0.3.0...mercurio-client-v0.4.0) - 2026-11-09

### Added

- *(server)* [**breaking**] pass the client to Plugin::begin_auth
- *(server)* [**breaking**] authenticate clients through the plugin chain

## [0.3.0](https://github.com/frenzox/mercurio/compare/mercurio-client-v0.2.0...mercurio-client-v0.3.0) - 2026-11-08

### Fixed

- refuse values too long for MQTT instead of truncating them

### Other

- *(packets)* [**breaking**] take a shared slice in ControlPacket::check
- [**breaking**] remove API the crates never used
- document the crates' APIs
- declare Rust 1.89 as the minimum supported version
- [**breaking**] mark the enums that may grow as non-exhaustive
- *(server)* [**breaking**] let configuration grow without breaking users
- *(client)* add an endurance check through broker failures
- *(client)* benchmark many concurrent connections

## [0.2.0](https://github.com/frenzox/mercurio/compare/mercurio-client-v0.1.0...mercurio-client-v0.2.0) - 2026-11-08

### Fixed

- [**breaking**] drop the std feature from crates that always need std

### Other

- *(client)* benchmark a persistent subscriber
- add a changelog to every crate

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercurio-client-v0.1.0) - 2026-11-08

### Added

- *(client)* authenticate with a client certificate
- *(client)* open the connection with a custom connector
- *(client)* support MQTT 5 enhanced authentication
- *(client)* announce the client's own limits and requests in CONNECT
- *(client)* reconnect and resume the session after losing the broker
- *(client)* expose MQTT 5 session, will, publish and subscribe options
- *(client)* add TLS support using rustls
- *(core,packets)* add no_std support for embedded systems
- *(client)* add mercurio-client library for MQTT connections

### Fixed

- name the TLS crypto provider instead of relying on the default
- *(client)* report the broker's answer to each unsubscribe
- *(client)* keep resent messages within the broker's new Receive Maximum
- *(packets)* keep every subscription identifier in a PUBLISH
- *(client)* respect the limits the broker announces in CONNACK
- *(client)* end the connection on DISCONNECT and report why
- *(client)* detect dead brokers and follow the broker's keep-alive
- *(client)* complete QoS and subscription exchanges with the broker
- *(client)* use the configured protocol version on the wire

### Other

- link to the documentation on GitHub Pages
- give every crate a README for its crates.io page
- add the metadata crates.io needs to publish each crate
- *(client)* add a load generator for comparing brokers
- *(tls)* load PEM files with rustls instead of rustls-pemfile
