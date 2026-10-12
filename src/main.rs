use std::io::{Read, Write};
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use winapi::um::winuser::*;
use winapi::um::wingdi::*;
use winapi::shared::windef::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LISTEN_ADDR: &str = "0.0.0.0:9999";
const PROTOCOL_MAGIC: u8 = 0x42; // 'B' for BGRA
const PROTOCOL_VERSION: u8 = 0x01;
const CAPTURE_FULL: u8 = 0x01;


/// Protocol request frame (5 bytes)
#[repr(C)]
struct ProtocolRequest {
    magic: u8,
    version: u8,
    command: u8,
    _reserved: [u8; 2],
}


impl ProtocolRequest {
    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < 5 {
            return None;
        }
        Some(ProtocolRequest {
            magic: buf[0],
            version: buf[1],
            command: buf[2],
            _reserved: [buf[3], buf[4]],
        })
    }

    fn validate(&self) -> bool {
        self.magic == PROTOCOL_MAGIC && self.version == PROTOCOL_VERSION
    }
}


/// Protocol response header (16 bytes) + raw BGRA pixel stream
#[repr(C)]
struct ProtocolHeader {
    width: u32,
    height: u32,
    stride: u32,
    frame_number: u32,
}


impl ProtocolHeader {
    fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&self.width.to_le_bytes());
        buf[4..8].copy_from_slice(&self.height.to_le_bytes());
        buf[8..12].copy_from_slice(&self.stride.to_le_bytes());
        buf[12..16].copy_from_slice(&self.frame_number.to_le_bytes());
        buf
    }
}


/// GDI screen capture: full 1920x1080 BGRA, no cropping.
/// Returns (width, height, stride, pixel_data) or error string.
fn capture_screen_gdi() -> Result<(u32, u32, u32, Vec<u8>), String> {
    unsafe {
        // Fixed dimensions for Windows 11 Pro target
        let width: i32 = 1920;
        let height: i32 = 1080;
        let stride: u32 = (width as u32) * 4; // BGRA = 4 bytes/pixel

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
        bmi.header.biBitCount = 32; // 32-bit BGRA
        bmi.header.biCompression = BI_RGB;

        // Allocate pixel buffer
        let pixel_count = (stride as usize) * (height as usize);
        let mut pixels = vec![0u8; pixel_count];

        // Extract pixels
        let lines_read = GetDIBits(
            hdc_compat,
            hbmp,
            0,
            height as u32,
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

        Ok((width as u32, height as u32, stride, pixels))
    }
}


/// Handle one client connection: read request, capture, send response.
async fn handle_client(mut socket: TcpStream, frame_counter: Arc<std::sync::atomic::AtomicU32>) -> Result<(), Box<dyn std::error::Error>> {
    let mut req_buf = [0u8; 5];
    socket.read_exact(&mut req_buf).await?;

    let req = ProtocolRequest::from_bytes(&req_buf)
        .ok_or("Invalid request format")?;

    if !req.validate() {
        return Err("Protocol magic/version mismatch".into());
    }

    if req.command != CAPTURE_FULL {
        return Err(format!("Unknown command: {}", req.command).into());
    }

    // Perform GDI capture
    let (width, height, stride, pixels) = capture_screen_gdi()?;

    // Build response header
    let frame_num = frame_counter.fetch_add(1, Ordering::SeqCst);
    let header = ProtocolHeader {
        width,
        height,
        stride,
        frame_number: frame_num,
    };

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
