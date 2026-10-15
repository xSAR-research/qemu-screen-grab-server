# Changelog

## Unreleased

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

