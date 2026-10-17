# QEMU screen grab server

Version **0.1.3** captures the interactive Windows desktop and returns fresh
1920 × 1080 frames over TCP for xSAR and qmp-qemu-socket. It runs manually inside
the Windows guest. Protocol v3 sends tightly packed, top-down BGR8 and supports
successive requests on one connection; explicit legacy v2 remains available
for comparison. Capture uses GDI on one dedicated thread.

See [Windows build and verification](docs/verification-windows.md) for reviewed
sibling-source builds, separate executable output and native acceptance checks.
Compilation and portable tests do not establish native GDI behaviour or live
game acceptance.

## Automatic logs

Each launch creates a new log in its working directory:

```text
qemu-screen-grab-server-YYYYMMDD-HHMMSS-mmm-pidPID.log
```

The filename uses Windows local time. Exclusive creation preserves existing
files, trying suffixes `-001` through `-099` when needed. Server output is
mirrored to the console and file, including shutdown feedback. Each complete
line is flushed; this does not guarantee power-loss durability.

Log creation failure refuses startup. A later file write failure reports to
stderr and requests cooperative shutdown; retain that console error because
the file may be incomplete. Logs are neither rotated nor removed automatically,
and their total disk use can grow without a lifetime limit. Check disk space
for sustained runs. Synchronous log writes can delay their caller on slow storage.

## Security and network exposure

The Windows server binds `0.0.0.0:9999`, which means all guest IPv4 interfaces.
**This is a bind address, not a client allowlist.** Actual reachability depends
on guest networking, forwarding and firewall rules.

In the inspected Beast reference configuration, `win11-nr` calls `win11-pro`,
which calls `qemu_win11_run`. Its capture forwarding fragment is:

```text
hostfwd=tcp:127.0.0.1:9999-10.0.2.15:9999
```

That rule binds the host forwarding listener to `127.0.0.1:9999` and forwards
it to guest `10.0.2.15:9999`. The inspected host listener agrees. This specific
forward does not listen on the `192.168.0.0/24` LAN. QEMU documents that an
explicit `hostaddr` selects the host interface for
[host forwarding](https://www.qemu.org/docs/master/system/invocation.html).
This is a reference configuration, not a guarantee for another deployment;
additional forwards, proxies or guest interfaces may change exposure.

The capture protocol has **no authentication, TLS or per-client rate limit**.
A reachable client can obtain current desktop pixels. It can also repeatedly
request captures, occupy the two client slots, and consume CPU, bandwidth and
log disk space. A denial-of-service (DoS) attack can come from one source;
distributed denial of service (DDoS) uses multiple sources. Resource exhaustion
can deny legitimate clients access; see
[MITRE CWE-400](https://cwe.mitre.org/data/definitions/400.html).

Two client slots, one executing plus one queued capture, two response leases
and deadlines bound concurrent work. They do not guarantee resistance to
flooding or sustained resource consumption. Synchronous GDI cannot be forcibly
cancelled by those deadlines, and logging has no automatic disk retention cap.
No flooding or denial-of-service testing was performed for this documentation.

Keep host forwarding on loopback and use firewall rules to restrict other
reachable paths to trusted clients. Do not expose the listener to the Internet
or broaden the forward to the LAN without a separate security design. Recheck
the actual launcher arguments, active listener and firewall rules before using
a different network configuration.
