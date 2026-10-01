//! Wry child views defer all Command shortcuts to their native host. Restore
//! standard editing through AppKit's responder chain when this webview owns
//! focus, while leaving application shortcuts with GPUI.
use objc2::{
    msg_send,
    runtime::{AnyClass, AnyObject, Bool, Sel},
    sel,
};
use std::ffi::CStr;

const SHIFT: usize = 1 << 17;
const CONTROL: usize = 1 << 18;
const OPTION: usize = 1 << 19;
const COMMAND: usize = 1 << 20;

fn edit_action(key: &[u8], modifiers: usize) -> Option<Sel> {
    if modifiers & COMMAND == 0 || modifiers & (CONTROL | OPTION) != 0 {
        return None;
    }
    if modifiers & SHIFT != 0 {
        return (key == b"z" || key == b"Z").then(|| sel!(redo:));
    }
    match key {
        b"a" => Some(sel!(selectAll:)),
        b"c" => Some(sel!(copy:)),
        b"x" => Some(sel!(cut:)),
        b"v" => Some(sel!(paste:)),
        b"z" => Some(sel!(undo:)),
        _ => None,
    }
}

pub(super) unsafe fn key_equivalent(view: &AnyObject, event: &AnyObject) -> Option<Bool> {
    let modifiers: usize = unsafe { msg_send![event, modifierFlags] };
    let characters: *mut AnyObject = unsafe { msg_send![event, charactersIgnoringModifiers] };
    if characters.is_null() {
        return None;
    }
    let text: *const std::ffi::c_char = unsafe { msg_send![characters, UTF8String] };
    if text.is_null() {
        return None;
    }
    let action = edit_action(unsafe { CStr::from_ptr(text) }.to_bytes(), modifiers)?;
    let window: *mut AnyObject = unsafe { msg_send![view, window] };
    if window.is_null() || !unsafe { msg_send![window, isKeyWindow] } {
        return None;
    }
    let responder: *mut AnyObject = unsafe { msg_send![window, firstResponder] };
    let is_view = !responder.is_null()
        && AnyClass::get(c"NSView")
            .is_some_and(|class| unsafe { msg_send![responder, isKindOfClass: class] });
    if !is_view {
        return None;
    }
    let webview_class = AnyClass::get(c"WKWebView")?;
    let mut ancestor = responder;
    let mut inside_webview = false;
    while !ancestor.is_null() {
        if unsafe { msg_send![ancestor, isKindOfClass: webview_class] } {
            inside_webview = true;
            break;
        }
        ancestor = unsafe { msg_send![ancestor, superview] };
    }
    if !inside_webview {
        return None;
    }
    let app_class = AnyClass::get(c"NSApplication")?;
    let app: *mut AnyObject = unsafe { msg_send![app_class, sharedApplication] };
    Some(unsafe {
        msg_send![app, sendAction: action, to: std::ptr::null_mut::<AnyObject>(), from: view]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_standard_edit_shortcuts_are_forwarded() {
        assert_eq!(edit_action(b"v", COMMAND), Some(sel!(paste:)));
        assert_eq!(edit_action(b"z", COMMAND | SHIFT), Some(sel!(redo:)));
        assert_eq!(edit_action(b"q", COMMAND), None);
        assert_eq!(edit_action(b"v", COMMAND | OPTION), None);
        assert_eq!(edit_action(b"a", COMMAND | SHIFT), None);
        assert_eq!(edit_action(b"c", CONTROL), None);
    }
}
