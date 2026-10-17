# Native Windows verification

The server runs manually in PowerShell inside the Windows 11 QEMU guest.
The existing source directories are `C:\Dev\qemu-screen-grab-server` and
`C:\Dev\xsar`. Keep them as siblings for the manifest's `../xsar` dependency.
Extract the reviewed source bundle into a fresh parent such as
`C:\Dev\capture-server-0.1.3`, retaining the two sibling names. This leaves the
previous source and executable untouched. The verifier refuses unlisted files
and source symlinks/junctions; it never deletes unrelated files to make a check
pass. Its only additional inventory exemption is a correctly named, regular
runtime log directly in the server root. Nested logs and arbitrary `.log`
filenames remain unlisted files; retained logs are never source inputs.

The reviewed set is server 0.1.3, xSAR 0.4.2-candidate.2 and Arch application
2.2.2-candidate.13. The source bundle includes `SOURCE-MANIFEST.json`; the
verification script checks its complete file hashes before compiling. A changed
source requires a new reviewed manifest, not bypassing the check.

## Build without replacing the running executable

1. From PowerShell, run the script in the reviewed server checkout. It requires
   an already installed nightly toolchain and makes no toolchain upgrade.
   The example uses the separate candidate directory; your existing checkouts
   remain at the paths above.

   ```powershell
   Write-Host 'Verify the reviewed sibling sources and build server 0.1.3 separately'
   Set-Location -LiteralPath 'C:\Dev\capture-server-0.1.3\qemu-screen-grab-server' -ErrorAction Stop
   .\scripts\verify-windows.ps1
   ```

   The script checks hashes, records compiler/Cargo versions, runs locked
   all-target check, tests including ignored tests, strict private rustdoc,
   all-target Clippy and release build with the installed Windows host target.
   It checks source inventory/hashes before and after, and saves individual logs and a JSON
   result containing the selected executable and its SHA-256. Its default
   target directory is `target\capture-0.1.3`, separate from `target\release`.
   It does not launch, install or stop any server.

2. Retain the verification result and all logs, including the first failure.
   Confirm the Arch application build is ready too. Record the full path and
   hash of the old executable before any later deployment decision. Compilation
   does not prove GetDIBits behaviour or replace the running process.

## Live checks after separate deployment approval

Use the interactive primary desktop at 1920x1080, display/text scale 100% and
the existing supported artwork. Launch the selected new executable manually
in PowerShell, as before, only after the old listener has been deliberately
stopped. Confirm the startup version and the automatically created log path.

## Automatic runtime logs

Each launch creates a log in the PowerShell working directory, independently of
the executable's location. To keep logs beside the reviewed server sources,
launch from `C:\Dev\capture-server-0.1.3\qemu-screen-grab-server`.
The local Windows date/time filename is
`qemu-screen-grab-server-YYYYMMDD-HHMMSS-mmm-pidPID.log`, with a three-digit
collision suffix `-001` through `-099` before `.log` when needed. Exclusive
creation never overwrites an earlier log; the base name and 99 suffixes give
at most 100 attempts. Local clock time names the file; existing monotonic
durations remain the timing evidence.

Server output is mirrored to the console and log, including request outcomes,
statistics, errors and shutdown feedback. Every complete line is flushed, and
shutdown explicitly flushes its final output. This is not an `fsync` or a
guarantee against power loss. Logs remain after exit, and later launches do not
delete or rotate them. Retain each original file alongside the native test
evidence; plan disk space for sustained runs.

An initial log creation failure reports its path/error and refuses startup
before TCP bind. A later file write failure reports to stderr and requests
cooperative shutdown. Preserve that console error separately: the affected
log cannot be assumed complete. In-flight synchronous GDI may still delay
shutdown. Synchronous logging can delay its caller on slow storage; do not
attribute that delay to a measured GDI stage.

Verify that an ordinary run writes startup, request summaries and completed
Ctrl+C shutdown to both destinations. Relaunch and confirm the previous file
is unchanged and a distinct file is retained. Exercise creation refusal from
an unwritable working directory and confirm the server never starts listening.
The portable logger tests cover collision handling and later write failure;
record native observations separately. Re-running the source verifier may
retain correctly named root runtime logs, while an unrelated unlisted file
must still be refused.

## Capture and shutdown acceptance

Verify v2, one-frame v3 and persistent v3 separately against the matching Arch
client. Each successful frame must have the full native dimensions and expected
raw colours. Version 2 sends 8,294,400 pixel bytes; version 3 sends 6,220,800 and
reconstructs opaque RGBA on Arch. Subsequent persistent requests must receive
fresh increasing capture IDs rather than a cached image. No crop or compression
is part of this package.

Check manual capture and PNG retention; client close/reconnect; malformed and
partial requests; bounded overload and stalled readers; loss of the interactive
desktop or incompatible dimensions; and Ctrl+C while idle and during capture.
Observe GDI resource counts across repeated capture requests. A synchronous
GDI call cannot be forcibly interrupted, so record any delayed shutdown honestly.
Native failure/cleanup paths need native evidence; portable mocks cannot prove
Win32 resource release.

Ctrl+C must print `Shutdown signal received.` and `Shutting down gracefully.`
once, followed by the final `stats cause=shutdown` summary and then
`Shutdown complete.` after the capture thread has joined. If a synchronous GDI
call is still running, completion must wait for it; a join/owner error must not
print completion. Check idle shutdown and shutdown during repeated captures,
retaining the complete console output and automatic log. Server 0.1.3 includes
the shutdown feedback correction and retains the existing capture protocol;
identify this source revision with the reviewed bundle hashes as well as the
version string.

Collect guest/host stage summaries for comparable samples in all three wire
modes. Keep `--image` settings identical and report PNG cost separately. Then
complete all five controller GAME WIN acceptance sequences and preserve their
original evidence. Read the Arch application's combined capture contract for
matching, measurement and rollback boundaries.
