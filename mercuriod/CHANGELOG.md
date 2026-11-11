# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/frenzox/mercurio/compare/mercuriod-v0.4.5...mercuriod-v0.5.0) - 2026-11-11

### Added

- *(server)* [**breaking**] start brokers through Server::builder
- *(server)* [**breaking**] hand a departed share member's messages to its group
- *(auth)* [**breaking**] let a failed authentication tell the client why
- *(plugin)* [**breaking**] name the mount point of an expired session in WIT 0.2
- *(server)* disconnect clients that stay over their publish rate
- *(mercurioctl)* name the mount point of a client
- *(server)* [**breaking**] give each mount point its own client id space
- *(mercuriod)* reload the HTTP auth certificate files
- *(server)* limit publish rates per client and connection rates per listener
- *(server)* [**breaking**] ban addresses that keep failing to log in
- *(server)* per-listener limits and user names as client ids
- *(server)* choose how shared subscriptions pick their member
- *(mercuriod)* store SCRAM-SHA-256 keys with passwords and offer SCRAM
- *(server)* [**breaking**] cap sessions and let mercuriod sessions expire after a week
- *(server)* let a full queue drop its oldest messages instead
- *(plugin)* [**breaking**] hand plugins their settings as a Config with get and require
- *(plugin)* let WebAssembly plugins publish messages
- *(plugin)* [**breaking**] move the WebAssembly plugin interface to 0.2
- *(http-auth)* add a superuser endpoint
- *(http-auth)* verify endpoints with a custom CA and a client certificate
- *(mercuriod)* configure bridge sessions, remote state and counters
- *(mercuriod)* [**breaking**] notify systemd of readiness and run as Type=notify
- *(mercuriod)* import mosquitto's dynamic-security.json
- *(dynsec)* add modifyClient, modifyGroup and modifyRole
- *(server)* serve /healthz and Prometheus /metrics on WebSocket listeners
- *(dynsec)* [**breaking**] check roles and groups in mosquitto's priority order
- *(server)* [**breaking**] keep $SYS retained messages in memory
- *(server)* choose how far client CRLs reach and what they must cover
- *(server)* serve a caller-supplied rustls configuration
- *(server)* accept clients without a certificate where it is optional
- *(server)* [**breaking**] count messages the broker drops, by cause and per client
- *(server)* [**breaking**] reload TLS certificates without a restart
- *(client)* [**breaking**] let the connector open the stream asynchronously
- *(mercuriod)* add --check and validate everything before binding
- *(mercuriod)* give Unix socket listeners a mode and a group
- *(mercuriod)* [**breaking**] make plugin_timeout and the tick interval configurable
- *(client)* [**breaking**] take connect_timeout as a Duration
- *(mercuriod)* back off exponentially between bridge connection attempts
- *(client)* [**breaking**] add manual acknowledgement of received messages
- *(storage)* [**breaking**] make the PostgreSQL pool size and wait configurable
- *(mercuriod)* disconnect clients a dynamic security change concerns
- *(mercuriod)* warn when HTTP auth sends passwords in clear
- *(mercuriod)* bind a dynamic security client to its clientid

### Fixed

- *(mercuriod)* let plugin publishes wait for the broker to serve
- *(mercuriod)* check bridges with --check and before binding
- *(storage)* version the SQLite schema and keep one broker per database
- *(mercuriod)* keep bridge acknowledgements across cancelled forwarding
- *(server)* store a client whole again after a batch is given up
- *(server)* [**breaking**] keep the queues of connected persistent sessions in storage
- *(storage)* [**breaking**] store subscription changes one at a time
- *(server)* [**breaking**] require proxy_from with proxy_protocol on TCP listeners
- *(server)* [**breaking**] refuse unknown admin arguments and log who changes things
- *(server)* [**breaking**] let enhanced authentication send final data on success
- *(mercuriod)* refuse to take over an admin socket another process serves
- *(server)* [**breaking**] require the mqtt subprotocol and add allowed_origins
- *(server)* [**breaking**] decide $CONTROL publishes with a hook of their own
- *(mercuriod)* never let a plugin's sockets listen
- *(mercuriod)* cap a plugin copy's memory across all its memories
- *(mercuriod)* count calls the broker cancels as plugin failures
- *(mercuriod)* [**breaking**] refuse password file entries that are not hashes
- *(recado)* report arguments the client refuses as usage errors
- *(mercuriod)* [**breaking**] forward bridged QoS 1 and 2 messages at least once
- *(mercuriod)* [**breaking**] refuse bridge settings that collide or can't work
- *(mercuriod)* give dynsec-init's file to the owner of its directory
- *(storage)* [**breaking**] require TLS for remote PostgreSQL and when a CA is set
- *(mercuriod)* install config.toml readable by root and mercurio only
- *(mercuriod)* raise the unit's file limit and tighten its sandbox
- *(mercuriod)* reconnect to syslog after the daemon restarts
- *(mercuriod)* keep a starting broker alive on SIGHUP
- *(mercuriod)* [**breaking**] deny all access to user names dynsec has no client for
- *(server)* [**breaking**] count connections per address before handshakes, IPv6 by /64
- *(mercuriod)* make unknown dynsec users cost a password check
- *(mercuriod)* bound HTTP auth requests and share identical ones
- *(mercuriod)* [**breaking**] refuse role and group priorities dynsec does not apply
- *(server)* [**breaking**] cap CONNECT at 64 KiB until the client is authenticated
- *(mercuriod)* stop caching HTTP auth failures as denials
- *(mercuriod)* flush the dynamic security file to disk before renaming it
- *(mercuriod)* apply setDefaultACLAccess only when every entry is valid
- *(mercuriod)* bring bridges back when this broker drops their connection
- *(mercuriod)* name the file and table in configuration errors
- *(server)* [**breaking**] check sent credentials even when auth is not required
- *(mercurio-server)* bound socket writes and the client id takeover wait
- *(server)* [**breaking**] cap unacknowledged messages per client with max_inflight
- *(mercuriod)* keep other users when passwd cannot read the password file
- check that a granted filter covers the filter a client subscribes to
- *(mercuriod)* [**breaking**] refuse authentication settings while auth is disabled
- *(storage)* open SQLite databases in WAL mode
- *(mercuriod)* [**breaking**] refuse configuration keys the daemon does not know
- *(mercuriod)* keep $CONTROL closed under a permissive dynsec default

### Other

- Merge branch 'build/rand-argon2'
- Merge branch 'feat/mount-point-client-ids'
- *(mercuriod)* bump argon2 to 0.6
- *(server)* drop rand for getrandom and std's RandomState
- Merge branch 'chore/workspace-manifests'
- share one lint configuration across the workspace
- inherit shared package metadata from [workspace.package]
- declare shared third-party dependencies once for the workspace
- move mercuriod to toml 1
- Merge branch 'master' into refactor/codec-api
- Merge branch 'fix/storage-read'
- Merge branch 'feat/limits'
- Merge branch 'feat/scram'
- Merge branch 'feat/plugin-interface-0.2'
- *(server)* format the limits work and satisfy clippy
- describe SCRAM-SHA-256 logins
- *(mercuriod)* keep the plugin panic log test from losing its log lines
- *(core)* [**breaking**] replace QoS and RetainHandling Invalid with TryFrom
- *(mercuriod)* allow for the refill while a publish flood test runs
- *(plugin)* describe interface 0.2 and what changes for 0.1 plugins
- *(core)* [**breaking**] give reason code 0x00 a single variant
- *(server)* [**breaking**] report storage health under one $SYS prefix
- Merge branch 'feat/client-features'
- Merge branch 'feat/auth-backends'
- Merge branch 'fix/durability'
- Merge branch 'feat/observability'
- *(plugins)* give every plugin crate its own workspace
- Merge branch 'feat/tls-reload'
- Merge branch 'feat/client-api'
- Merge branch 'fix/auth-admin'
- Merge branch 'fix/edge'
- *(mercuriod)* name ureq as the client that sends URL user info
- Merge branch 'fix/client-reconnect-bridge'
- Merge branch 'fix/dynsec'
- Merge branch 'fix/postgres'
- Merge branch 'fix/httpauth-packaging'
- Merge branch 'chore/ci-release-docs'
- *(mercuriod)* describe HTTP auth failures, limits and https use
- *(mercuriod)* correct the user_url client id and auto_id_prefix notes
- *(mercuriod)* list the plugin keys in the sample config and README
- Merge remote-tracking branch 'origin/master'
- Merge branch 'fix/client-event-loop'
- Merge branch 'fix/conn-loop-hardening'
- Merge branch 'fix/daemon-config'
- Merge branch 'fix/session-limits'
- Merge branch 'fix/authorization'

## [0.4.5](https://github.com/frenzox/mercurio/compare/mercuriod-v0.4.4...mercuriod-v0.4.5) - 2026-11-10

### Fixed

- *(mercuriod)* warn when the configured log level is replaced by info

### Other

- *(mercuriod)* wrap the admin socket comment in the sample config

## [0.4.4](https://github.com/frenzox/mercurio/compare/mercuriod-v0.4.3...mercuriod-v0.4.4) - 2026-11-10

### Added

- *(mercuriod)* recommend mercurioctl in its packages
- *(mercuriod)* list bridges with their link state and queue
- *(mercuriod)* show the configuration in use with its secrets redacted
- *(mercuriod)* reload files on request and report how each went
- *(mercuriod)* change the log level of a running broker
- *(mercuriod)* let the dynsec-init administrator run broker admin commands
- *(mercuriod)* report WASM plugin hooks, copies and failures
- *(mercuriod)* report the daemon's own plugins as built-in in listPlugins
- *(mercuriod)* serve an admin socket for administrators on the host

### Other

- point the READMEs at mercurioctl and the admin guide

## [0.4.3](https://github.com/frenzox/mercurio/compare/mercuriod-v0.4.2...mercuriod-v0.4.3) - 2026-11-09

### Other

- Merge pull request #70 from frenzox/docs/readme-recado
- use recado in examples that don't need mosquitto's tools
- match package files by glob instead of naming a version

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
