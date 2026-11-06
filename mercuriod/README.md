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

### WebSocket

`websocket_port` serves MQTT over WebSocket, using the `mqtt` subprotocol,
for browsers and clients behind HTTP proxies. Its sessions, retained
messages and subscriptions are shared with the TCP listener.

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
