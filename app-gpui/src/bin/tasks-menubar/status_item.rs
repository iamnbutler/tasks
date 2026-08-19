//! The NSStatusItem, and nothing else.
//!
//! gpui owns the run loop, the windows and everything drawn; this module owns
//! the one AppKit object gpui has no concept of — the item in the menu bar —
//! plus the activation policy that keeps this binary out of the Dock. It is
//! written with the same `objc`/`cocoa` bindings gpui-macos uses (and pins),
//! so nothing new enters the dependency tree.
//!
//! Everything here runs on the main thread: [`install`] is called from
//! `Application::run`'s callback, and the button's action fires from the
//! AppKit run loop. The thread-locals are that assumption written down —
//! touching them from another thread finds them empty rather than racing.

#![cfg(target_os = "macos")]
// The `objc` 0.2 macros expand `cfg(feature = "cargo-clippy")` checks into
// this crate, which trips `unexpected_cfgs` here through no fault of ours.
#![allow(unexpected_cfgs)]

use std::cell::RefCell;
use std::sync::Once;

use cocoa::appkit::{NSApplication, NSApplicationActivationPolicy, NSScreen, NSWindow};
use cocoa::base::{id, nil, YES};
use cocoa::foundation::{NSArray, NSString};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};

use crate::popup::Anchor;

/// What a click on the status item calls, with where the item is.
type ClickHandler = Box<dyn FnMut(Anchor)>;

thread_local! {
    /// The item, retained for the process's life — a status item that is
    /// dropped vanishes from the bar.
    static STATUS_ITEM: RefCell<Option<StatusItem>> = const { RefCell::new(None) };
}

struct StatusItem {
    item: id,
    /// The action target, retained alongside the item it serves.
    _target: id,
    on_click: ClickHandler,
}

/// Put the item in the menu bar and take this app out of the Dock.
///
/// Call once, from the `Application::run` callback — that callback fires
/// inside `applicationDidFinishLaunching`, *after* gpui has set the
/// activation policy to Regular, which is the only reason the Accessory
/// write here wins.
pub fn install(on_click: ClickHandler) {
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        app.setActivationPolicy_(
            NSApplicationActivationPolicy::NSApplicationActivationPolicyAccessory,
        );

        let status_bar: id = msg_send![class!(NSStatusBar), systemStatusBar];
        // NSVariableStatusItemLength; the constant is -1.0 and cocoa's
        // binding for it is a private const, so it is stated here.
        let item: id = msg_send![status_bar, statusItemWithLength: -1.0f64];
        let item: id = msg_send![item, retain];

        let button: id = msg_send![item, button];
        set_button_face(button);

        let target: id = msg_send![target_class(), new];
        let _: () = msg_send![button, setTarget: target];
        let _: () = msg_send![button, setAction: sel!(tasksMenubarClicked:)];

        STATUS_ITEM.with(|slot| {
            *slot.borrow_mut() = Some(StatusItem {
                item,
                _target: target,
                on_click,
            });
        });
    }
}

/// An SF Symbol when the OS has it (macOS 11+, and this repo's floor is far
/// above that), a plain title if it somehow does not — a status item with
/// neither image nor title is an invisible click target.
unsafe fn set_button_face(button: id) {
    let name = NSString::alloc(nil).init_str("checklist");
    let description = NSString::alloc(nil).init_str("Tasks");
    let image: id = msg_send![class!(NSImage), imageWithSystemSymbolName: name accessibilityDescription: description];
    if image != nil {
        // Template rendering is what makes it recolor with the menu bar.
        let _: () = msg_send![image, setTemplate: YES];
        let _: () = msg_send![button, setImage: image];
    } else {
        let title = NSString::alloc(nil).init_str("✓");
        let _: () = msg_send![button, setTitle: title];
    }
}

/// The action target's class: an NSObject with one method, registered once.
fn target_class() -> &'static Class {
    static REGISTER: Once = Once::new();
    static mut CLASS: *const Class = std::ptr::null();
    REGISTER.call_once(|| {
        let mut decl = ClassDecl::new("TasksMenubarTarget", class!(NSObject))
            .expect("register TasksMenubarTarget once");
        unsafe {
            decl.add_method(
                sel!(tasksMenubarClicked:),
                clicked as extern "C" fn(&Object, Sel, id),
            );
        }
        unsafe { CLASS = decl.register() };
    });
    unsafe { &*CLASS }
}

extern "C" fn clicked(_this: &Object, _sel: Sel, _sender: id) {
    STATUS_ITEM.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(status_item) = slot.as_mut() else {
            return;
        };
        let anchor = unsafe { anchor_for(status_item.item) };
        (status_item.on_click)(anchor);
    });
}

/// Where the popup goes, in gpui's global coordinates.
///
/// AppKit's screen space is bottom-left-origin with y up, unified across
/// displays relative to the primary screen; gpui's is the same space flipped
/// to top-left-origin with y down. Both are in points, so the flip is the
/// whole conversion: `gpui_y = primary_height - appkit_y`.
unsafe fn anchor_for(item: id) -> Anchor {
    let fallback = Anchor {
        x: 8.0,
        bottom: 28.0,
    };

    let button: id = msg_send![item, button];
    if button == nil {
        return fallback;
    }
    let window: id = msg_send![button, window];
    if window == nil {
        return fallback;
    }
    let frame = NSWindow::frame(window);

    let screens = NSScreen::screens(nil);
    if NSArray::count(screens) == 0 {
        return fallback;
    }
    let primary = NSArray::objectAtIndex(screens, 0);
    let primary_height = NSScreen::frame(primary).size.height;

    Anchor {
        x: frame.origin.x,
        // The button's window spans the menu bar's height, so its AppKit
        // bottom (origin.y) is the popup's gpui top.
        bottom: primary_height - frame.origin.y,
    }
}
