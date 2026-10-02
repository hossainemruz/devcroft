//! Join GPUI's AccessKit tree with its native WKWebView child. AccessKit
//! overrides the content view's AX methods and otherwise hides native children.
use objc2::{
    ffi::object_setClass,
    msg_send,
    runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Imp, Sel},
    sel,
};
use objc2_foundation::NSPoint;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use wry::WebViewExtMacOS as _;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    fn NSAccessibilityUnignoredChildrenForOnlyChild(child: *mut AnyObject) -> *mut AnyObject;
}

#[derive(Clone, Copy)]
struct Methods {
    children: Imp,
    focus: Imp,
    hit: Imp,
    key: Imp,
}
unsafe extern "C" fn key(view: &AnyObject, cmd: Sel, event: &AnyObject) -> Bool {
    if let Some(handled) = unsafe { super::input::key_equivalent(view, event) } {
        return handled;
    }
    let Some(methods) = methods(view) else {
        return Bool::NO;
    };
    let original: unsafe extern "C" fn(&AnyObject, Sel, &AnyObject) -> Bool =
        unsafe { std::mem::transmute(methods.key) };
    unsafe { original(view, cmd, event) }
}
static METHODS: OnceLock<Mutex<HashMap<String, Methods>>> = OnceLock::new();
fn methods(view: &AnyObject) -> Option<Methods> {
    let registry = METHODS.get()?.lock().ok()?;
    // AppKit may introduce another dynamic subclass after attachment. Find
    // our registered ancestor; never panic across an Objective-C callback.
    let mut class = Some(view.class());
    while let Some(current) = class {
        if let Some(methods) = registry.get(current.name().to_string_lossy().as_ref()) {
            return Some(*methods);
        }
        class = current.superclass();
    }
    None
}
// Objective-C arrays here are autoreleased; neither callback transfers an
// owned Rust reference. The window owns native children throughout the call.
unsafe extern "C" fn children(view: &AnyObject, cmd: Sel) -> *mut AnyObject {
    let Some(methods) = methods(view) else {
        return std::ptr::null_mut();
    };
    let original: unsafe extern "C" fn(&AnyObject, Sel) -> *mut AnyObject =
        unsafe { std::mem::transmute(methods.children) };
    let old = unsafe { original(view, cmd) };
    let Some(array_class) = AnyClass::get(c"NSMutableArray") else {
        return old;
    };
    let array: *mut AnyObject = unsafe { msg_send![array_class, array] };
    if !old.is_null() {
        let _: () = unsafe { msg_send![array, addObjectsFromArray: old] };
    }
    let native: *mut AnyObject = unsafe { msg_send![view, subviews] };
    let count: usize = unsafe { msg_send![native, count] };
    for i in 0..count {
        let child: *mut AnyObject = unsafe { msg_send![native, objectAtIndex: i] };
        // WKWebView itself is an ignored NSView; expose its web-area children
        // alongside AccessKit's root instead of adding an ignored container.
        let descendants = unsafe { NSAccessibilityUnignoredChildrenForOnlyChild(child) };
        if !descendants.is_null() {
            let _: () = unsafe { msg_send![array, addObjectsFromArray: descendants] };
        }
    }
    array
}
unsafe extern "C" fn focus(view: &AnyObject, cmd: Sel) -> *mut AnyObject {
    let Some(methods) = methods(view) else {
        return std::ptr::null_mut();
    };
    let window: *mut AnyObject = unsafe { msg_send![view, window] };
    if !window.is_null() {
        let responder: *mut AnyObject = unsafe { msg_send![window, firstResponder] };
        let native: *mut AnyObject = unsafe { msg_send![view, subviews] };
        let count: usize = unsafe { msg_send![native, count] };
        if !responder.is_null() {
            let is_view: bool = AnyClass::get(c"NSView")
                .is_some_and(|class| unsafe { msg_send![responder, isKindOfClass: class] });
            if is_view {
                for i in 0..count {
                    let child: *mut AnyObject = unsafe { msg_send![native, objectAtIndex: i] };
                    let inside: bool = unsafe { msg_send![responder, isDescendantOf: child] };
                    if inside {
                        let focused: *mut AnyObject =
                            unsafe { msg_send![child, accessibilityFocusedUIElement] };
                        if !focused.is_null() {
                            return focused;
                        }
                    }
                }
            }
        }
    }
    let original: unsafe extern "C" fn(&AnyObject, Sel) -> *mut AnyObject =
        unsafe { std::mem::transmute(methods.focus) };
    unsafe { original(view, cmd) }
}
unsafe extern "C" fn hit(view: &AnyObject, cmd: Sel, point: NSPoint) -> *mut AnyObject {
    let Some(methods) = methods(view) else {
        return std::ptr::null_mut();
    };
    let native: *mut AnyObject = unsafe { msg_send![view, subviews] };
    let count: usize = unsafe { msg_send![native, count] };
    for i in 0..count {
        let child: *mut AnyObject = unsafe { msg_send![native, objectAtIndex: i] };
        let rect: objc2_foundation::NSRect = unsafe { msg_send![child, accessibilityFrame] };
        if point.x >= rect.origin.x
            && point.y >= rect.origin.y
            && point.x <= rect.origin.x + rect.size.width
            && point.y <= rect.origin.y + rect.size.height
        {
            let found: *mut AnyObject = unsafe { msg_send![child, accessibilityHitTest: point] };
            if !found.is_null() {
                return found;
            }
        }
    }
    let original: unsafe extern "C" fn(&AnyObject, Sel, NSPoint) -> *mut AnyObject =
        unsafe { std::mem::transmute(methods.hit) };
    unsafe { original(view, cmd, point) }
}
pub(crate) fn attach(webview: &wry::WebView) -> anyhow::Result<()> {
    let native = webview.webview();
    let window: *mut AnyObject = unsafe { msg_send![&*native, window] };
    anyhow::ensure!(!window.is_null(), "Webview has no native window");
    // GPUI's raw handle is its drawing view, nested below the window content
    // view. AccessKit subclasses the content view, so that is the tree to join.
    let view: *mut AnyObject = unsafe { msg_send![window, contentView] };
    let view =
        unsafe { view.as_ref() }.ok_or_else(|| anyhow::anyhow!("Webview has no native parent"))?;
    let previous = view.class();
    if previous
        .name()
        .to_string_lossy()
        .starts_with("DevcroftWebViewAX_")
    {
        return Ok(());
    }
    let name = format!("DevcroftWebViewAX_{}", previous.name().to_string_lossy());
    let mut registry = METHODS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("Native accessibility registry unavailable"))?;
    let name_c = std::ffi::CString::new(name.clone())?;
    let class = if let Some(class) = AnyClass::get(&name_c) {
        class
    } else {
        let get = |s| {
            previous
                .instance_method(s)
                .map(|m| m.implementation())
                .ok_or_else(|| anyhow::anyhow!("Native accessibility method missing"))
        };
        registry.insert(
            name,
            Methods {
                children: get(sel!(accessibilityChildren))?,
                focus: get(sel!(accessibilityFocusedUIElement))?,
                hit: get(sel!(accessibilityHitTest:))?,
                key: get(sel!(performKeyEquivalent:))?,
            },
        );
        let mut class = ClassBuilder::new(&name_c, previous)
            .ok_or_else(|| anyhow::anyhow!("Native accessibility class registration failed"))?;
        // No ivars are added, preserving the existing AccessKit instance layout.
        unsafe {
            class.add_method(
                sel!(accessibilityChildren),
                children as unsafe extern "C" fn(_, _) -> _,
            );
            class.add_method(
                sel!(accessibilityFocusedUIElement),
                focus as unsafe extern "C" fn(_, _) -> _,
            );
            class.add_method(
                sel!(accessibilityHitTest:),
                hit as unsafe extern "C" fn(_, _, _) -> _,
            );
            class.add_method(
                sel!(performKeyEquivalent:),
                key as unsafe extern "C" fn(_, _, _) -> _,
            );
        }
        class.register()
    };
    unsafe { object_setClass(view as *const _ as *mut _, class) };
    Ok(())
}
