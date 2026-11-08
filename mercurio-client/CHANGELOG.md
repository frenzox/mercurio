# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
