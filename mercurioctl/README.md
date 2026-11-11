# mercurioctl

Administers a running [Mercurio](https://github.com/frenzox/mercurio) MQTT
broker: lists and disconnects clients, bans them, manages retained
messages, shows subscriptions, listeners, bridges and plugins, changes the
log level, reloads files and manages dynamic security. It speaks the
broker's `$CONTROL/broker/v1` API over MQTT 5. The bridge, configuration,
log level, reload and dynamic security commands need the
[`mercuriod`](https://crates.io/crates/mercuriod) daemon.

## Installation

Each [release](https://github.com/frenzox/mercurio/releases) has `.deb`
and `.rpm` packages for x86_64, aarch64 and armv7 Linux:

```sh
sudo apt install ./mercurioctl_*.deb
sudo dnf install ./mercurioctl-*.rpm
```

To install it with the broker from local files, give `apt` both:
`sudo apt install ./mercuriod_*.deb ./mercurioctl_*.deb`. From a
repository, the `mercuriod` package recommends it. Or with cargo:

```sh
cargo install mercurioctl
```

## Usage

On the broker's host, as root or a member of the `mercurio` group,
mercurioctl connects through the admin socket, `/run/mercurio/admin.sock`,
with no credentials:

```sh
# Who is connected
mercurioctl client list --connected

# Disconnect a client without publishing its will, then ban its address
mercurioctl client disconnect sensor-7 --no-will
mercurioctl ban add address 203.0.113.0/24 --duration 3600 --reason flooding

# Retained messages under sensors/, and deleting them; a filter with
# wildcards asks first, and needs --yes in scripts
mercurioctl retained list 'sensors/#'
mercurioctl retained delete 'sensors/#'

# More logging until the next restart, and rereading the ACL file
mercurioctl broker log-level info,mercurio_server::session=debug
mercurioctl broker reload

# Dynamic security, with mosquitto_ctrl's commands in kebab-case
mercurioctl dynsec create-client alice --role sensors
mercurioctl dynsec add-role-acl sensors publishClientSend 'sensors/#' allow

# A remote broker, as a dynamic security administrator, as JSON
mercurioctl -H broker.example.com -p 8883 --tls -u admin dynsec list-clients --json
```

Connection options go before the command, and each can also be set with
its `MERCURIOCTL_` variable, such as `MERCURIOCTL_HOST`. With none of
them set, mercurioctl uses the admin socket. `-u` without `-P` asks for
the password on a terminal; `MERCURIOCTL_PASSWORD` keeps it out of `ps`.

| Option | Meaning | Default |
|---|---|---|
| `-H, --host`, `-p, --port` | Broker over TCP | admin socket, else `localhost:1883` |
| `--unix` | Broker over a Unix socket | |
| `-u, --username`, `-P, --password` | Credentials | |
| `--tls`, `--cafile`, `--insecure` | TLS, verified with the operating system's certificate store or only the `--cafile` CA, or not at all; `--cafile` and `--insecure` imply `--tls` | |
| `--cert`, `--key` | Client certificate and its key (PEM); imply `--tls` | |
| `--ws`, `--ws-path` | MQTT over WebSocket, `wss://` with `--tls`, at the path given (`MERCURIOCTL_WS`, `MERCURIOCTL_WS_PATH`) | `/mqtt` |
| `--json` | Print the response data as JSON | tables |
| `--timeout` | Seconds to wait for the answer (`MERCURIOCTL_TIMEOUT`) | `10` |

Exit status: 0 on success, 1 when the broker refused the command, 2 when
it couldn't be reached, refused the login or didn't answer in time, 3 on
wrong usage.

`mercurioctl completions <shell>` prints a completion script for bash,
zsh, fish, elvish or PowerShell, such as
`mercurioctl completions bash > ~/.local/share/bash-completion/completions/mercurioctl`.

Run `mercurioctl --help`, or `--help` after any command, for the rest, or
see the [mercurioctl guide](https://frenzox.github.io/mercurio/mercurioctl.html)
and the [administration guide](https://frenzox.github.io/mercurio/admin.html).

## License

MIT or Apache-2.0, at your option.
