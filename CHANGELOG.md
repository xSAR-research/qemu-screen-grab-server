# Changelog

## Unreleased

- Version 0.1.3 adds automatic per-run logging in the launch working directory.
  Use a local date/time, millisecond and process identifier in the filename;
  preserve existing logs with exclusive creation and at most 100 collision
  attempts. Mirror server output to the console and flush every completed line.
- Refuse startup if the log cannot be created. Report later log write failures
  on stderr and request cooperative shutdown. Retain logs without automatic
  deletion or rotation; flushing is not a filesystem durability guarantee.
- Update the native verifier for 0.1.3 and allow only correctly named regular
  runtime logs directly in the server root, retaining strict source inventory
  and hash checks for other files.
- Restore Ctrl+C shutdown feedback and report completion only after client
  handlers drain, the capture owner joins successfully and final statistics print.
  Cover delayed capture cleanup and owner failure with portable regressions.
- Version 0.1.2 pairs with xSAR 0.4.2-candidate.2 and application candidate.13.
  Add full-screen version-3 BGR8 responses and repeated fresh requests on one
  connection, while retaining exact legacy v2 requests for explicit comparison.
- Correct bitmap deselection before GetDIBits and check selection/restoration
  failures. Keep per-request GDI resources on one capture owner thread.
- Bound admission, queued work, response ownership and network/queue deadlines;
  add guest stage timings and fixed-size rolling summaries. Synchronous GDI
  cannot be forcibly cancelled by these network deadlines.
- Add portable fake-capture lifecycle tests. Windows compilation and native GDI
  tests are performed separately by the operator inside the Windows 11 VM; see
  [Windows verification](docs/verification-windows.md).

- Use shared xSAR TCP protocol version 2 with a packed 14-byte header and
  `u16` stride; rebuild the host client and guest server together.
- Calculate stride with checked conversion: 1920 pixels * 4 bytes = 7680 bytes.
  `u16` holds 65535, limiting complete BGRA rows to 65532 bytes / 16383 pixels.
- Normalize GDI's unused fourth byte to opaque alpha: wire pixels are BGRA,
  and the client converts them to RGBA. Neither byte layout is ARGB.
- Reject invalid dimensions, row lengths and frame sizes; test signed-input
  boundaries, exact header serialization and rejection of old requests.

## v0.1.0

Initial code build, needs fixes for:

- u32/i32 being a mismatch for the stride size passed over the TCP socket.
- When the client disconnects, we need to make sure to release the shared memory and close the handle to the window.
- We should look into using authentication, like a shared secret or something.
- Better error handling.
- Ability to handle sub-set returns rather than a full image (1920x1080 pixels) as only a section of the screen is used for CV. This will dramatically cut down on the transport size/time.
- Move the screen-grabber to a service, that is always running on boot.
- Ensure shared memory is always cleaned up on exit.
- Ensure shared memory is cleaned up on disconnect.

