# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.2](https://github.com/frenzox/mercurio/compare/mercuriod-v0.4.1...mercuriod-v0.4.2) - 2026-11-09

### Other

- explain installing recado from the packages

## [0.4.1](https://github.com/frenzox/mercurio/compare/mercuriod-v0.4.0...mercuriod-v0.4.1) - 2026-11-09

### Added

- *(mercuriod)* describe the contents of deb and rpm packages
- *(mercuriod)* run the systemd service as a mercurio user

### Other

- explain installing mercuriod from the packages

## [0.4.0](https://github.com/frenzox/mercurio/compare/mercuriod-v0.3.0...mercuriod-v0.4.0) - 2026-11-09

### Added

- *(server)* [**breaking**] pass the client to Plugin::begin_auth
- *(plugin)* log a Rust plugin's panics through the broker
- *(examples)* add audit and rewrite plugin examples
- *(mercuriod)* configure WebAssembly plugins and reload them on SIGHUP
- *(mercuriod)* run WebAssembly plugins in the plugin chain
- *(server)* [**breaking**] check topic access through the plugin chain
- *(server)* [**breaking**] authenticate clients through the plugin chain

### Fixed

- *(examples)* let payload-rewrite pass empty retained messages
- *(mercuriod)* refuse plugin names that clash with topics or built-ins
- *(mercuriod)* bound and escape the messages plugins log
- *(mercuriod)* count a fixed cost per kv entry against the quota
- *(mercuriod)* keep plugins from holding sockets without bound
- *(examples)* keep every byte the webhook test receives
- *(mercuriod)* bound what a WebAssembly plugin can hold and wait on
- *(mercuriod)* keep a dynamic security reload out of a running batch
- *(mercuriod)* serialize dynamic security command batches

### Other

- *(mercuriod)* count distinct kv keys to show the store is shared
- *(mercuriod)* pin the init timeout test to the timeout error
- *(mercuriod)* hold WASM plugins to their time, memory and network limits
- *(mercuriod)* build the HTTP ACL request only when the cache misses
- *(mercuriod)* wait for HTTP auth without blocking a runtime thread
- *(mercuriod)* answer dynamic security commands as a plugin

## [0.3.0](https://github.com/frenzox/mercurio/compare/mercuriod-v0.2.0...mercuriod-v0.3.0) - 2026-11-08

### Added

- limit connections per client address, and who may send PROXY
- cap how many topics may hold a retained message
- cap how long sessions outlive their connection
- *(mercuriod)* [**breaking**] refuse anonymous clients once authentication is on

### Fixed

- *(server)* give the whole handshake one deadline
- *(mercuriod)* evict HTTP auth answers one at a time
- bound the subscriptions and queued bytes of each session
- *(mercuriod)* tighten dynamic security
- *(mercuriod)* require mount points to end with a slash
- *(mercuriod)* forward from bridges only what their routes cover
- check passwords off the async threads, unknown users as slowly

### Other

- declare Rust 1.89 as the minimum supported version
- [**breaking**] mark the enums that may grow as non-exhaustive
- *(server)* [**breaking**] let configuration grow without breaking users
- *(server)* [**breaking**] make the internal modules private

## [0.2.0](https://github.com/frenzox/mercurio/compare/mercuriod-v0.1.0...mercuriod-v0.2.0) - 2026-11-08

### Added

- *(storage)* [**breaking**] apply changes in batches, fenced on PostgreSQL
- *(mercuriod)* keep state in PostgreSQL, with standby brokers

### Other

- document storage durability, failover timings and PgBouncer
- add a changelog to every crate

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercuriod-v0.1.0) - 2026-11-08

### Added

- *(mercuriod)* configure http_dir for WebSocket listeners
- *(mercuriod)* log to syslog, and optionally without connection messages
- stop sending retained messages whose publisher lost access
- optionally deliver at the subscription's QoS, as upgrade_outgoing_qos
- *(mercuriod)* IPv6 hosts, bind_interface and per-listener max_connections
- *(mercuriod)* merge configuration files from include_dir
- take the user name from the whole client certificate subject
- *(mercuriod)* dynamic security groups and the anonymous group
- *(server)* check subscribing separately from receiving
- *(mercuriod)* delegate authentication and ACL checks to HTTP endpoints
- publish mosquitto's traffic, load and client statistics in $SYS
- *(mercuriod)* accept password files written by mosquitto_passwd
- *(mercuriod)* bridge to MQTT 3.1.1 brokers, with failover and mutual TLS
- *(mercuriod)* manage clients, roles and access at runtime
- *(mercuriod)* mount points and auth settings per listener
- *(server)* mount points and authentication per listener
- limit the payload bytes queued for one client
- accept the PROXY protocol from load balancers
- serve MQTT on Unix domain sockets
- *(mercuriod)* write logs to a file
- expire persistent MQTT 3.x sessions after a configured time
- client id policies: required ids, assigned id prefix, allowed prefixes
- limit the QoS clients may use and allow disabling retain
- revocation lists, minimum TLS version and certificate identities
- *(mercuriod)* bridge topics to and from other brokers
- *(server)* accept trusted in-process connections
- *(mercuriod)* configure extra listeners with [[listeners]]
- *(server)* serve any number of listeners with their own TLS settings
- *(mercuriod)* reload password and ACL files on SIGHUP
- publish broker statistics under $SYS
- serve MQTT over WebSocket
- optionally send topic aliases to MQTT 5 clients
- redirect clients to another server
- give MQTT 5 clients Response Information when they ask
- let operators cap the keep-alive of MQTT 5 clients
- *(server)* stateful authentication exchanges and verified identities
- restrict topics per client with mosquitto-style ACL files
- *(daemon)* choose the storage backend in the configuration
- enforce MQTT 5 Receive Maximum in both directions
- accept MQTT 5 topic aliases from clients
- *(daemon)* expose broker limits in the configuration file
- *(daemon)* add file-based credential store with argon2
- *(server)* integrate username/password authentication
- *(client)* add TLS support using rustls
- *(server)* add TLS support using rustls
- add mercuriod daemon and reorganize CLI tools

### Fixed

- *(mercuriod)* limit glibc to two malloc arenas
- name the TLS crypto provider instead of relying on the default
- *(server)* check shared subscriptions against their topic filter
- *(daemon)* exit with a failure status when the broker can't start
- *(daemon)* shut down gracefully on SIGTERM
- honour allow_anonymous when authentication is enabled
- *(server)* close connections that don't send CONNECT in time

### Other

- link to the documentation on GitHub Pages
- give every crate a README for its crates.io page
- add the metadata crates.io needs to publish each crate
- *(mercuriod)* document every broker feature and fix the systemd unit
- clear all clippy warnings
- fmt
