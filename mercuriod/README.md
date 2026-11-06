# Mercuriod

The Mercurio MQTT broker daemon. It serves MQTT 3.1, 3.1.1 and 5.0 over TCP,
TLS and WebSocket.

## Installation

```bash
cargo install --path mercuriod
```

Or build from the workspace root:

```bash
cargo build --release --package mercuriod
```

## Usage

```bash
mercuriod [OPTIONS]
mercuriod passwd -f <FILE> -u <USER> [-d]
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
[server]
# Host address to bind to
# Use 0.0.0.0 to listen on all interfaces
host = "127.0.0.1"

# Port to listen on (default MQTT port is 1883)
port = 1883

# Also serve MQTT over WebSocket (with TLS when it is enabled) on this port
# websocket_port = 8080

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

# Seconds a new connection has to complete TLS and send CONNECT
connect_timeout = 10

# Highest topic alias MQTT 5 clients may use (0 disables topic aliases)
max_topic_alias = 10

# QoS 2 messages an MQTT 5 client may have awaiting PUBREL at once
receive_maximum = 65535

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
# Oldest TLS version accepted: "1.2" or "1.3"
# min_version = "1.2"

[logging]
# Log level: trace, debug, info, warn, error
level = "info"

[storage]
# Where sessions, retained messages, wills and in-flight messages are kept:
# "memory" (lost on restart) or "sqlite" (kept in the file at `path`)
backend = "memory"
# path = "/var/lib/mercurio/mercurio.db"

[auth]
# Require clients to authenticate
enabled = false

# Allow clients that send no user name (when auth is enabled)
allow_anonymous = true

# User names and Argon2 password hashes, managed with `mercuriod passwd`
# password_file = "/etc/mercurio/passwd"

# Restrict the topics each client may publish and subscribe to, using
# mosquitto's acl_file format. Anything not granted is denied. User rules
# only apply to user names verified by authentication; other clients are
# matched as anonymous.
# acl_file = "/etc/mercurio/acl"

# Extra listeners, each with its own protocol ("mqtt" or "websocket") and
# TLS settings. `host` defaults to 127.0.0.1.
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

# Bridges forward topics to and from another MQTT 5 broker
# [[bridges]]
# name = "cloud"
# host = "broker.example.com"
# port = 8883
# client_id = "mercurio-bridge-cloud"
# username = "site1"
# password = "secret"
# [bridges.tls]
# enabled = true
# ca_path = "/etc/mercurio/cloud-ca.crt"
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

### Reloading

Send `SIGHUP` (`systemctl reload mercuriod`) to reread `password_file` and
`acl_file` without dropping connections. New ACL rules apply to every later
publish and subscribe, including those of connected clients. Password
changes apply to later logins. If a file fails to load, the broker logs the
error and keeps the previous version. Other settings still need a restart.

### TLS

Set `enabled = true` under `[server.tls]` with a certificate and key to serve
TLS on `port`, and on `websocket_port` when it is set. With `ca_path`, the
broker also requires a client certificate signed by that CA.
`crl_path` rejects revoked client certificates. With
`identity_as_username`, the certificate's Common Name becomes the client's
user name: it counts as authenticated, and `user` rules in `acl_file`
apply to it. The user name and password in CONNECT are then ignored.
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

### Bridges

A `[[bridges]]` entry connects to another broker and forwards topics between
the two, like mosquitto's bridges. Each `[[bridges.topics]]` entry
forwards `pattern` `out`, `in` or `both` ways. The prefixes rename
topics on the way: with `local_prefix = "site/"` and
`remote_prefix = "plant1/"`, `site/sensors/t1` here is
`plant1/sensors/t1` there.

The remote broker must support MQTT 5: bridges use No Local
subscriptions so that forwarded messages never come back. The bridge
connects to this broker in-process, so authentication and `acl_file` don't
apply to it. If the remote broker is unreachable, the bridge retries every
five seconds. Its session there lasts a day, so messages for `in` topics
wait for the bridge across short outages.

### Persistence

With `backend = "memory"`, everything is lost when the broker stops. With
`backend = "sqlite"`, sessions, subscriptions, retained messages, wills,
queued messages and messages in flight are kept in the database file, so
clients with persistent sessions resume where they left off after a
restart.

### Statistics

Every `sys_interval` seconds the broker publishes retained statistics under
`$SYS/broker/`: `version`, `uptime`, `clients/connected`, `clients/total`,
`messages/received`, `messages/sent` and `subscriptions/count`. Clients
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
