# Mercuriod

Mercurio MQTT broker daemon.

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
```

### Options

| Option | Description | Default |
|--------|-------------|---------|
| `-c, --config <FILE>` | Path to configuration file | /etc/mercurio/config.toml |
| `-l, --listen <ADDR>` | Override listen address | - |
| `-v, --verbose` | Enable verbose/debug output | - |

## Configuration

Mercuriod uses a TOML configuration file. By default, it looks for `/etc/mercurio/config.toml`.

### Example Configuration

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

[logging]
# Log level: trace, debug, info, warn, error
level = "info"

[storage]
# Where sessions, retained messages, wills and in-flight messages are kept:
# "memory" (lost on restart) or "sqlite" (kept in the file at `path`)
backend = "memory"
# path = "/var/lib/mercurio/mercurio.db"

[auth]
# Enable authentication
enabled = false

# Allow anonymous connections (when auth is enabled)
allow_anonymous = true

# Restrict the topics each client may publish and subscribe to, using
# mosquitto's acl_file format. Anything not granted is denied. User rules
# only apply to user names verified by authentication; other clients are
# matched as anonymous.
# acl_file = "/etc/mercurio/acl"
```

## Running

### Direct execution

```bash
# With default config location
mercuriod

# With custom config file
mercuriod -c /path/to/config.toml

# Override listen address
mercuriod -l 0.0.0.0:1883

# Verbose output for debugging
mercuriod -v
```

### Systemd service

A systemd service file is provided in `contrib/mercuriod.service`.

```bash
# Copy service file
sudo cp contrib/mercuriod.service /etc/systemd/system/

# Create config directory and copy config
sudo mkdir -p /etc/mercurio
sudo cp contrib/config.toml /etc/mercurio/

# Copy binary
sudo cp target/release/mercuriod /usr/bin/

# Enable and start service
sudo systemctl daemon-reload
sudo systemctl enable mercuriod
sudo systemctl start mercuriod

# Check status
sudo systemctl status mercuriod

# View logs
journalctl -u mercuriod -f
```

## Protocol Support

Mercuriod supports:
- MQTT 3.1
- MQTT 3.1.1
- MQTT 5.0

## Features

- Publish/Subscribe messaging
- QoS 0, 1, and 2
- Retained messages
- Will messages
- Topic wildcards (`+` and `#`)
- Keep-alive handling
- Session persistence
