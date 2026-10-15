#[cfg(windows)]
use std::mem;
#[cfg(windows)]
use std::ptr;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use winapi::um::winuser::*;
#[cfg(windows)]
use winapi::um::wingdi::*;
#[cfg(windows)]
use tokio::net::{TcpListener, TcpStream};
#[cfg(windows)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(any(windows, test))]
use xsar::tcp::protocol::{FrameHeader, REQUEST_BYTES, stride_from_width};

#[cfg(windows)]
const LISTEN_ADDR: &str = "0.0.0.0:9999";

/// Version 2 uses a u16 stride; reject older framing and nonzero reserved bytes.
#[cfg(any(windows, test))]
fn validate_request(request: &[u8; 5]) -> Result<(), &'static str> {
    if request != &REQUEST_BYTES {
        return Err("Invalid capture request: expected protocol version 2");
    }
    Ok(())
}

/// Convert signed GDI dimensions before calculating the unsigned wire layout.
#[cfg(any(windows, test))]
fn capture_layout(width: i32, height: i32) -> Result<(u32, u32, u16, usize), String> {
    let width = u32::try_from(width).map_err(|_| "Capture width must be positive")?;
    let height = u32::try_from(height).map_err(|_| "Capture height must be positive")?;
    // Four bytes per BGRA pixel: 1920 * 4 = 7680 bytes per row.
    // u16 holds at most 65535 bytes; packed rows reach 65532 bytes at
    // 16383 pixels. A 16384-pixel row needs 65536 bytes and is rejected.
    let stride = stride_from_width(width)?;
    let layout = FrameHeader::new(width, height, stride, 0)?;
    Ok((width, height, stride, layout.pixel_bytes()))
}

/// GDI screen capture: full 1920x1080, top-down opaque BGRA, no cropping.
/// Returns (width, height, stride, pixel_data) or error string.
#[cfg(windows)]
fn capture_screen_gdi() -> Result<(u32, u32, u16, Vec<u8>), String> {
    unsafe {
        // Fixed dimensions for Windows 11 Pro target
        let width: i32 = 1920;
        let height: i32 = 1080;
        let (wire_width, wire_height, stride, pixel_count) = capture_layout(width, height)?;
        // Reserve before acquiring GDI resources; report allocation failures.
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(pixel_count).map_err(|e| e.to_string())?;
        pixels.resize(pixel_count, 0);

        // Get desktop DC
        let hdc_screen = GetDC(ptr::null_mut());
        if hdc_screen.is_null() {
            return Err("GetDC(desktop) failed".to_string());
        }

        // Create compatible DC
        let hdc_compat = CreateCompatibleDC(hdc_screen);
        if hdc_compat.is_null() {
            ReleaseDC(ptr::null_mut(), hdc_screen);
            return Err("CreateCompatibleDC failed".to_string());
        }

        // Create compatible bitmap
        let hbmp = CreateCompatibleBitmap(hdc_screen, width, height);
        if hbmp.is_null() {
            DeleteDC(hdc_compat);
            ReleaseDC(ptr::null_mut(), hdc_screen);
            return Err("CreateCompatibleBitmap failed".to_string());
        }

        // Select bitmap into DC
        let old_bmp = SelectObject(hdc_compat, hbmp as *mut _);

        // BitBlt: copy screen to compatible bitmap
        let blit_ok = BitBlt(
            hdc_compat,
            0,
            0,
            width,
            height,
            hdc_screen,
            0,
            0,
            SRCCOPY,
        );

        if blit_ok == 0 {
            SelectObject(hdc_compat, old_bmp);
            DeleteObject(hbmp as *mut _);
            DeleteDC(hdc_compat);
            ReleaseDC(ptr::null_mut(), hdc_screen);
            return Err("BitBlt failed".to_string());
        }

        // Prepare BITMAPINFO for GetDIBits
        #[repr(C)]
        struct BitmapInfo {
            header: BITMAPINFOHEADER,
            colors: [RGBQUAD; 0], // No color table for 32-bit
        }

        let mut bmi: BitmapInfo = mem::zeroed();
        bmi.header.biSize = mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.header.biWidth = width;
        bmi.header.biHeight = -height; // Negative = top-down DIB
        bmi.header.biPlanes = 1;
        bmi.header.biBitCount = 32; // BI_RGB returns B, G, R, unused byte (BGRX).
        bmi.header.biCompression = BI_RGB;

        // Extract pixels
        let lines_read = GetDIBits(
            hdc_compat,
            hbmp,
            0,
            wire_height,
            pixels.as_mut_ptr() as *mut _,
            &mut bmi.header as *mut _ as *mut BITMAPINFO,
            DIB_RGB_COLORS,
        );

        if lines_read as i32 != height {
            SelectObject(hdc_compat, old_bmp);
            DeleteObject(hbmp as *mut _);
            DeleteDC(hdc_compat);
            ReleaseDC(ptr::null_mut(), hdc_screen);
            return Err(format!(
                "GetDIBits returned {} lines, expected {}",
                lines_read, height
            ));
        }

        // Cleanup
        SelectObject(hdc_compat, old_bmp);
        DeleteObject(hbmp as *mut _);
        DeleteDC(hdc_compat);
        ReleaseDC(ptr::null_mut(), hdc_screen);

        // The unused GDI byte is not alpha. Make it opaque so the wire bytes
        // are B, G, R, A (BGRA), not ARGB or RGBA.
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }

        Ok((wire_width, wire_height, stride, pixels))
    }
}


/// Handle one client connection: read request, capture, send response.
#[cfg(windows)]
async fn handle_client(mut socket: TcpStream, frame_counter: Arc<std::sync::atomic::AtomicU32>) -> Result<(), Box<dyn std::error::Error>> {
    let mut req_buf = [0u8; REQUEST_BYTES.len()];
    socket.read_exact(&mut req_buf).await?;
    validate_request(&req_buf)?;

    // Perform GDI capture
    let (width, height, stride, pixels) = capture_screen_gdi()?;

    // Build response header
    let frame_num = frame_counter.fetch_add(1, Ordering::SeqCst);
    let header = FrameHeader::new(width, height, stride, frame_num)?;

    // Send header
    socket.write_all(&header.to_bytes()).await?;

    // Send pixel data
    socket.write_all(&pixels).await?;
    socket.flush().await?;

    println!(
        "Frame {}: {}x{} stride={} ({} bytes) sent to client",
        frame_num,
        width,
        height,
        stride,
        pixels.len()
    );

    Ok(())
}


/// Main server loop: listen, accept, spawn per-client handler.
#[cfg(windows)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Guest TCP server starting on {}", LISTEN_ADDR);

    let listener = TcpListener::bind(LISTEN_ADDR).await?;
    let frame_counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));

    // Spawn signal handler for graceful shutdown
    let shutdown_clone = shutdown.clone();
    ctrlc::set_handler(move || {
        println!("\nShutdown signal received.");
        shutdown_clone.store(true, Ordering::SeqCst);
    })?;

    println!("Listening for capture requests...");

    loop {
        if shutdown.load(Ordering::SeqCst) {
            println!("Shutting down gracefully.");
            break;
        }

        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            listener.accept(),
        )
        .await
        {
            Ok(Ok((socket, addr))) => {
                println!("Client connected: {}", addr);
                let frame_counter_clone = frame_counter.clone();
                tokio::spawn(async move {
                    match handle_client(socket, frame_counter_clone).await {
                        Ok(()) => println!("Client {} disconnected normally", addr),
                        Err(e) => eprintln!("Client {} error: {}", addr, e),
                    }
                });
            }
            Ok(Err(e)) => eprintln!("Accept error: {}", e),
            Err(_) => {
                // Timeout, check shutdown flag and loop
            }
        }
    }

    Ok(())
}

#[cfg(not(windows))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("Screen capture requires Windows GDI; run this server inside the Windows guest".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_version_2_request_and_rejects_other_framing() {
        assert!(validate_request(&REQUEST_BYTES).is_ok());
        assert!(validate_request(&[0x42, 0x01, 0x01, 0, 0]).is_err());
        assert!(validate_request(&[0x42, 0x02, 0x02, 0, 0]).is_err());
        assert!(validate_request(&[0x42, 0x02, 0x01, 1, 0]).is_err());
    }

    #[test]
    fn gdi_dimensions_produce_the_wire_stride_and_buffer_size() {
        assert_eq!(capture_layout(1920, 1080).unwrap(), (1920, 1080, 7680, 8_294_400));
        assert_eq!(capture_layout(16383, 1).unwrap(), (16383, 1, 65532, 65532));
    }

    #[test]
    fn rejects_negative_zero_and_over_limit_gdi_dimensions() {
        assert!(capture_layout(-1, 1080).is_err());
        assert!(capture_layout(1920, -1).is_err());
        assert!(capture_layout(0, 1080).is_err());
        assert!(capture_layout(1920, 0).is_err());
        assert!(capture_layout(16384, 1080).is_err());
        assert!(capture_layout(i32::MAX, 1080).is_err());
    }
}
