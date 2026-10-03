# SSH transport feasibility — 2 October 2026

Scope: native Swift dependency/client proof, not an iOS app, Pika endpoint,
provider integration, physical-device test, or Tailscale network test.

## Result

Citadel 0.12.1 is a viable candidate for ordinary SSH. Actual OpenSSH loopback
proof exited zero:

```text
Build of product 'TransportSpike' complete! (2.57s)
PASS OpenSSH imported-key pinned-host handshake; elapsed=0.046889708 seconds
PASS OpenSSH sustained split/coalesced three-frame bidirectional non-PTY exec
PASS explicit disconnect
PASS mismatch rejected specifically as InvalidHostKey
PASS valid pinned-host reconnect immediately after mismatch
```

Evidence tool session: 54769, completed exit 0. The endpoint forcibly ran
`/bin/cat`; it did not execute Pika or arbitrary requested shell commands. The
three schema-1 fixture frames were returned byte-for-byte after split/coalesced
writes on one exec session. This proves transport I/O, not protocol semantics,
subscription delivery, server authorisation, or production frame backpressure.

Citadel dependency target cross-compiled successfully for arm64 iOS17 deployment
with Xcode 27.0 (27A266a), iPhoneOS27.0 SDK:

```text
Build complete! (29.59 sec)
```

Evidence tool session: 58650, completed exit 0. Compiled module:
`/tmp/pika-ios-transport.b4aP3S/ios-build/out/Products/Debug-iphoneos/Citadel.swiftmodule/arm64-apple-ios.swiftmodule`.
This is dependency compilation only; no signed app, Simulator, phone, Keychain,
keyboard or lifecycle journey was exercised. BigInt's old watchOS deployment
manifest emitted a deprecation warning. This was not a warnings-as-errors gate.

Initial macOS dependency build: Swift 6.3.2, completed 77.86s. Debug fixture
executable: 22,165,856 bytes, including its server fixture and debug information.
This is NOT iOS package/download-size evidence or a release performance budget.

Cleanup verified: listener PID92703 stopped with TERM; subsequent process
inventory showed no owned SSH/spike process and `lsof -nP -iTCP:59273
-sTCP:LISTEN` returned no listener. Scratch build outputs/new disposable keys are
retained under the explicit temp directory for evidence; no installed service
or user configuration was changed. The spike has a 15-second watchdog now.

## Failed alternative retained honestly

Citadel's own ephemeral server accepted pinned-host/password authentication
(observed 11.95ms) and received exec, but its stdin pipe readability callback
never fired. The client/server fixture stalled; its owned processes were stopped.
It is not a passing exec test. Testing against independently implemented OpenSSH
then passed. The two-argument executable mode retains that bounded diagnostic;
normal four-argument proof uses OpenSSH. Password against OpenSSH, encrypted-key
import, denied login and cancellation remain unverified.

## Exact reproduction of the passing path

All generated keys, authorised-key file, host key, config, HOME/XDG directories,
Swift cache/config/security paths and build scratch were under the new directory
`/tmp/pika-ios-transport.b4aP3S`. No existing SSH configuration, user key or real
credential was read or changed. No fleet machine/provider/model was contacted.

Generate fresh disposable Ed25519 client and server keys with `ssh-keygen -q
-t ed25519 -N '' -f PATH`. Use this explicit disposable sshd configuration:

```text
Port 59273
ListenAddress 127.0.0.1
HostKey /tmp/pika-ios-transport.b4aP3S/sshd-host-key
PidFile /tmp/pika-ios-transport.b4aP3S/sshd.pid
AuthorizedKeysFile /tmp/pika-ios-transport.b4aP3S/fixture-key.pub
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
AllowUsers ayushjain
AllowTcpForwarding no
X11Forwarding no
PermitTunnel no
PermitTTY no
SetEnv HOME=/tmp/pika-ios-transport.b4aP3S XDG_CONFIG_HOME=/tmp/pika-ios-transport.b4aP3S/config XDG_CACHE_HOME=/tmp/pika-ios-transport.b4aP3S/cache
ForceCommand /bin/cat
```

The local username is only the existing OS account executing the fixture. Keys
are new throwaway bytes, not credentials for its installed SSH service. Start
`/usr/sbin/sshd -D -e -f /tmp/pika-ios-transport.b4aP3S/sshd_config` (tool session
45589), then from this package directory:

```sh
env HOME=/tmp/pika-ios-transport.b4aP3S \
  XDG_CONFIG_HOME=/tmp/pika-ios-transport.b4aP3S/config \
  XDG_CACHE_HOME=/tmp/pika-ios-transport.b4aP3S/cache \
  swift run --scratch-path /tmp/pika-ios-transport.b4aP3S/build \
  --cache-path /tmp/pika-ios-transport.b4aP3S/swift-cache \
  --config-path /tmp/pika-ios-transport.b4aP3S/swift-config \
  --security-path /tmp/pika-ios-transport.b4aP3S/swift-security \
  TransportSpike /tmp/pika-ios-transport.b4aP3S/fixture-key \
  /tmp/pika-ios-transport.b4aP3S/sshd-host-key.pub 59273 ayushjain
```

For the dependency-only iOS compile, same environment and package directory:

```sh
env HOME=/tmp/pika-ios-transport.b4aP3S \
  XDG_CONFIG_HOME=/tmp/pika-ios-transport.b4aP3S/config \
  XDG_CACHE_HOME=/tmp/pika-ios-transport.b4aP3S/cache \
  DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  swift build --target Citadel --triple arm64-apple-ios17.0 \
  --sdk /Applications/Xcode.app/Contents/Developer/Platforms/iPhoneOS.platform/Developer/SDKs/iPhoneOS27.0.sdk \
  --scratch-path /tmp/pika-ios-transport.b4aP3S/ios-build \
  --cache-path /tmp/pika-ios-transport.b4aP3S/swift-cache \
  --config-path /tmp/pika-ios-transport.b4aP3S/swift-config \
  --security-path /tmp/pika-ios-transport.b4aP3S/swift-security
```

For a new run, choose a new `mktemp -d` directory and unused loopback port,
substitute those paths throughout, and stop only the listener/process created
for that run. Do not reuse fixture-only `StrictModes no` in production.

## Implementable route and remaining gates

- Address first: ordinary Tailscale provides reachability; collect username and
  guided password or explicit Files key import. Do not claim Tailscale SSH web/
  policy authentication works with this client. Do not embed another VPN.
- Citadel provides `.passwordBased`, Ed25519/RSA/P256/P384/P521 methods and
  OpenSSH Ed25519/RSA parsers with passphrase inputs. Only unencrypted Ed25519
  import/authentication was proved here. Enumerate unsupported formats honestly.
- A custom host-key validator can surface a SHA256 fingerprint for independent
  user verification before authentication. Persist only explicitly verified
  host bytes; `.trustedKeys` fails closed on changes. Never use acceptAnything.
  Verify exact Pika node identity separately before saving an authorised machine.
- Save credential bytes with iOS Security/Keychain APIs, non-synchronising
  `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`; connection metadata/cache is
  not a credential store. No Keychain write was performed by this spike.
- Open one non-PTY exec with a fixed versioned Pika stdio command; screens share
  its connection/feed. The command used here is illustrative and is not an
  implemented Rust endpoint. Add bounded incremental framing/backpressure,
  identity/version handshake, foreground reconnect and explicit cancellation.
  Citadel stream internals use unbounded AsyncThrowingStream buffering: audit
  or constrain upstream buffering before treating app-level frame bounds as a
  production memory ceiling. Unknown writes must never automatically replay.
- Citadel 0.12.1 targets iOS17/macOS14 and uses the Wellz26 NIOSSH fork (resolved
  0.3.7), NIO2.103.0, Crypto3.15.1 and other transitive packages recorded in
  Package.resolved. Pin/audit licences, cryptography and maintenance before
  shipping; prefer Apple-only NIOSSH if reducing fork risk outweighs implementing
  key import and higher-level exec/auth plumbing. Do not enable deprecated
  algorithms through SSHAlgorithms.all.

Primary sources: [Citadel package manifest](https://github.com/orlandos-nl/Citadel/blob/0.12.1/Package.swift),
[withExec implementation](https://github.com/orlandos-nl/Citadel/blob/0.12.1/Sources/Citadel/TTY/Client/TTY.swift),
[host-key validator](https://github.com/orlandos-nl/Citadel/blob/0.12.1/Sources/Citadel/ClientSession.swift),
[key import](https://github.com/orlandos-nl/Citadel/blob/0.12.1/Sources/Citadel/SSHCert.swift).
