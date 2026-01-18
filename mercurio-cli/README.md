# Mercurio CLI Tools

Command-line tools for publishing and subscribing to MQTT topics.

## Installation

```bash
cargo install --path mercurio-cli
```

Or build from the workspace root:

```bash
cargo build --release --package mercurio-cli
```

Binaries will be available at `target/release/mercurio-pub` and `target/release/mercurio-sub`.

## mercurio-pub

Publish messages to an MQTT broker.

### Usage

```bash
mercurio-pub [OPTIONS] --topic <TOPIC>
```

### Examples

```bash
# Publish a simple message
mercurio-pub -t test/topic -m "Hello, MQTT!"

# Publish with QoS 1 and retain flag
mercurio-pub -t sensor/temperature -m "23.5" -q 1 -r

# Read message from stdin
echo "Hello from stdin" | mercurio-pub -t test/topic

# Publish to a remote broker with authentication
mercurio-pub -h broker.example.com -u myuser -P mypass -t test/topic -m "Hello"

# Use MQTT 3.1.1 protocol
mercurio-pub -V 311 -t test/topic -m "Hello"
```

### Options

| Option | Description |
|--------|-------------|
| `-t, --topic <TOPIC>` | Topic to publish to (required) |
| `-m, --message <MESSAGE>` | Message payload (reads from stdin if not provided) |
| `-q, --qos <QOS>` | QoS level: 0, 1, or 2 (default: 0) |
| `-r, --retain` | Retain the message on the broker |

## mercurio-sub

Subscribe to topics and print received messages.

### Usage

```bash
mercurio-sub [OPTIONS] --topic <TOPIC>
```

### Examples

```bash
# Subscribe to a single topic
mercurio-sub -t test/topic

# Subscribe to multiple topics
mercurio-sub -t "sensor/#" -t "device/+/status"

# Print topic name with each message
mercurio-sub -t "sensor/#" -T

# Subscribe with QoS 1
mercurio-sub -t test/topic -q 1

# Connect to a remote broker
mercurio-sub -h broker.example.com -p 1883 -t test/topic

# With authentication
mercurio-sub -h broker.example.com -u myuser -P mypass -t test/topic
```

### Options

| Option | Description |
|--------|-------------|
| `-t, --topic <TOPIC>` | Topic(s) to subscribe to (required, can be repeated) |
| `-q, --qos <QOS>` | QoS level for subscriptions: 0, 1, or 2 (default: 0) |
| `-T, --print-topic` | Print topic name before each message |

## Common Options

Both tools share these connection options:

| Option | Description | Default |
|--------|-------------|---------|
| `-h, --host <HOST>` | MQTT broker hostname | localhost |
| `-p, --port <PORT>` | MQTT broker port | 1883 |
| `-i, --client-id <ID>` | Client ID (auto-generated if not set) | - |
| `-u, --username <USER>` | Username for authentication | - |
| `-P, --password <PASS>` | Password for authentication | - |
| `-k, --keep-alive <SECS>` | Keep-alive interval in seconds | 60 |
| `-V, --protocol-version <VER>` | MQTT protocol version | 5 |
| `-v, --verbose` | Enable verbose/debug output | - |

### Protocol Versions

The `-V` flag accepts:
- `5` or `5.0` - MQTT 5.0 (default)
- `311` or `3.1.1` - MQTT 3.1.1
- `31` or `3.1` - MQTT 3.1

## Examples

### Basic pub/sub test

Terminal 1:
```bash
mercurio-sub -t test/hello
```

Terminal 2:
```bash
mercurio-pub -t test/hello -m "Hello, World!"
```

### Wildcard subscriptions

```bash
# Single-level wildcard (+)
mercurio-sub -t "sensor/+/temperature"

# Multi-level wildcard (#)
mercurio-sub -t "home/#"
```

### Piping data

```bash
# Publish file contents
cat data.json | mercurio-pub -t data/upload

# Publish command output
date | mercurio-pub -t system/time
```
