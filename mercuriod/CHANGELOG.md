# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
