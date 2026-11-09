# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1](https://github.com/frenzox/mercurio/compare/mercurio-server-v0.4.0...mercurio-server-v0.4.1) - 2026-11-09

### Other

- updated the following local packages: mercurio-storage

## [0.4.0](https://github.com/frenzox/mercurio/compare/mercurio-server-v0.3.0...mercurio-server-v0.4.0) - 2026-11-09

### Added

- *(server)* [**breaking**] pass the client to Plugin::begin_auth
- *(server)* [**breaking**] leave room to grow plugin events and message actions
- *(server)* give plugins a tick and their own $CONTROL topics
- *(server)* tell plugins about connections, subscriptions and expiry
- *(server)* let plugins rewrite or drop published messages
- *(server)* [**breaking**] check topic access through the plugin chain
- *(server)* [**breaking**] authenticate clients through the plugin chain
- *(server)* add the Plugin trait and the chain that runs it

### Fixed

- *(server)* drop messages a plugin moves to reserved topics
- *(server)* name the plugin in enhanced authentication timeouts and panics
- *(server)* bound the plugin event queue and drop events past it
- *(server)* catch plugin panics raised while a hook builds its future
- *(server)* pair Disconnected with Connected and skip no-plugin work

### Other

- spell out the plugin behaviours that are easy to get wrong
- *(server)* cover fail-closed authentication and listener plugin chains
- explain plugins in the embedding guide
- *(server)* skip access checks for trusted connections

## [0.3.0](https://github.com/frenzox/mercurio/compare/mercurio-server-v0.2.0...mercurio-server-v0.3.0) - 2026-11-08

### Added

- limit connections per client address, and who may send PROXY
- cap how many topics may hold a retained message
- cap how long sessions outlive their connection

### Fixed

- refuse values too long for MQTT instead of truncating them
- *(server)* give the whole handshake one deadline
- bound the subscriptions and queued bytes of each session
- *(server)* keep payloads out of debug logs
- *(server)* refuse wills that couldn't be published
- check passwords off the async threads, unknown users as slowly
- *(server)* cap WebSocket messages at the largest MQTT packet
- *(server)* keep network clients from taking in-process client ids
- *(server)* resume a session only for the user and listener it belongs to
- *(server)* keep wildcards in client ids from widening ACL patterns
- *(server)* cap topic levels so a deep filter can't overflow the stack

### Other

- *(packets)* [**breaking**] take a shared slice in ControlPacket::check
- [**breaking**] remove API the crates never used
- document the crates' APIs
- *(server)* keep an idle connection's state small
- *(server)* stop storage writers once their client goes quiet
- *(server)* drop the redundant write buffer, release grown buffers
- declare Rust 1.89 as the minimum supported version
- [**breaking**] mark the enums that may grow as non-exhaustive
- *(server)* [**breaking**] let configuration grow without breaking users
- *(server)* [**breaking**] make the internal modules private
- *(server)* cut the memory each connection holds from 44 KB to 25 KB
- *(server)* look retained messages up only for those that get them

## [0.2.0](https://github.com/frenzox/mercurio/compare/mercurio-server-v0.1.0...mercurio-server-v0.2.0) - 2026-11-08

### Added

- *(server)* store each client's state in the background, in order

### Fixed

- [**breaking**] drop the std feature from crates that always need std

### Other

- add a changelog to every crate

## [0.1.0](https://github.com/frenzox/mercurio/releases/tag/mercurio-server-v0.1.0) - 2026-11-08

### Added

- *(server)* serve static files from WebSocket listeners
- *(server)* log connections under their own tracing target
- stop sending retained messages whose publisher lost access
- optionally deliver at the subscription's QoS, as upgrade_outgoing_qos
- *(server)* limit connections per listener
- take the user name from the whole client certificate subject
- *(server)* check subscribing separately from receiving
- publish mosquitto's traffic, load and client statistics in $SYS
- *(server)* let trusted connections publish to $SYS
- *(server)* only an authorizer can open $CONTROL topics to clients
- *(server)* mount points and authentication per listener
- limit the payload bytes queued for one client
- accept the PROXY protocol from load balancers
- serve MQTT on Unix domain sockets
- expire persistent MQTT 3.x sessions after a configured time
- client id policies: required ids, assigned id prefix, allowed prefixes
- limit the QoS clients may use and allow disabling retain
- revocation lists, minimum TLS version and certificate identities
- *(server)* accept trusted in-process connections
- *(server)* serve any number of listeners with their own TLS settings
- publish broker statistics under $SYS
- serve MQTT over WebSocket
- *(server)* validate UTF-8 payloads and report unread publishes
- optionally send topic aliases to MQTT 5 clients
- redirect clients to another server
- give MQTT 5 clients Response Information when they ask
- let operators cap the keep-alive of MQTT 5 clients
- *(server)* explain refusals and disconnects with reason strings
- *(server)* announce keep-alive timeouts and shutdowns to clients
- *(server)* keep messages for offline clients across restarts
- *(server)* stateful authentication exchanges and verified identities
- restrict topics per client with mosquitto-style ACL files
- *(server)* run on any storage backend, expiring sessions across restarts
- *(server)* advertise broker capabilities in CONNACK
- support MQTT 5 enhanced authentication during CONNECT
- support MQTT 5 shared subscriptions
- respect the client's Maximum Packet Size
- enforce MQTT 5 Receive Maximum in both directions
- accept MQTT 5 topic aliases from clients
- support MQTT 5 subscription identifiers
- support MQTT 5 subscription options
- honour the MQTT 5 Message Expiry Interval
- forward MQTT 5 message properties to subscribers
- *(server)* make broker limits configurable and cap queued messages
- *(server)* tell clients why their connection is being closed
- *(server)* support the MQTT 5 Will Delay Interval
- *(server)* integrate username/password authentication
- *(client)* add TLS support using rustls
- *(server)* add TLS support using rustls
- *(core,packets)* add no_std support for embedded systems
- add mercuriod daemon and reorganize CLI tools
- *(packets)* add multi-version MQTT protocol support (3.1, 3.1.1, 5.0)
- *(server)* add keep-alive timeout handling
- *(server)* add inflight message tracking for QoS 1/2 retry
- *(server)* add will message persistence across broker restarts
- *(server)* add session persistence across broker restarts
- *(server)* integrate storage abstraction into broker
- *(auth)* implement extensible authentication framework
- *(topic)* add topic name and filter validation
- *(session)* implement will message support
- *(broker)* implement retained message support
- *(session)* implement UNSUBSCRIBE packet handler

### Fixed

- *(server)* log a full message queue once instead of per message
- name the TLS crypto provider instead of relying on the default
- *(server)* deliver one copy to clients with overlapping subscriptions
- *(server)* keep the DISCONNECT from being lost when closing
- *(server)* check shared subscriptions against their topic filter
- *(daemon)* exit with a failure status when the broker can't start
- *(server)* stop logging connections closed for shutdown as errors
- *(packets)* keep every subscription identifier in a PUBLISH
- *(server)* report invalid filters in UNSUBSCRIBE
- reject MQTT 5 property values the spec rules out
- *(server)* reject packets using packet identifier 0
- *(server)* let QoS 0 messages overtake a full Receive Maximum window
- keep will messages accurate across reconnects and restarts
- honour allow_anonymous when authentication is enabled
- *(server)* resend unacknowledged messages when a session resumes
- handle QoS 2 acknowledgements for unknown packet identifiers
- *(server)* close connections that don't send CONNECT in time
- *(server)* keep accepting connections after accept errors
- *(server)* enforce and advertise a maximum packet size
- don't match $-prefixed topics with leading wildcards
- *(server)* queue messages per session instead of per topic
- *(server)* honour session expiry and will requests in DISCONNECT
- *(server)* don't resend retained messages when restoring a session
- *(server)* persist the granted QoS of each subscription
- *(server)* keep stored subscriptions when resuming a live session
- *(server)* end sessions when they expire
- *(server)* close the existing connection when a client id reconnects
- *(server)* reject empty MQTT 3.x client id without clean session
- *(server)* give clients without an identifier their own session
- *(server)* keep incoming QoS 2 state apart from outgoing state
- *(server)* speak the protocol version negotiated in CONNECT
- *(server)* allocate outgoing packet identifiers per session
- *(server)* honour the QoS requested in SUBSCRIBE
- *(server)* flag retained messages as retained on delivery
- *(server)* stop crashing connections that disable keep-alive
- *(server)* deliver messages under the published topic name

### Other

- link to the documentation on GitHub Pages
- give every crate a README for its crates.io page
- add the metadata crates.io needs to publish each crate
- *(tls)* load PEM files with rustls instead of rustls-pemfile
- clear all clippy warnings
- *(server)* read CONNACK by its length in integration tests
- *(server)* fix remaining length of hand-built SUBSCRIBE packets
- fmt
- code fmt
- code fmt
- *(session)* add unit tests for UNSUBSCRIBE handler
- *(connection)* reuse write buffer to avoid per-packet allocation
- *(session)* optimize ack tracking with HashMap for O(1) lookup
- *(broker)* remove redundant mutex wrapper
- *(topic-tree)* reduce allocations in publish path
- update comment
- Remove dead code
- QoS2
- Update some dependencies
- Make return packet optional
- Restructure codebase
