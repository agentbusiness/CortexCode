//! The single Ghostty terminal core and exact PTY byte boundary.
//! Shared verbatim with CortexCode. Native calls are serialized by the owner's mutex.
use base64::Engine;
use cc_protocol::contracts::terminal::ScreenRestore;
use std::{ffi::c_void, ptr::NonNull};

const LIMIT: usize = 16 * 1024 * 1024;
unsafe extern "C" {
    fn ghostty_terminal_new(
        allocator: *const c_void,
        out: *mut *mut c_void,
        cols: u16,
        rows: u16,
    ) -> i32;
    fn ghostty_terminal_free(handle: *mut c_void);
    fn ghostty_terminal_set(handle: *mut c_void, option: i32, value: *const c_void) -> i32;
    fn ghostty_terminal_get(handle: *mut c_void, key: i32, out: *mut c_void) -> i32;
    fn ghostty_terminal_vt_write(handle: *mut c_void, bytes: *const u8, len: usize);
    fn ghostty_terminal_resize(
        handle: *mut c_void,
        cols: u16,
        rows: u16,
        width: u32,
        height: u32,
    ) -> i32;
    fn cortex_terminal_read(
        handle: *mut c_void,
        kind: u8,
        bytes: *mut u8,
        capacity: usize,
        written: *mut usize,
    ) -> i32;
}

pub struct ScreenState {
    handle: NonNull<c_void>,
    pub offset: u64,
    valid_size: bool,
}
// Ownership may move between reader/async threads, but every access is under the
// existing terminal mutex. No callback is registered and the core owns no UI object.
unsafe impl Send for ScreenState {}
impl Drop for ScreenState {
    fn drop(&mut self) {
        unsafe { ghostty_terminal_free(self.handle.as_ptr()) }
    }
}
impl ScreenState {
    pub fn new(rows: u16, cols: u16) -> Result<Self, String> {
        let mut raw = std::ptr::null_mut();
        if unsafe { ghostty_terminal_new(std::ptr::null(), &mut raw, cols, rows) } != 0 {
            return Err("Ghostty could not allocate the terminal screen".into());
        }
        let handle = NonNull::new(raw).ok_or("Ghostty returned no terminal screen")?;
        let state = Self {
            handle,
            offset: 0,
            valid_size: true,
        };
        // Numeric keys are pinned to the checked-in native API revision. No history
        // or graphics side effects: this core only supplies the existing text/VT wire.
        state.option(27, &0usize)?; // scrollback_max_bytes
        state.option(31, &65536usize)?; // continuation_max_bytes, before first input
        state.option(15, &0u64)?; // kitty_image_storage_limit
        state.option(16, &false)?; // kitty_image_medium_file
        state.option(18, &false)?; // kitty_image_medium_shared_mem
        if unsafe { ghostty_terminal_set(state.handle.as_ptr(), 17, std::ptr::null()) } != 0 {
            return Err("Ghostty could not disable temporary-file image input".into());
        }
        Ok(state)
    }
    fn option<T>(&self, key: i32, value: &T) -> Result<(), String> {
        let result =
            unsafe { ghostty_terminal_set(self.handle.as_ptr(), key, (value as *const T).cast()) };
        if result == 0 {
            Ok(())
        } else {
            Err(format!("Ghostty option {key} failed ({result})"))
        }
    }
    fn get<T: Default>(&self, key: i32) -> Result<T, String> {
        let mut value = T::default();
        let result = unsafe {
            ghostty_terminal_get(self.handle.as_ptr(), key, (&mut value as *mut T).cast())
        };
        if result == 0 {
            Ok(value)
        } else {
            Err(format!("Ghostty screen query {key} failed ({result})"))
        }
    }
    pub fn process(&mut self, bytes: &[u8]) {
        unsafe { ghostty_terminal_vt_write(self.handle.as_ptr(), bytes.as_ptr(), bytes.len()) };
        self.offset = self.offset.saturating_add(bytes.len() as u64);
    }
    pub fn set_size(&mut self, rows: u16, cols: u16) -> Result<(), String> {
        let result = unsafe { ghostty_terminal_resize(self.handle.as_ptr(), cols, rows, 0, 0) };
        self.valid_size = result == 0;
        if result == 0 {
            Ok(())
        } else {
            Err(format!(
                "Ghostty resize failed ({result}); screen geometry is unavailable until a successful resize"
            ))
        }
    }
    fn read(&self, kind: u8) -> Result<Vec<u8>, String> {
        if !self.valid_size {
            return Err("Ghostty screen geometry is unavailable after a failed resize".into());
        }
        let mut bytes = vec![0; LIMIT];
        let mut written = 0;
        let result = unsafe {
            cortex_terminal_read(
                self.handle.as_ptr(),
                kind,
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut written,
            )
        };
        if result != 0 || written > bytes.len() {
            return Err(
                "Ghostty snapshot exceeds its limit or cannot preserve terminal state".into(),
            );
        }
        bytes.truncate(written);
        Ok(bytes)
    }
    pub fn screen(&self) -> Result<ScreenView, String> {
        Ok(ScreenView {
            text: String::from_utf8(self.read(0)?)
                .map_err(|_| "Ghostty returned invalid screen text")?,
            rows: self.get(2)?,
            cols: self.get(1)?,
            row: self.get(4)?,
            col: self.get(3)?,
            alternate: self.get::<i32>(6)? == 1,
        })
    }
    pub fn restore(&self) -> Result<ScreenRestore, String> {
        let bytes = self.read(1)?;
        Ok(ScreenRestore {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            offset: self.offset,
        })
    }
}
pub struct ScreenView {
    text: String,
    rows: u16,
    cols: u16,
    row: u16,
    col: u16,
    alternate: bool,
}
impl ScreenView {
    pub fn contents(&self) -> String {
        self.text.clone()
    }
    pub fn cursor_position(&self) -> (u16, u16) {
        (self.row, self.col)
    }
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }
    pub fn alternate_screen(&self) -> bool {
        self.alternate
    }
}
