# Security policy

## Supported versions

Fixes go into the latest release of each crate: `mercuriod`,
`mercurioctl`, `recado`, `mercurio-client`, `mercurio-server`,
`mercurio-storage`, `mercurio-plugin`, `mercurio-packets` and
`mercurio-core`. Older releases get no fixes; upgrade to the latest one.

## Reporting a vulnerability

Report vulnerabilities privately, through
[GitHub's security advisories](https://github.com/frenzox/mercurio/security/advisories/new)
for this repository. Do not open a public issue or pull request for them.

Include what you can of:

- the crate and version, and the configuration involved
- what an attacker can do, and from where: an MQTT client, a plugin, the
  admin socket, the network between the broker and its database
- the steps or the packets that show it

The discussion stays in the advisory until a release fixes the
vulnerability.
