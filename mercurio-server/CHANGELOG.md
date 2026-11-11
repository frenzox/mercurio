# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://github.com/frenzox/mercurio/compare/mercurio-server-v0.6.0...mercurio-server-v0.7.0) - 2026-11-11

### Added

- *(server)* [**breaking**] accept SCRAM-SHA-256-PLUS on TLS 1.3 connections
- *(scram)* [**breaking**] bind exchanges to the TLS channel with tls-exporter
- *(server)* let ScramAuth take its decoy secret

### Other

- *(server)* pin plain SCRAM over TLS 1.3 and the binding mismatch code

## [0.6.0](https://github.com/frenzox/mercurio/compare/mercurio-server-v0.5.0...mercurio-server-v0.6.0) - 2026-11-11

### Added

- *(server)* [**breaking**] start brokers through Server::builder
- *(server)* add with_* setters to the configuration structs
- *(server)* [**breaking**] type the TLS and listener configuration errors
- *(server)* [**breaking**] hand a departed share member's messages to its group
- *(auth)* [**breaking**] let a failed authentication tell the client why
- *(admin)* filter listClients by mount point
- *(server)* disconnect clients that stay over their publish rate
- *(server)* [**breaking**] give each mount point its own client id space
- *(storage)* [**breaking**] key stored client state by mount point and client id
- *(server)* limit publish rates per client and connection rates per listener
- *(server)* [**breaking**] ban addresses that keep failing to log in
- *(core)* [**breaking**] make encoding fallible and refuse what peers must reject
- *(server)* per-listener limits and user names as client ids
- *(server)* choose how shared subscriptions pick their member
- *(server)* [**breaking**] cap sessions and let mercuriod sessions expire after a week
- *(server)* add a built-in SCRAM-SHA-256 enhanced auth mechanism
- *(server)* let a full queue drop its oldest messages instead
- *(server)* report the plugin interface a WASM plugin was built for
- *(server)* [**breaking**] tell plugins which connection a client came in on
- *(server)* serve /healthz and Prometheus /metrics on WebSocket listeners
- *(server)* [**breaking**] keep $SYS retained messages in memory
- *(server)* choose how far client CRLs reach and what they must cover
- *(server)* serve a caller-supplied rustls configuration
- *(server)* accept clients without a certificate where it is optional
- *(server)* [**breaking**] count messages the broker drops, by cause and per client
- *(server)* [**breaking**] reload TLS certificates without a restart
- *(storage)* [**breaking**] tell fenced and transient storage failures apart

### Fixed

- *(server)* substitute ACL pattern identities in one pass
- *(server)* [**breaking**] end every task of a stopped broker with Server::run
- *(server)* read a waiting packet before expiring the keep-alive
- *(server)* store MQTT 3.x sessions with their configured expiry
- *(server)* keep only the CONNECT settings a session reads later
- *(server)* store a starting session's own expiry when reconciling
- *(server)* keep a restored will stored until it is published
- *(server)* keep a delayed will due when a takeover is abandoned
- *(server)* unsubscribe a removed session even if its removal is cancelled
- *(server)* set aside stored sessions that can't be read at restore
- *(server)* stop blaming the previous connection when admissions are full
- *(storage)* version the SQLite schema and keep one broker per database
- *(server)* store a client whole again after a batch is given up
- *(server)* name the crypto provider for the client certificate verifier
- *(server)* [**breaking**] keep the queues of connected persistent sessions in storage
- *(storage)* [**breaking**] store subscription changes one at a time
- *(server)* [**breaking**] keep incoming QoS 2 state and record it only once accepted
- *(storage)* [**breaking**] keep RETAIN and the expiry deadline of in-flight messages
- *(server)* [**breaking**] publish the will when a client disconnects with an error
- *(server)* [**breaking**] compare built-in plaintext passwords in constant time
- *(server)* refuse wildcard mount points and misplaced WebSocket settings
- *(server)* hold WebSocket clients to the CONNECT size before auth
- *(server)* [**breaking**] require proxy_from with proxy_protocol on TCP listeners
- *(server)* [**breaking**] refuse unknown admin arguments and log who changes things
- *(server)* [**breaking**] let enhanced authentication send final data on success
- *(server)* [**breaking**] require the mqtt subprotocol and add allowed_origins
- *(server)* [**breaking**] decide $CONTROL publishes with a hook of their own
- *(server)* leave a session to expire when its takeover is abandoned
- *(server)* log client-caused connection failures below error
- *(server)* record a session's expiry before giving its client id back
- *(server)* set TCP_NODELAY, keepalive and TCP_USER_TIMEOUT on accept
- *(server)* let go of a claim and its admission when start is abandoned
- *(server)* stream http_dir files under their own timeout and limit
- *(server)* exempt in-process connections from max_connections
- *(server)* give the client id back when a connection lease is dropped
- *(server)* [**breaking**] close banned clients with Administrative Action
- *(server)* check the client's packet size limit after topic aliasing
- *(server)* hold resumed retransmissions to the client's new limits
- *(server)* send every retained message to a new subscription in pages
- *(server)* keep storage reads out of the session manager's lock
- *(server)* free the client id when stacked takeovers all give up
- *(server)* end connections still connecting when the server shuts down
- *(server)* give the client id back when a session fails to start
- *(server)* [**breaking**] count connections per address before handshakes, IPv6 by /64
- *(server)* [**breaking**] end an expiry-0 session even when a takeover waits
- *(server)* make room in a full queue by dropping expired messages
- *(server)* [**breaking**] cap CONNECT at 64 KiB until the client is authenticated
- *(server)* send shared subscription messages to members that can take them
- *(server)* give up a queued login once its connect time is up
- *(mercurio-server)* refuse banned addresses before any handshake
- *(server)* cap the session expiry a DISCONNECT asks for
- *(server)* [**breaking**] check sent credentials even when auth is not required
- *(mercurio-server)* bound socket writes and the client id takeover wait
- *(server)* free superseded will-delay and session-expiry timers at once
- *(server)* [**breaking**] cap unacknowledged messages per client with max_inflight
- *(server)* [**breaking**] refuse a client id that belongs to another owner's session
- check that a granted filter covers the filter a client subscribes to

### Other

- Merge branch 'refactor/server-structure'
- *(server)* replace SessionManagerDropGuard with a constructor
- *(server)* move the session manager's tests to their own file
- *(server)* move address limits and publish pacing out of server/
- *(server)* move the broker's configuration types to server/config.rs
- *(server)* have acknowledgements hand back their storage writes
- *(server)* give a session's in-flight messages their own type
- *(server)* move subscription handling to session/subscriptions.rs
- *(server)* move outbound delivery to session/outbound.rs
- *(server)* move inbound publish handling to session/inbound.rs
- *(server)* move retained delivery to session/retained.rs
- *(server)* move session presence to session/presence.rs
- *(server)* move the session's will handling to session/will.rs
- *(server)* move the session tests out of session.rs
- *(server)* drop unreachable ack persistence and restore stray docs
- *(server)* [**breaking**] give the broker its own error type
- Merge branch 'build/rand-argon2'
- Merge branch 'feat/mount-point-client-ids'
- *(server)* wait for a crashed broker to let go of its database
- *(server)* drop rand for getrandom and std's RandomState
- Merge branch 'chore/workspace-manifests'
- share one lint configuration across the workspace
- inherit shared package metadata from [workspace.package]
- declare shared third-party dependencies once for the workspace
- ask tokio only for the features mercurio-server uses
- *(mercurio-server)* drop the keep-alive from the write timeout test
- move mercurio-client and mercurio-server to thiserror 2
- Merge branch 'master' into refactor/codec-api
- Merge branch 'fix/storage-read'
- Merge branch 'feat/limits'
- Merge branch 'feat/scram'
- Merge branch 'feat/plugin-interface-0.2'
- *(server)* format the limits work and satisfy clippy
- *(core)* [**breaking**] check Variable Byte Integer and identifier ranges
- *(storage)* [**breaking**] restore stored sessions in pages with one query per table
- *(storage)* [**breaking**] look retained messages up by topic range, a page at a time
- *(packets)* [**breaking**] pass the protocol version to every encode and decode
- *(packets)* [**breaking**] derive CONNECT flags from the payload
- *(core)* [**breaking**] replace QoS and RetainHandling Invalid with TryFrom
- *(core)* [**breaking**] rename Error::MQTTReasonCode to Error::Reason
- *(storage)* [**breaking**] make apply the only write and share one conformance suite
- *(core)* [**breaking**] give reason code 0x00 a single variant
- *(server)* [**breaking**] report storage health under one $SYS prefix
- Merge branch 'fix/durability'
- *(server)* let the drop counter test's plugin grant admin commands
- Merge branch 'feat/observability'
- *(server)* let a writer held by a journal wait without polling
- Merge branch 'feat/tls-reload'
- *(server)* add a test PKI for TLS integration tests
- Merge branch 'fix/auth-admin'
- Merge branch 'fix/edge'
- Merge branch 'fix/session-manager-cancellation'
- Merge branch 'fix/delivery'
- Merge branch 'fix/session-manager'
- Merge branch 'fix/postgres'
- Merge branch 'fix/conn-loop-hardening'
- Merge branch 'fix/session-limits'
- Merge branch 'fix/authorization'

## [0.5.0](https://github.com/frenzox/mercurio/compare/mercurio-server-v0.4.1...mercurio-server-v0.5.0) - 2026-11-10

### Added

- *(server)* serve trusted Unix socket listeners behind a peer filter
- *(server)* drop bans once they expire
- *(server)* close the connections a new ban matches
- *(server)* ban clients by client id, user name or address
- *(server)* report plugin status through the admin API
- *(server)* inspect subscriptions through the admin API
- *(server)* manage retained messages through the admin API
- *(server)* disconnect clients and delete sessions through the admin API
- *(server)* list clients and their sessions through the admin API
- *(server)* serve the broker admin API on $CONTROL/broker/v1
- *(server)* split starting the broker from running it, with an Admin handle
- *(server)* [**breaking**] answer $CONTROL requests only to the client that sent them

### Other

- point the READMEs at mercurioctl and the admin guide

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
