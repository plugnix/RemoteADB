# remoteadb

Run `adb` against an Android device that is plugged into a different machine,
over an encrypted QUIC tunnel that uses only relay servers you control.

```
        your laptop                                      the machine with the phone
  +-------------------------+                          +----------------------------+
  |  adb --> 127.0.0.1:5037 |      iroh / QUIC         |  remoteadb host            |
  |      (remoteadb connect)| =======================> |    --> adb server (local)  |
  +-------------------------+   your own relay, or     |        --> USB --> phone   |
                                 direct holepunch      +----------------------------+
```

The host machine runs the real adb server next to the phone. Your machine
exposes that server on a local TCP port. The ordinary `adb` binary connects to
it through `ADB_SERVER_SOCKET`, so everything works unchanged: `devices`,
`shell`, `install`, `logcat`, `push`, `pull`, `forward`, `reverse`, Android
Studio, Gradle.

There is nothing to install on the phone and no need for `adb tcpip`, so the
device keeps using its USB connection and does not need to be reachable on any
IP network.

## Why this instead of the alternatives

| Approach | Needs | Trade-off |
| --- | --- | --- |
| `adb tcpip` + `adb connect` | Phone on a routable network | Does not use the USB link; the port is open to that whole network |
| SSH port forward to :5037 | A reachable SSH host, or a jump box | Works well, but needs public IP or VPN, and forwards to an unauthenticated port |
| USB/IP over a tunnel | Kernel modules both ends, Linux client | Gives real fastboot, but USB control transfers over WAN latency are painful |
| remoteadb | A relay you control | Holepunches when it can; no public ports; access controlled by key |

## Security model, in short

adb has **no authentication of its own**. Anything that can reach an adb
server has full control of every device attached to it, including shell access
and app installation. So remoteadb puts the access control in the tunnel:

- A client must present an Ed25519 key whose **endpoint ID is on the host's
  allowlist**. The check happens immediately after the QUIC handshake, before
  a single byte of adb traffic is read.
- The host's adb server stays bound to `127.0.0.1`. Only the tunnel reaches it.
- Traffic is end-to-end encrypted between endpoints. A relay forwards opaque
  QUIC packets and cannot read them.
- The relay has **its own allowlist**, so strangers cannot use your relay
  even as a dumb packet forwarder.
- Nothing is published to any public discovery service. remoteadb builds its
  endpoints with iroh's minimal preset, so no DNS or pkarr record about your
  machines ever leaves the relays you configured.

Keys live in `~/.config/remoteadb/key` (or `/etc/remoteadb/key`) with mode
0600. Whoever can read a client key can impersonate that client, so treat it
like an SSH private key.

## Install

Requires a Rust toolchain (1.91 or newer) and Android `platform-tools` on the
host machine.

```bash
git clone https://github.com/plugnix/RemoteADB
cd RemoteADB
make release
sudo make install          # installs to /usr/local/bin/remoteadb
```

Use the **same platform-tools version on every machine**. See Pitfalls below
for why this matters more than usual.

## Set up a relay

remoteadb refuses to start without a relay, and deliberately does not fall
back to anyone else's. Run [iroh-relay] 1.3.x on any small VPS:

```bash
cargo install iroh-relay --version 1.3.0 --locked --features server
sudo cp deployment/iroh-relay.toml   /etc/iroh-relay/iroh-relay.toml
sudo cp deployment/iroh-relay.service /etc/systemd/system/
# edit the config: set your hostname, contact email, and the allowlist
sudo systemctl enable --now iroh-relay
```

The relay needs ports 80 and 443 reachable; it gets its own Let's Encrypt
certificate and does not sit behind nginx. Put every machine's endpoint ID in
`access.allowlist`, which each machine prints with:

```bash
remoteadb id
```

Then point each machine at the relay once, in
`~/.config/remoteadb/config.toml`:

```toml
relay_urls = ["https://relay.example.com"]
```

For a quick local trial you can skip all of this and run
`iroh-relay --dev`, then pass `--relay-url http://localhost:3340`.

## Share a device

On the machine with the phone plugged in:

```bash
remoteadb host
```

It starts `adb nodaemon server` on loopback if nothing is listening there,
keeps it running, and prints a ticket. Nobody can connect yet. Allow a client
by its endpoint ID:

```bash
remoteadb allow <CLIENT_ENDPOINT_ID>
```

That takes effect on the running host within a couple of seconds, with no
restart. `remoteadb deny` revokes it the same way.

To run at boot:

```bash
remoteadb service install --user     # Linux: systemd --user, adb runs as you
sudo remoteadb service install       # system service, adb runs as root
remoteadb service status             # prints the ticket
remoteadb service log
```

Prefer `--user` on a machine where you also use adb yourself. The server is
then shared with your own `adb` and uses your `~/.android/adbkey`, so devices
that already trust the machine stay trusted. A root service has its own adb
key, so every phone prompts again to authorize USB debugging. With `--user`,
run `loginctl enable-linger $USER` so it survives logout.

A system service reads the machine-wide config, so its allowlist edits need
`sudo remoteadb allow --system <ID>`.

## Use the device

On your own machine:

```bash
remoteadb add --name buildbox <TICKET>   # save it once
remoteadb connect buildbox               # listens on 127.0.0.1:5037
```

In another shell:

```bash
export ADB_SERVER_SOCKET=tcp:127.0.0.1:5037
adb devices
adb -s <serial> shell
adb install app.apk
```

For several hosts at once, give each its own port and select it with `-P`:

```bash
remoteadb add --name lab-a --port 5038 <TICKET_A>
remoteadb add --name lab-b --port 5039 <TICKET_B>
adb -P 5038 devices
adb -P 5039 devices
```

Android Studio and Gradle honour `ADB_SERVER_SOCKET` too, so launching them
with that variable set makes the remote devices appear in the device picker.

## Commands

```
remoteadb host [--adb-port N] [--adb-path P] [--no-adb] [--allow ID]... [--relay-url URL]...
remoteadb connect <ticket|id|name> [--port N] [--bind ADDR] [--relay-url URL]...
remoteadb add --name NAME [--port N] <ticket|id>
remoteadb list
remoteadb remove NAME
remoteadb allow [--system] <ID>
remoteadb deny  [--system] <ID>
remoteadb id    [--system]
remoteadb service install [--user] [--adb-port N] [--relay-url URL]...
remoteadb service uninstall | restart | status | log [--user]
remoteadb paths
remoteadb version
```

## Configuration

```toml
# ~/.config/remoteadb/config.toml   (per user)
# /etc/remoteadb/config.toml        (read by a system service)

relay_urls = ["https://relay.example.com"]
adb_port   = 5037            # host: where adb listens. client: default local port
adb_path   = "/opt/platform-tools/adb"

# Host side: client endpoint IDs allowed to reach this machine's devices.
allowed_clients = [
    "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeeeffffffff0000000011111111",
]

# Client side: hosts saved by name.
[hosts.buildbox]
ticket = "endpoint..."
port   = 5038
```

An unprivileged run reads your file layered over the machine-wide one: lists
merge, single values prefer yours. A system service reads only the
machine-wide file. `remoteadb paths` prints every location.

## Pitfalls

**Pin the same platform-tools version everywhere.** When the adb client finds
a server on `127.0.0.1` whose version differs from its own, it assumes the
server is local and stale, sends `host:kill`, and starts its own. Through the
tunnel that kills the *host's* server. remoteadb restarts it within a couple
of seconds, but the client's own start attempt fails because the tunnel is
holding the port, so the command you ran fails too. Matching versions avoids
this entirely.

**One server, many clients.** Everyone connected to a host sees the same
device list and shares `adb forward` and `adb reverse` state. Address devices
explicitly with `-s <serial>`.

**Latency.** Each adb command opens a new TCP connection, which becomes a new
QUIC stream. The QUIC connection itself is reused, so there is no handshake
cost after the first command, but a relayed path still pays its round trip.
iroh holepunches to a direct path whenever the two networks permit it.

**The Windows service backend is untested.** It type-checks against the
`x86_64-pc-windows-gnu` target but has never been run on Windows. The Linux
and macOS paths are the maintained ones.

## Fastboot

Not implemented. fastboot does not go through the adb server at all; it talks
to the USB device directly through libusb, so this tunnel cannot carry it as
is.

The planned route is a second service on the host speaking fastboot's own TCP
transport, which the fastboot client already supports via
`fastboot -s tcp:127.0.0.1:5554`. That protocol is small: a four-byte `FB01`
handshake each way, then messages with an eight-byte big-endian length prefix.
A host-side bridge would claim the USB interface with class `0xFF`, subclass
`0x42`, protocol `0x03` and relay messages to its bulk endpoints, carried on a
second ALPN of the same tunnel. The awkward parts are USB re-enumeration when
a phone moves between adb, bootloader and fastbootd, and that
`fastboot devices` does not discover TCP targets, so each device needs its own
known port.

## Development

```bash
make build
make test
make clippy
make check-windows     # needs: rustup target add x86_64-pc-windows-gnu
```

The tunnel can be exercised end to end on one machine against
`iroh-relay --dev`: start a host, note the client's `remoteadb id`, allow it,
connect, and run `adb host-features` through the local port.

## Credit and license

remoteadb is a fork of [pigeons] by [n0], which does the same thing for SSH.
The roost and fly structure, the service installers for all three platforms,
and the key handling come from there; see [NOTICE](NOTICE) for exactly what
was derived and what changed. The fork point is recorded in
`UPSTREAM_PIGEONS_COMMIT`.

The networking is [iroh], also by n0: QUIC with holepunching and relay
fallback, dialing peers by public key.

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option, the same dual license as pigeons. You may pick either one;
you do not have to satisfy both. See [COPYRIGHT](COPYRIGHT) for the summary.

Unless you state otherwise, any contribution you intentionally submit for
inclusion in this work shall be dual licensed as above, without any additional
terms or conditions.

[iroh]: https://github.com/n0-computer/iroh
[iroh-relay]: https://crates.io/crates/iroh-relay
[pigeons]: https://github.com/n0-computer/pigeons
[n0]: https://n0.computer
