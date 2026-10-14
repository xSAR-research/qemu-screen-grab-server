# Changelog

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

