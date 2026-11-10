# mercuriod

The [Mercurio](https://github.com/frenzox/mercurio) MQTT broker daemon. It
serves MQTT 3.1, 3.1.1 and 5.0 over TCP, TLS and WebSocket. To try it, the
[`recado`](https://crates.io/crates/recado) client publishes and subscribes
from the command line, and
[`mercurioctl`](https://crates.io/crates/mercurioctl) administers it while
it runs.

## Installation

Each [release](https://github.com/frenzox/mercurio/releases) has `.deb`
and `.rpm` packages for x86_64, aarch64 and armv7 Linux, which install the
binary, the configuration in `/etc/mercurio/` and the systemd unit:

```bash
sudo apt install ./mercuriod_*.deb
sudo dnf install ./mercuriod-*.rpm
```

Or with cargo:

```bash
cargo install mercuriod
```

Or build from the workspace root:

```bash
cargo build --release --package mercuriod
```

## Usage

```bash
mercuriod [OPTIONS]
mercuriod passwd -f <FILE> -u <USER> [-d]
mercuriod dynsec-init -f <FILE> [-u <USER>]
```

| Option | Description | Default |
|--------|-------------|---------|
| `-c, --config <FILE>` | Path to configuration file | `/etc/mercurio/config.toml` |
| `-l, --listen <ADDR>` | Override the listen address, such as `0.0.0.0:1883` | - |
| `-v, --verbose` | Log at debug level | - |

Without a configuration file at the default path, the broker starts with
the defaults below. A missing file at a path given with `-c` is an error.

## Configuration

Mercuriod reads a TOML file. Every setting is optional; this is the full
list with the defaults, also available as
[`contrib/config.toml`](contrib/config.toml):

```toml
# Merge the *.toml files of this directory, in name order: their tables
# extend this file's, their [[listeners]] and [[bridges]] are added, and
# their other values replace this file's
# include_dir = "/etc/mercurio/conf.d"

[server]
# Host address to bind to
# Use 0.0.0.0 to listen on all interfaces
host = "127.0.0.1"

# Port to listen on (default MQTT port is 1883)
port = 1883

# Only accept connections arriving on this network interface (Linux only).
# IPv6 hosts, such as "::", work as they are
# bind_interface = "eth0"

# Also serve MQTT over WebSocket (with TLS when it is enabled) on this port
# websocket_port = 8080

# Serve the files of this directory to plain HTTP requests on websocket_port,
# such as a browser client's page; [[listeners]] take it too
# http_dir = "/var/www/mercurio"

# Seconds between updates of the broker statistics published under
# $SYS/broker/ (0 disables them)
sys_interval = 10

# Maximum number of concurrent connections
max_connections = 10000

# Largest packet accepted from a client, in bytes
max_packet_size = 1048576

# Messages that may wait for one client (QoS 1 and 2 included) before
# further ones are dropped
max_queued_messages = 10000

# Queued messages beyond which QoS 0 messages are dropped
max_queued_qos0_messages = 1000

# Payload bytes that may wait for one client before further messages are
# dropped, whatever their QoS
# max_queued_bytes = 104857600

# Send messages at the QoS of the subscription even when they were published
# with a lower one; goes beyond the MQTT specification
# upgrade_outgoing_qos = false

# Send a retained message to new subscribers only while the client that
# published it may still publish to its topic, so access revoked in the ACL
# also stops its retained messages
# check_retain_source = false
# Subscriptions one session may hold; further ones get Quota Exceeded
# max_subscriptions = 10000

# Seconds a new connection has, in all, to send its PROXY header, complete
# the TLS and WebSocket handshakes and send CONNECT
connect_timeout = 10

# Highest topic alias MQTT 5 clients may use (0 disables topic aliases)
max_topic_alias = 10

# QoS 2 messages an MQTT 5 client may have awaiting PUBREL at once
receive_maximum = 65535

# QoS 1 and 2 messages sent to one client and not yet acknowledged, at most,
# whatever Receive Maximum the client announces; each holds its message in
# memory until acknowledged
max_inflight = 64

# Longest keep-alive, in seconds, MQTT 5 clients may use; clients asking for
# more, or for none, are told to use this instead
# max_keepalive = 300

# Response topic prefix given to MQTT 5 clients that request Response
# Information; %c is replaced by the client id
# response_information = "responses/%c"

# Replace repeated topics with aliases for MQTT 5 clients that accept them,
# saving bandwidth. Off by default because some client libraries announce
# alias support they don't implement.
# send_topic_aliases = true

# Highest QoS clients may publish and subscribe with; higher subscriptions
# are downgraded, higher publishes cost the connection
# max_qos = 2

# Set to false to refuse retained messages
# retain_available = true

# Accept clients that send an empty client id, assigning them one
# allow_zero_length_clientid = true
# Prefix of the client ids the broker assigns
# auto_id_prefix = "auto-"
# Only accept client ids starting with one of these
# clientid_prefixes = ["sensor-", "auto-"]

# Seconds the session of an MQTT 3.x client that connected without clean
# session outlives its connection; forever when unset, as MQTT 3.x has no
# session expiry of its own
# persistent_client_expiration = 604800
# Most seconds any session may outlive its connection, MQTT 5 clients
# included; worth setting where clients aren't trusted (unbounded by default)
# max_session_expiry = 2592000
# Topics that may hold a retained message at once, about 20 of the broker's
# own $SYS topics included; further ones get Quota Exceeded (unbounded)
# max_retained_messages = 100000
# Connections one client address may hold at once, counting the address a
# PROXY header gives (unbounded by default, as NAT puts many behind one)
# max_connections_per_ip = 100

# Send every client to another server instead of serving it. MQTT 5 clients
# are told where to go; MQTT 3.x clients only learn this server is unavailable.
# [server.redirect]
# server_reference = "broker2.example.com:1883"
# permanent = false

[server.tls]
# Serve TLS on `port` (and on `websocket_port`) instead of plain TCP
enabled = false
# Certificate chain and private key, in PEM format
# cert_path = "/etc/mercurio/server.crt"
# key_path = "/etc/mercurio/server.key"
# Require clients to present a certificate signed by this CA (mutual TLS)
# ca_path = "/etc/mercurio/ca.crt"
# Reject client certificates revoked in these CRLs (PEM); needs ca_path
# crl_path = "/etc/mercurio/crl.pem"
# Use the client certificate's Common Name as the user name, instead of
# the user name and password the client sends; needs ca_path
# identity_as_username = false
# Or the whole certificate subject, as in "CN=alice,O=Example,C=PT"
# subject_as_username = false
# Oldest TLS version accepted: "1.2" or "1.3"
# min_version = "1.2"

[logging]
# Log level: trace, debug, info, warn, error. Levels can also be set per
# module, as in "info,mercurio_server::session=warn"
level = "info"
# Append logs to this file instead of standard output. Rotate it with
# logrotate's copytruncate, as the file stays open
# file = "/var/log/mercurio/mercuriod.log"
# Send logs to the local syslog daemon (/dev/log) instead
# syslog = false
# Log clients connecting and disconnecting; turn off on busy brokers
# connection_messages = true

[storage]
# Where sessions, retained messages, wills and in-flight messages are kept:
# "memory" (lost on restart), "sqlite" (kept in the file at `path`) or
# "postgres" (kept in the database at `url`)
backend = "memory"
# path = "/var/lib/mercurio/mercurio.db"
# url = "postgres://mercurio:password@localhost/mercurio?sslmode=require"
# CA that signed the PostgreSQL server's certificate; Mozilla's roots otherwise
# ca_path = "/etc/mercurio/postgres-ca.pem"
# One broker serves from a PostgreSQL database at a time. When another is,
# exit (false), or wait and take over once it stops (true)
# standby = false

[auth]
# Require clients to authenticate
enabled = false

# Allow clients that send no user name (when auth is enabled)
allow_anonymous = false

# User names and Argon2 password hashes, managed with `mercuriod passwd`
# password_file = "/etc/mercurio/passwd"

# Restrict the topics each client may publish and subscribe to, using
# mosquitto's acl_file format. Anything not granted is denied. User rules
# only apply to user names verified by authentication; other clients are
# matched as anonymous.
# acl_file = "/etc/mercurio/acl"

# Manage clients, roles and access at runtime instead, with the commands of
# mosquitto's dynamic security plugin; replaces password_file and acl_file.
# Create the file with `mercuriod dynsec-init -f <path>`.
# dynamic_security = "/var/lib/mercurio/dynamic-security.json"

# Or delegate checks to an HTTP service: a 2xx answer allows
# [auth.http]
# user_url = "http://auth.internal/mqtt/user"
# acl_url = "http://auth.internal/mqtt/acl"
# timeout_seconds = 5
# cache_seconds = 30

[admin]
# Unix socket for administering the broker from this machine, as mercurioctl
# does. Its clients skip authentication and access control, so only root,
# the broker's own user and the members of `group` may connect; others are
# closed at once. Every directory on a custom socket path must be writable
# only by root or the broker's user. "" turns it off. Linux only: elsewhere
# this is ignored.
socket = "/run/mercurio/admin.sock"
# Members are read when the broker starts; add an administrator with
# `usermod -aG mercurio <user>`. "" leaves only root and the broker's user.
group = "mercurio"

# Extra listeners, each with its own protocol ("mqtt" or "websocket") and
# TLS settings, on a TCP `port` (`host` defaults to 127.0.0.1) or a Unix
# socket `path`.
# [[listeners]]
# host = "0.0.0.0"
# port = 8883
# [listeners.tls]
# enabled = true
# cert_path = "/etc/mercurio/server.crt"
# key_path = "/etc/mercurio/server.key"
#
# [[listeners]]
# port = 8080
# protocol = "websocket"
#
# Unix domain socket, for clients on the same machine
# [[listeners]]
# path = "/run/mercurio/mqtt.sock"
#
# Any TCP listener may also set max_connections, on top of the broker-wide
# limit, and bind_interface
# [[listeners]]
# host = "0.0.0.0"
# port = 8884
# max_connections = 100
# bind_interface = "eth1"
#
# Behind a load balancer that sends the PROXY protocol (v1 or v2), so logs
# show the clients' own addresses
# [[listeners]]
# host = "0.0.0.0"
# port = 1884
# proxy_protocol = true
# Proxies allowed to send the header, as addresses or CIDR blocks; others
# are closed, as their header could claim any address (any when empty)
# proxy_from = ["10.0.0.0/8"]
#
# A tenant's listener: its clients only see topics under the mount point
# and log in with their own password file. `[listeners.auth]` takes the
# same settings as `[auth]` and replaces it for this listener.
# [[listeners]]
# host = "0.0.0.0"
# port = 1885
# mount_point = "tenant1/"
# [listeners.auth]
# enabled = true
# allow_anonymous = false
# password_file = "/etc/mercurio/tenant1.passwd"

# Bridges forward topics to and from another broker
# [[bridges]]
# name = "cloud"
# host = "broker.example.com"
# port = 8883
# client_id = "mercurio-bridge-cloud"
# username = "site1"
# password = "secret"
# Instead of host and port: brokers tried in turn when one is unreachable
# addresses = ["broker1.example.com:8883", "broker2.example.com:8883"]
# "mqttv5" (default) or "mqttv311" for brokers without MQTT 5
# protocol_version = "mqttv5"
# Publish "1"/"0", retained, to $SYS/broker/connection/<client id>/state
# notifications = true
# [bridges.tls]
# enabled = true
# ca_path = "/etc/mercurio/cloud-ca.crt"
# Client certificate, for brokers that require mutual TLS
# cert_path = "/etc/mercurio/site1.crt"
# key_path = "/etc/mercurio/site1.key"
#
# Each topic is forwarded "out" (the default), "in" or "both" ways.
# `pattern` is matched below `local_prefix` here and `remote_prefix` there.
# [[bridges.topics]]
# pattern = "sensors/#"
# direction = "out"
# qos = 1
# local_prefix = ""
# remote_prefix = "site1/"
```

### Authentication

With `[auth] enabled = true`, clients must log in with a user name and
password from `password_file`, unless `allow_anonymous` lets clients that
send no user name in. Credentials that are sent are always checked.

The password file holds one `user:hash` line per user, with Argon2 hashes.
Files written by `mosquitto_passwd` work too: their `$7$` (PBKDF2-SHA512)
and `$6$` (SHA-512) hashes are recognised.
Manage it with:

```bash
# Add a user, or change their password (prompts for it)
mercuriod passwd -f /etc/mercurio/passwd -u alice

# Remove a user
mercuriod passwd -f /etc/mercurio/passwd -u alice -d
```

### Access control

`acl_file` restricts which topics each client may publish and subscribe to,
in the format of mosquitto's `acl_file`, so existing files can be reused:

```text
# Rules before any `user` line apply to anonymous clients
topic read public/#

user alice
topic readwrite alice/#
topic deny alice/secret

# Patterns apply to every client; %u is the user name, %c the client id
pattern write devices/%c/status
```

The access is `read`, `write`, `readwrite` (the default) or `deny`.
Anything not granted is denied, and `deny` overrides any grant. A
subscription is allowed only if a rule covers its whole filter: a client
granted `sensors/+/temperature` may subscribe to it, but not to
`sensors/#`. Shared subscriptions are checked by their topic filter:
`$share/group/sensors/+/temperature` needs the same grant.

### Dynamic security

`dynamic_security` replaces `password_file` and `acl_file` with clients,
roles and access rules that change while the broker runs, as with
mosquitto's dynamic security plugin. Create the file with an administrator:

```bash
mercuriod dynsec-init -f /var/lib/mercurio/dynamic-security.json -u admin
```

The administrator manages it with `mercurioctl`, which names mosquitto's
commands in kebab-case, as `mosquitto_ctrl` does:

```bash
mercurioctl dynsec create-role sensor
mercurioctl dynsec add-role-acl sensor publishClientSend 'sensors/#' allow
mercurioctl dynsec create-client s1 --role sensor   # asks for the password
```

Underneath, these are JSON commands on `$CONTROL/dynamic-security/v1`, in
the format of mosquitto's plugin.

Supported commands: `createClient`, `deleteClient`, `setClientPassword`,
`enableClient`, `disableClient`, `addClientRole`, `removeClientRole`,
`getClient`, `listClients`, `createRole`, `deleteRole`, `addRoleACL`,
`removeRoleACL`, `getRole`, `listRoles`, `setDefaultACLAccess` and
`getDefaultACLAccess`, and for groups `createGroup`, `deleteGroup`,
`addGroupRole`, `removeGroupRole`, `addGroupClient`, `removeGroupClient`,
`getGroup`, `listGroups`, `setAnonymousGroup` and `getAnonymousGroup`. A
client has its own roles and those of its groups; clients without a user
name get those of the anonymous group. ACLs of type `publishClientSend`
decide publishing, `publishClientReceive` receiving, and
`subscribePattern` and `subscribeLiteral` subscribing. The highest
`priority` wins. Without a matching ACL, only receiving is allowed, unless
`setDefaultACLAccess` changes it. Changes are saved to the file at once.

`$CONTROL` topics are only open to clients when access control grants
them, so a broker without `acl_file` or `dynamic_security` refuses them
over the network. The admin socket, below, needs no grant.

### HTTP authentication

`[auth.http]` delegates checks to an HTTP service, as mosquitto-go-auth's
HTTP backend does. For each login, the broker POSTs
`{"username", "password", "clientid"}` to `user_url`; `clientid` is
always empty. For each topic check, it POSTs
`{"username", "clientid", "topic", "acc"}` to `acl_url`, with `acc` 1 to
receive, 2 to publish and 4 to subscribe. A 2xx status allows; anything else, a timeout
included, denies. Topic answers are reused for `cache_seconds`, since
every message is checked. `user_url` replaces `password_file` and
`acl_url` replaces `acl_file`.

### Reloading

Send `SIGHUP` (`systemctl reload mercuriod`), or run
`mercurioctl broker reload`, to reread `password_file`,
`acl_file` and `dynamic_security` without dropping connections. New ACL
rules apply to every later publish and subscribe, including those of
connected clients. Password changes apply to later logins. If a file fails
to load, the broker logs the error and keeps the previous version. Other
settings still need a restart.

### Administration

`mercurioctl` lists and disconnects clients, bans them, deletes sessions
and retained messages, shows subscriptions, changes the log level and
reloads files while the broker runs. On the broker's host it connects
through the admin socket set in `[admin]`, `/run/mercurio/admin.sock` by
default, which root, the broker's user and members of the `mercurio` group
may use without credentials. Over the network it needs a user that access
control grants `$CONTROL/broker/v1/#`. The
[administration guide](https://frenzox.github.io/mercurio/admin.html)
describes every command, its permissions and what changed since 0.4.

### TLS

Set `enabled = true` under `[server.tls]` with a certificate and key to serve
TLS on `port`, and on `websocket_port` when it is set. With `ca_path`, the
broker also requires a client certificate signed by that CA.
`crl_path` rejects revoked client certificates. With
`identity_as_username`, the certificate's Common Name becomes the client's
user name: it counts as authenticated, and `user` rules in `acl_file`
apply to it. The user name and password in CONNECT are then ignored.
`subject_as_username` uses the whole subject instead, written as RFC 2253
does and as mosquitto's `use_subject_as_username` gives it:
`CN=alice,O=Example,C=PT`.
`min_version = "1.3"` refuses TLS 1.2.

### WebSocket

`websocket_port` serves MQTT over WebSocket, using the `mqtt` subprotocol,
for browsers and clients behind HTTP proxies. Its sessions, retained
messages and subscriptions are shared with the TCP listener.

### Listeners

Each `[[listeners]]` entry serves another port, with its own `protocol`
(`mqtt` or `websocket`) and its own `[listeners.tls]` settings. A typical
setup keeps plain MQTT on localhost in `[server]` and adds a TLS listener on
`0.0.0.0:8883`. All listeners share sessions, subscriptions and retained
messages.

`mount_point` puts a listener's clients in their own part of the topic
tree: a client publishing `sensors/t1` on a listener mounted at
`tenant1/` publishes `tenant1/sensors/t1`, and it is sent topics without
the prefix. A mount point must end with `/`, as `tenant1` alone would
also reach `tenant10/...`. `[listeners.auth]` gives a listener its own authentication and
`acl_file`, which `SIGHUP` reloads like the main ones. ACL rules see the
full topic, mount point included.

A listener with `path` instead of `port` serves a Unix domain socket, which
local clients reach without TCP and which file permissions can restrict.
A socket file left behind by a broker that did not stop cleanly is
replaced.

### Bridges

A `[[bridges]]` entry connects to another broker and forwards topics between
the two, like mosquitto's bridges. Each `[[bridges.topics]]` entry
forwards `pattern` `out`, `in` or `both` ways. The prefixes rename
topics on the way: with `local_prefix = "site/"` and
`remote_prefix = "plant1/"`, `site/sensors/t1` here is
`plant1/sensors/t1` there.

With MQTT 5, bridges use No Local subscriptions so that forwarded
messages never come back. MQTT 3.1.1 has no No Local, so with
`protocol_version = "mqttv311"` the bridge recognises the copies the remote
broker sends back and drops them; an identical message published on the
remote broker just after one was forwarded is dropped too. With
`addresses`, the bridge moves on to the next broker whenever it can't
connect or loses the connection. With `notifications`, the bridge's state
is published, retained, to `$SYS/broker/connection/<client id>/state`:
`1` while connected, `0` otherwise. The bridge
connects to this broker in-process, so authentication and `acl_file` don't
apply to it. If the remote broker is unreachable, the bridge retries every
five seconds, and messages to forward wait in the bridge's queue on this
broker, up to `max_queued_messages`. Its session there lasts a day, so messages for `in` topics
wait for the bridge across short outages.

### Persistence

With `backend = "memory"`, everything is lost when the broker stops. With
`backend = "sqlite"` or `backend = "postgres"`, sessions, subscriptions,
retained messages, wills, queued messages and messages in flight are kept in
the database, so
clients with persistent sessions resume where they left off after a
restart.

Changes are written in the background within milliseconds, so a crash loses
only the changes of its last moments; a shutdown writes everything first.

The SQLite database runs in WAL mode: while the broker runs, it keeps
`-wal` and `-shm` files next to the database, and recent changes may be
only in the `-wal` file. To back it up while the broker runs, use
`sqlite3 mercurio.db ".backup copy.db"`; a plain copy of the database file
alone can miss those changes.

Only one broker serves from a PostgreSQL database at a time. Another one
started on it exits, or, with `standby = true`, waits without listening and
takes over as soon as the serving broker's process stops, or about 11
seconds after its host goes down. A broker that can't reach the database for
about 7 seconds stops and exits with an error, as a standby may take over;
the database refuses writes from a broker that was taken over. PostgreSQL
needs version 12 or later, connected to directly, without PgBouncer.

### Statistics

Every `sys_interval` seconds the broker publishes retained statistics under
`$SYS/broker/`, with mosquitto's names: `version`, `uptime`,
`clients/connected`, `clients/disconnected`, `clients/total`,
`clients/maximum`, `clients/expired`, `messages/received`,
`messages/sent`, `bytes/received`, `bytes/sent`,
`retained messages/count` and `subscriptions/count`, and the 1, 5 and 15
minute averages per minute of messages, bytes and connections under
`load/`, such as `load/messages/received/1min`. Clients
may subscribe to them, but not publish to `$SYS` topics.

## Running

### Direct execution

```bash
# With the default config location
mercuriod

# With a custom config file
mercuriod -c /path/to/config.toml

# Override the listen address
mercuriod -l 0.0.0.0:1883

# Debug logging
mercuriod -v
```

Ctrl+C or SIGTERM shuts the broker down gracefully: clients are sent a
DISCONNECT saying the server is shutting down, and the broker waits up to
five seconds for their connections to close before exiting.

### Systemd service

A systemd unit is provided in [`contrib/mercuriod.service`](contrib/mercuriod.service).
It runs the broker sandboxed, with `/var/lib/mercurio` as its only
writable directory, which suits the SQLite database.

```bash
# Copy service file
sudo cp mercuriod/contrib/mercuriod.service /etc/systemd/system/

# Create config directory and copy config
sudo mkdir -p /etc/mercurio
sudo cp mercuriod/contrib/config.toml /etc/mercurio/

# Copy binary
sudo cp target/release/mercuriod /usr/bin/

# Enable and start service
sudo systemctl daemon-reload
sudo systemctl enable --now mercuriod

# Check status
sudo systemctl status mercuriod

# View logs
journalctl -u mercuriod -f
```

## Features

See the [project README](../README.md#broker-mercuriod) for the full list
of supported MQTT features.
