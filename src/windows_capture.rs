//! Corrected per-request compatible-bitmap GDI acquisition.
//!
//! This deliberately retains allocation/setup and GetDIBits on every request.
//! There is no DIB-section experiment, capture cache or resource reuse here.

use crate::server::{
    Capture, CaptureStages, Captured, Failure, HEIGHT, PixelMode, WIDTH, prepare_pixels,
};
use std::{mem, ptr, time::Instant};
use winapi::{
    shared::windef::{HBITMAP, HDC, HGDIOBJ},
    um::{wingdi::*, winuser::*},
};
use xsar::tcp::protocol::v3::Status;


/// Stateless provider; all GDI resources are created and released on its owner thread.
pub(crate) struct GdiCapture;


/// Checked ownership of desktop DC, memory DC and one compatible bitmap.
struct Resources {
    /// Desktop DC released with ReleaseDC, never DeleteDC.
    screen: HDC,
    /// Compatible memory DC released with DeleteDC.
    memory: HDC,
    /// Per-request bitmap, deleted only after deselection or DC destruction.
    bitmap: HBITMAP,
    /// Original memory-DC bitmap, restored before readback and cleanup.
    original: HGDIOBJ,
    /// True only while our bitmap is selected into the compatible DC.
    selected: bool,
}


impl Resources {
    /// Acquire handles incrementally; Drop owns every successful acquisition.
    fn new() -> Result<Self, Failure> {
        let mut resources = Self {
            screen: ptr::null_mut(),
            memory: ptr::null_mut(),
            bitmap: ptr::null_mut(),
            original: ptr::null_mut(),
            selected: false,
        };
        // SAFETY: null denotes the desktop; returned handles are checked and owned
        // by this guard on the same OS thread until their matching release call.
        unsafe {
            resources.screen = GetDC(ptr::null_mut());
            if resources.screen.is_null() {
                return Err(failed("GetDC(desktop) failed"));
            }
            resources.memory = CreateCompatibleDC(resources.screen);
            if resources.memory.is_null() {
                return Err(failed("CreateCompatibleDC failed"));
            }
            resources.bitmap =
                CreateCompatibleBitmap(resources.screen, WIDTH as i32, HEIGHT as i32);
            if resources.bitmap.is_null() {
                return Err(failed("CreateCompatibleBitmap failed"));
            }
            resources.original = SelectObject(resources.memory, resources.bitmap.cast());
            if invalid_object(resources.original) {
                return Err(failed("SelectObject(capture bitmap) failed"));
            }
            resources.selected = true;
        }
        Ok(resources)
    }


    /// Restore and check the original selection before GetDIBits touches the bitmap.
    fn deselect(&mut self) -> Result<(), Failure> {
        if self.selected {
            // SAFETY: both handles are valid and belong to this thread; the
            // original object came from the successful matching SelectObject.
            let replaced = unsafe { SelectObject(self.memory, self.original) };
            if invalid_object(replaced) {
                return Err(failed("SelectObject(original bitmap) failed"));
            }
            self.selected = false;
            if replaced != self.bitmap.cast() {
                return Err(failed("unexpected capture bitmap selection"));
            }
        }
        Ok(())
    }


    /// Release every acquired handle, preserving the first cleanup failure.
    fn cleanup(&mut self) -> Result<(), Failure> {
        let mut error = self.deselect().err();
        // SAFETY: guards prevent duplicate releases. If deselection failed,
        // destroy the owning DC first to release the bitmap's selection.
        unsafe {
            if self.selected && !self.memory.is_null() {
                if DeleteDC(self.memory) == 0 {
                    error.get_or_insert(failed("DeleteDC after failed deselection failed"));
                } else {
                    self.memory = ptr::null_mut();
                    self.selected = false;
                }
            }
            if !self.memory.is_null() {
                if DeleteDC(self.memory) == 0 {
                    error.get_or_insert(failed("DeleteDC failed"));
                } else {
                    self.memory = ptr::null_mut();
                    self.selected = false;
                }
            }
            if !self.bitmap.is_null() && !self.selected {
                if DeleteObject(self.bitmap.cast()) == 0 {
                    error.get_or_insert(failed("DeleteObject(capture bitmap) failed"));
                } else {
                    self.bitmap = ptr::null_mut();
                }
            }
            if !self.screen.is_null() {
                if ReleaseDC(ptr::null_mut(), self.screen) == 0 {
                    error.get_or_insert(failed("ReleaseDC(desktop) failed"));
                } else {
                    self.screen = ptr::null_mut();
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}


impl Drop for Resources {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            crate::logging::error!(
                "GDI cleanup outcome={:?} reason={}",
                error.status,
                error.reason
            );
        }
    }
}


/// Both Win32 object failure representations are refused before handle reuse.
fn invalid_object(object: HGDIOBJ) -> bool {
    object.is_null() || object == HGDI_ERROR
}


/// Capture failures use a fixed status and static diagnostic.
fn failed(reason: &'static str) -> Failure {
    Failure::new(Status::CaptureFailed, reason)
}


impl Capture for GdiCapture {
    fn capture(&mut self, mode: PixelMode) -> Result<Captured, Failure> {
        let started = Instant::now();
        // SAFETY: read-only system metrics require no owned resources. Every
        // acquisition checks the active primary desktop dimensions independently.
        let dimensions = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if dimensions != (WIDTH as i32, HEIGHT as i32) {
            return Err(Failure::new(
                Status::IncompatibleDisplay,
                "active desktop must be 1920x1080",
            ));
        }
        let mut pixels = Vec::new();
        let bytes = PixelMode::LegacyBgra8.bytes();
        pixels
            .try_reserve_exact(bytes)
            .map_err(|_| failed("capture pixel allocation failed"))?;
        pixels.resize(bytes, 0);
        let mut resources = Resources::new()?;
        let allocation_setup = started.elapsed();
        let bitblt_started = Instant::now();
        // SAFETY: valid checked DCs, selected bitmap and fixed positive native
        // dimensions; no pointer to pixels is exposed during the drawing call.
        let copied = unsafe {
            BitBlt(
                resources.memory,
                0,
                0,
                WIDTH as i32,
                HEIGHT as i32,
                resources.screen,
                0,
                0,
                SRCCOPY,
            )
        };
        let bitblt = bitblt_started.elapsed();
        if copied == 0 {
            return Err(failed("BitBlt failed"));
        }
        resources.deselect()?;

        // SAFETY: BITMAPINFO is a plain Win32 structure; zeroed optional fields
        // are valid for this explicitly configured top-down 32-bit BI_RGB DIB.
        let mut info: BITMAPINFO = unsafe { mem::zeroed() };
        info.bmiHeader.biSize = mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = WIDTH as i32;
        info.bmiHeader.biHeight = -(HEIGHT as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let readback_started = Instant::now();
        // SAFETY: the bitmap is deselected. The destination owns exactly the
        // complete 32-bit top-down storage; DWORD alignment is inherent in width*4.
        let lines = unsafe {
            GetDIBits(
                resources.screen,
                resources.bitmap,
                0,
                HEIGHT,
                pixels.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        let readback = readback_started.elapsed();
        if lines != HEIGHT as i32 {
            return Err(failed("GetDIBits scan-line count mismatch"));
        }
        resources.cleanup()?;
        let packing_started = Instant::now();
        prepare_pixels(&mut pixels, mode)?;
        Ok(Captured {
            pixels,
            stages: CaptureStages {
                allocation_setup,
                bitblt,
                readback,
                packing: packing_started.elapsed(),
                total: started.elapsed(),
            },
        })
    }
}
