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

# Each line of standard input as its own message, on one connection
tail -F /var/log/app.log | recado pub -t logs/app -l

# A binary file; sub prints payloads byte for byte
recado pub -t cameras/door/snapshot -f snapshot.jpg

# One JSON object per message, with its topic and properties; the payload
# is in `payload_base64`, and in `payload` too when it is UTF-8
recado sub -t 'sensors/#' --format json | jq .topic

# Over TLS, with credentials
recado sub -H broker.example.com -p 8883 --tls -u alice -t '#'

# In a script, with the password out of `ps`
RECADO_PASSWORD="$(cat ~/.mqtt-password)" recado pub -u alice -t status -m up

# An MQTT 5 request with properties
recado pub -t requests/lamp -m on --response-topic replies/me \
    --correlation-data 42 --user-property source=cli

# In a script: wait up to 10 s for one message, then exit
recado sub -t devices/7/status -C 1 -W 10

# A persistent subscriber that reconnects on its own
recado sub -t 'alerts/#' -i monitor -c -x 3600 --reconnect 5 -q 1
```

Connection options, for both commands:

| Option | Meaning | Default |
|---|---|---|
| `-H, --host` | Broker host | `localhost` |
| `-p, --port` | Broker port | `1883` |
| `--unix` | Unix socket, instead of host and port | |
| `-i, --client-id` | Client id; for MQTT 3.1, and 3.1.1 with `-c`, recado makes up a `recado-` id | assigned by the broker |
| `-u, --username`, `-P, --password` | Credentials; the password also comes from `RECADO_PASSWORD`, and `-u` alone asks for it on a terminal | |
| `-V, --protocol-version` | `5`, `311` or `31`, or mosquitto's `mqttv5`, `mqttv311`, `mqttv31` | `5` |
| `--tls`, `--cafile`, `--insecure` | TLS, verified with the operating system's certificate store or only the `--cafile` CA, or not at all; `--cafile` and `--insecure` imply `--tls` | |
| `--cert`, `--key` | Client certificate and its key (PEM); imply `--tls` | |

Exit status: 0 on success, 1 when the broker refused a subscription or a
message, 2 when it couldn't be reached, refused the login or closed the
connection, 3 on wrong usage, 4 when `sub -W` ran out of time. Messages
go to standard error.

`recado completions <shell>` prints a completion script for bash, zsh,
fish, elvish or PowerShell, such as
`recado completions bash > ~/.local/share/bash-completion/completions/recado`.

Run `recado pub --help` or `recado sub --help` for every option, or see
the [CLI guide](https://frenzox.github.io/mercurio/cli.html).

## License

MIT or Apache-2.0, at your option.
