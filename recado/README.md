# recado

A command-line MQTT client: `recado pub` publishes a message and
`recado sub` prints messages as they arrive. It speaks MQTT 3.1, 3.1.1
and 5.0 with any broker. *Recado* is Portuguese for a message passed on.

It's part of [Mercurio](https://github.com/frenzox/mercurio), together with the
[`mercuriod`](https://crates.io/crates/mercuriod) broker.

## Installation

Each [release](https://github.com/frenzox/mercurio/releases) has `.deb`
and `.rpm` packages for x86_64, aarch64 and armv7 Linux:

```sh
sudo apt install ./recado_*.deb
sudo dnf install ./recado-*.rpm
```

Or with cargo:

```sh
cargo install recado
```

## Usage

```sh
# Print every message under sensors/, with its topic
recado sub -t 'sensors/#' -T

# Publish at QoS 1
recado pub -t sensors/kitchen -m 21.5 -q 1

# From standard input, retained
date | recado pub -t system/time -r

# Over TLS, with credentials
recado sub -H broker.example.com -p 8883 --tls -u alice -P secret -t '#'

# An MQTT 5 request with properties
recado pub -t requests/lamp -m on --response-topic replies/me \
    --correlation-data 42 --user-property source=cli

# A persistent subscriber that reconnects on its own
recado sub -t 'alerts/#' -i monitor -c -x 3600 --reconnect 5 -q 1
```

Connection options, for both commands:

| Option | Meaning | Default |
|---|---|---|
| `-H, --host` | Broker host | `localhost` |
| `-p, --port` | Broker port | `1883` |
| `-i, --client-id` | Client id | generated |
| `-u, --username`, `-P, --password` | Credentials | |
| `-V, --protocol-version` | `5`, `311` or `31` | `5` |
| `--tls`, `--cafile`, `--insecure` | TLS, its CA, or no verification | |

Run `recado pub --help` or `recado sub --help` for every option, or see
the [CLI guide](https://frenzox.github.io/mercurio/cli.html).

## License

MIT or Apache-2.0, at your option.
