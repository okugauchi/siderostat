//! Minimal macOS tray-icon experiment.
//!
//! Purpose: judge whether tray-icon 0.24 is usable on this macOS (26.6.1),
//! independent of the siderostat-monitor process. The monitor's `main.rs`
//! creates a TrayIcon but never runs the AppKit event loop, so this spike
//! demonstrates the correct pattern: create the NSApplication, set the
//! activation policy to a background accessory app, create the tray icon,
//! then run the AppKit main loop.

use objc2::MainThreadMarker;
use std::sync::atomic::{AtomicPtr, Ordering};

/// Raw pointer to the shared NSApplication, stored so the Send+Sync menu
/// event handler can reach it without capturing the non-Sync object.
static APP_PTR: AtomicPtr<NSApplication> = AtomicPtr::new(core::ptr::null_mut());
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

const QUIT_ID: &str = "quit";

fn main() {
    // 1. Create the shared NSApplication on the main thread (tray-icon requirement).
    let mtm = MainThreadMarker::new().expect("tray creation requires the main thread");
    let app = NSApplication::sharedApplication(mtm);

    // 2. Background accessory app: menu bar only, no dock icon.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    eprintln!("[tray-min] NSApplication created (policy=Accessory)");

    // 3. Build the tray icon + menu.
    let header = MenuItem::new("tray-min", false, None);
    let quit = MenuItem::with_id(QUIT_ID, "終了", true, None);
    let menu = Menu::new();
    menu.append(&header).expect("append header");
    menu.append(&PredefinedMenuItem::separator()).expect("separator");
    menu.append(&quit).expect("append quit");

    let icon = simple_icon();
    let tray = TrayIconBuilder::new()
        .with_tooltip("tray-min experiment")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .expect("build tray icon");
    eprintln!("[tray-min] TrayIcon created; running AppKit main loop");

    // 4. Quit from the menu terminates the app. The Send+Sync event-handler
    // closure cannot capture `Retained<NSApplication>` (not Send), so store the
    // app pointer in an `AtomicPtr` global instead. The app lives for the whole
    // process lifetime (held until main ends).
    APP_PTR.store((&*app as *const NSApplication).cast_mut(), Ordering::Relaxed);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if *event.id() == MenuId::new(QUIT_ID) {
            eprintln!("[tray-min] quit requested");
            let ptr = APP_PTR.load(Ordering::Relaxed);
            // SAFETY: APP_PTR is set before the handler can fire and the app
            // object outlives the process (main blocks in `app.run()`).
            unsafe { (*ptr).terminate(None) };
        }
    }));

    // 5. Run the AppKit main event loop. This is the piece the monitor is missing.
    app.run();

    drop(tray);
    eprintln!("[tray-min] app loop ended");
}

/// Small solid menu bar icon (same style as the monitor).
fn simple_icon() -> Icon {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        rgba.extend_from_slice(&[0x2d, 0x6c, 0xdf, 0xff]);
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("create menu bar icon")
}
