use std::time::Duration;

use serde_json::{json, Value};

use crate::string_argument;

#[cfg(all(target_os = "windows", not(test)))]
use crate::{base64_encode, path_guard, workspace_root};
#[cfg(all(target_os = "windows", not(test)))]
use std::fs;
#[cfg(all(target_os = "windows", not(test)))]
use uuid::Uuid;

#[cfg(all(target_os = "windows", not(test)))]
#[repr(C)]
struct WinPoint {
    x: i32,
    y: i32,
}

#[cfg(all(target_os = "windows", not(test)))]
#[link(name = "user32")]
unsafe extern "system" {
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetCursorPos(point: *mut WinPoint) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn mouse_event(flags: u32, x: u32, y: u32, data: u32, extra_info: usize);
    fn keybd_event(virtual_key: u8, scan_code: u8, flags: u32, extra_info: usize);
    fn VkKeyScanW(character: u16) -> i16;
}

#[cfg(all(target_os = "windows", not(test)))]
#[repr(C)]
struct WinBitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    clr_used: u32,
    clr_important: u32,
}

#[cfg(all(target_os = "windows", not(test)))]
#[repr(C)]
struct WinBitmapInfo {
    header: WinBitmapInfoHeader,
    colors: [u32; 1],
}

#[cfg(all(target_os = "windows", not(test)))]
#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(device: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CreateCompatibleBitmap(
        device: *mut std::ffi::c_void,
        width: i32,
        height: i32,
    ) -> *mut std::ffi::c_void;
    fn SelectObject(
        device: *mut std::ffi::c_void,
        object: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn BitBlt(
        destination: *mut std::ffi::c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        source: *mut std::ffi::c_void,
        source_x: i32,
        source_y: i32,
        operation: u32,
    ) -> i32;
    fn GetDIBits(
        device: *mut std::ffi::c_void,
        bitmap: *mut std::ffi::c_void,
        start_scan: u32,
        scan_lines: u32,
        bits: *mut std::ffi::c_void,
        info: *mut WinBitmapInfo,
        usage: u32,
    ) -> i32;
    fn DeleteObject(object: *mut std::ffi::c_void) -> i32;
    fn DeleteDC(device: *mut std::ffi::c_void) -> i32;
}

#[cfg(all(target_os = "windows", not(test)))]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetDC(window: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn ReleaseDC(window: *mut std::ffi::c_void, device: *mut std::ffi::c_void) -> i32;
}

#[cfg(all(target_os = "windows", not(test)))]
fn windows_key_code(key: &str) -> Option<u8> {
    let normalized = key.to_lowercase();
    let code = match normalized.as_str() {
        "enter" => 0x0D,
        "escape" | "esc" => 0x1B,
        "tab" => 0x09,
        "backspace" => 0x08,
        "space" => 0x20,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "home" => 0x24,
        "end" => 0x23,
        "delete" => 0x2E,
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f7" => 0x76,
        "f8" => 0x77,
        "f9" => 0x78,
        "f10" => 0x79,
        "f11" => 0x7A,
        "f12" => 0x7B,
        _ if normalized.len() == 1 => return normalized.as_bytes().first().copied(),
        _ => return None,
    };
    Some(code)
}

#[cfg(all(target_os = "windows", not(test)))]
fn capture_screen_bmp() -> Result<(i32, i32, Vec<u8>), String> {
    const SRCCOPY: u32 = 0x00CC0020;
    const DIB_RGB_COLORS: u32 = 0;
    let width = unsafe { GetSystemMetrics(0) };
    let height = unsafe { GetSystemMetrics(1) };
    if width <= 0 || height <= 0 {
        return Err("Windows returned an invalid screen size.".to_string());
    }
    let screen = unsafe { GetDC(std::ptr::null_mut()) };
    if screen.is_null() {
        return Err("Unable to acquire the Windows screen device context.".to_string());
    }
    let memory = unsafe { CreateCompatibleDC(screen) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    if memory.is_null() || bitmap.is_null() {
        if !bitmap.is_null() {
            unsafe { DeleteObject(bitmap) };
        }
        if !memory.is_null() {
            unsafe { DeleteDC(memory) };
        }
        unsafe { ReleaseDC(std::ptr::null_mut(), screen) };
        return Err("Unable to allocate a Windows screen bitmap.".to_string());
    }
    unsafe {
        SelectObject(memory, bitmap);
    }
    let copied = unsafe { BitBlt(memory, 0, 0, width, height, screen, 0, 0, SRCCOPY) };
    if copied == 0 {
        unsafe {
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
        return Err("Windows BitBlt failed while capturing the screen.".to_string());
    }
    let stride = (width as usize * 3).div_ceil(4) * 4;
    let mut pixels = vec![0u8; stride * height as usize];
    let mut info = WinBitmapInfo {
        header: WinBitmapInfoHeader {
            size: std::mem::size_of::<WinBitmapInfoHeader>() as u32,
            width,
            height: -height,
            planes: 1,
            bit_count: 24,
            compression: 0,
            size_image: pixels.len() as u32,
            x_pels_per_meter: 0,
            y_pels_per_meter: 0,
            clr_used: 0,
            clr_important: 0,
        },
        colors: [0],
    };
    let scan_lines = unsafe {
        GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            pixels.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
    }
    if scan_lines == 0 {
        return Err("Windows GetDIBits failed while capturing the screen.".to_string());
    }
    let mut bmp = Vec::with_capacity(14 + 40 + pixels.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + 40 + pixels.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(54u32).to_le_bytes());
    bmp.extend_from_slice(&(40u32).to_le_bytes());
    bmp.extend_from_slice(&width.to_le_bytes());
    bmp.extend_from_slice(&(-height).to_le_bytes());
    bmp.extend_from_slice(&(1u16).to_le_bytes());
    bmp.extend_from_slice(&(24u16).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 16]);
    bmp.extend_from_slice(&pixels);
    Ok((width, height, bmp))
}

#[cfg(all(target_os = "windows", not(test)))]
fn computer_snapshot(arguments: &Value, external_path_approved: bool) -> Result<String, String> {
    let mut point = WinPoint { x: 0, y: 0 };
    let cursor_ok = unsafe { GetCursorPos(&mut point) != 0 };
    let (width, height, bmp) = capture_screen_bmp()?;
    let requested_path = string_argument(arguments, "path").unwrap_or_else(|| {
        workspace_root()
            .join(".rustpilot")
            .join(format!("screen-{}.bmp", Uuid::new_v4()))
            .display()
            .to_string()
    });
    let path = path_guard::resolve_mutation_path(
        &workspace_root(),
        &requested_path,
        external_path_approved,
    )?
    .canonical;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Unable to create screenshot directory: {error}"))?;
    }
    fs::write(&path, &bmp).map_err(|error| format!("Unable to write screenshot: {error}"))?;
    let include_base64 = arguments
        .get("include_base64")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    serde_json::to_string_pretty(&json!({
        "screen_width": width,
        "screen_height": height,
        "cursor": if cursor_ok { json!({"x": point.x, "y": point.y}) } else { Value::Null },
        "screenshot_available": true,
        "path": path,
        "mime_type": "image/bmp",
        "image_base64": include_base64.then(|| base64_encode(&bmp))
    }))
    .map_err(|error| format!("Unable to encode screenshot metadata: {error}"))
}

#[cfg(all(target_os = "windows", test))]
fn computer_snapshot(_arguments: &Value, _external_path_approved: bool) -> Result<String, String> {
    Ok(serde_json::to_string_pretty(&json!({
        "screenshot_available": false,
        "note": "Screen capture is disabled in the Windows GNU test binary."
    }))
    .unwrap_or_default())
}

#[cfg(not(target_os = "windows"))]
fn computer_snapshot(_arguments: &Value, _external_path_approved: bool) -> Result<String, String> {
    Ok(serde_json::to_string_pretty(&json!({
        "screenshot_available": false,
        "note": "Computer input is only available on Windows in this desktop build."
    }))
    .unwrap_or_default())
}

pub(crate) async fn run(arguments: &Value, external_path_approved: bool) -> Result<String, String> {
    let action = string_argument(arguments, "action")
        .ok_or_else(|| "rust_computer_use requires action".to_string())?;
    if action == "wait" {
        let duration = arguments
            .get("duration")
            .and_then(Value::as_f64)
            .unwrap_or(0.5)
            .clamp(0.0, 30.0);
        tokio::time::sleep(Duration::from_secs_f64(duration)).await;
        return Ok(format!("Waited for {duration:.2} seconds."));
    }
    if action == "screenshot" {
        return computer_snapshot(arguments, external_path_approved);
    }

    #[cfg(all(target_os = "windows", not(test)))]
    {
        match action.as_str() {
            "move_to" => {
                let x = arguments
                    .get("x")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "move_to requires x".to_string())?
                    as i32;
                let y = arguments
                    .get("y")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "move_to requires y".to_string())?
                    as i32;
                if unsafe { SetCursorPos(x, y) } == 0 {
                    return Err("Windows rejected SetCursorPos.".to_string());
                }
                Ok(format!("Moved cursor to ({x}, {y})."))
            }
            "click" => {
                if let (Some(x), Some(y)) = (
                    arguments.get("x").and_then(Value::as_i64),
                    arguments.get("y").and_then(Value::as_i64),
                ) {
                    unsafe {
                        SetCursorPos(x as i32, y as i32);
                    }
                }
                let button =
                    string_argument(arguments, "button").unwrap_or_else(|| "left".to_string());
                let (down, up) = match button.as_str() {
                    "right" => (0x0008, 0x0010),
                    "middle" => (0x0020, 0x0040),
                    _ => (0x0002, 0x0004),
                };
                let clicks = arguments
                    .get("num_clicks")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .clamp(1, 3);
                for _ in 0..clicks {
                    unsafe {
                        mouse_event(down, 0, 0, 0, 0);
                        mouse_event(up, 0, 0, 0, 0);
                    }
                }
                Ok(format!("Performed {clicks} {button} click(s)."))
            }
            "scroll" => {
                let amount = arguments
                    .get("amount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .clamp(-10, 10);
                unsafe {
                    mouse_event(0x0800, 0, 0, (amount * 120) as u32, 0);
                }
                Ok(format!("Scrolled by {amount}."))
            }
            "type" => {
                let text = string_argument(arguments, "text")
                    .ok_or_else(|| "type requires text".to_string())?;
                for character in text.encode_utf16() {
                    let mapped = unsafe { VkKeyScanW(character) };
                    if mapped == -1 {
                        continue;
                    }
                    let virtual_key = (mapped & 0xFF) as u8;
                    let shift_state = ((mapped >> 8) & 0xFF) as u8;
                    if shift_state & 1 != 0 {
                        unsafe {
                            keybd_event(0x10, 0, 0, 0);
                        }
                    }
                    unsafe {
                        keybd_event(virtual_key, 0, 0, 0);
                        keybd_event(virtual_key, 0, 0x0002, 0);
                    }
                    if shift_state & 1 != 0 {
                        unsafe {
                            keybd_event(0x10, 0, 0x0002, 0);
                        }
                    }
                }
                Ok(format!("Typed {} characters.", text.chars().count()))
            }
            "press" => {
                let key = string_argument(arguments, "key")
                    .ok_or_else(|| "press requires key".to_string())?;
                let virtual_key =
                    windows_key_code(&key).ok_or_else(|| format!("Unsupported key: {key}"))?;
                unsafe {
                    keybd_event(virtual_key, 0, 0, 0);
                    keybd_event(virtual_key, 0, 0x0002, 0);
                }
                Ok(format!("Pressed {key}."))
            }
            _ => Err(format!("Unsupported computer action: {action}")),
        }
    }
    #[cfg(any(not(target_os = "windows"), test))]
    {
        Err(
            "rust_computer_use input actions require Windows user32 in a production build."
                .to_string(),
        )
    }
}
