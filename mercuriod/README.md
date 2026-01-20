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

# Maximum number of concurrent connections
max_connections = 10000

[logging]
# Log level: trace, debug, info, warn, error
level = "info"

[auth]
# Enable authentication
enabled = false

# Allow anonymous connections (when auth is enabled)
allow_anonymous = true
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
