//! Native macOS integration: rounded app icon and system-menu About panel.
//!
//! On macOS the app name menu lives in the OS menu bar (top-left, next to the
//! Apple logo), not inside the window. winit/eframe already builds that menu,
//! including an "About <app>" item whose action is
//! `orderFrontStandardAboutPanel:`. We:
//!
//! 1. set `NSApplication`'s dock icon to the rounded-rectangle Joplin icon, and
//! 2. retarget the existing About menu item to a small
//!    `NSApplicationDelegate` that shows `orderFrontStandardAboutPanelWithOptions`
//!    populated with the rounded icon, app name and version.
//!
//! macOS only; other platforms keep the in-window About menu.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSImage, NSMenu};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSObject, NSString};

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

define_class!(
    // Delegate that owns the custom About action.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "JoplinRsAppDelegate"]
    struct AppDelegate;

    impl AppDelegate {
        #[unsafe(method(showAbout:))]
        fn show_about(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().unwrap();
            if let Some(options) = about_options() {
                let app = NSApplication::sharedApplication(mtm);
                unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
            }
        }
    }
);

impl AppDelegate {
    objc2::extern_methods!(
        #[unsafe(method(new))]
        fn new(mtm: MainThreadMarker) -> Retained<Self>;
    );
}

/// Build the options dictionary for the standard About panel: rounded icon,
/// name and version.
///
/// The panel renders `ApplicationVersion (Version)`, where `Version` is the
/// build number. Leaving `NSAboutPanelOptionVersion` unset makes AppKit fall
/// back to the bundle's `CFBundleVersion`, so an app whose Info.plist has the
/// same value as the release (`1.3.0`) shows a redundant
/// `v1.3.0 (6b3370cd2) (1.3.0)`. An explicit empty string leaves the build
/// part blank, matching `--version` exactly.
fn about_options() -> Option<Retained<NSDictionary<objc2_app_kit::NSAboutPanelOptionKey, AnyObject>>>
{
    let icon = rounded_icon()?;
    let icon_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(icon) };

    let keys: [&NSString; 4] = [
        unsafe { objc2_app_kit::NSAboutPanelOptionApplicationIcon },
        unsafe { objc2_app_kit::NSAboutPanelOptionApplicationName },
        unsafe { objc2_app_kit::NSAboutPanelOptionApplicationVersion },
        unsafe { objc2_app_kit::NSAboutPanelOptionVersion },
    ];
    let name = NSString::from_str("joplin-rs");
    let version = NSString::from_str(crate::APP_VERSION);
    let build = NSString::from_str("");
    let name_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(name) };
    let version_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(version) };
    let build_obj: Retained<AnyObject> = unsafe { Retained::cast_unchecked(build) };
    let objects: [&AnyObject; 4] = [&icon_obj, &name_obj, &version_obj, &build_obj];
    Some(NSDictionary::from_slices(&keys, &objects))
}

/// Wire up the rounded dock icon and the system About menu. Called once at
/// startup on the main thread.
pub fn install_native_about() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);

    if let Some(icon) = rounded_icon() {
        unsafe { app.setApplicationIconImage(Some(&icon)) };
    }

    // Build the delegate. Menu item targets are NOT retained (unsafe weak
    // references), so leak exactly one instance to keep it alive for the
    // whole process.
    let delegate = AppDelegate::new(mtm);
    let delegate: &'static AppDelegate = Box::leak(Box::new(delegate));

    let main_menu = match app.mainMenu() {
        Some(m) => m,
        None => return,
    };
    let Some(app_submenu_item) = main_menu.itemAtIndex(0) else {
        return;
    };
    let Some(app_menu) = app_submenu_item.submenu() else {
        return;
    };
    unsafe { retarget_about(&app_menu, delegate) };
}

/// Show the native About panel immediately (rounded icon, app name,
/// version). Used by the app-menu About action and the toolbar About
/// button on macOS.
pub fn show_native_about() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if let Some(options) = about_options() {
        let app = NSApplication::sharedApplication(mtm);
        unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
    }
}

fn rounded_icon() -> Option<Retained<NSImage>> {
    let mtm = MainThreadMarker::new()?;
    let data = NSData::with_bytes(ICON_PNG);
    // NSImage is declared MainThreadOnly in app-kit: alloc takes the marker.
    NSImage::initWithData(mtm.alloc(), &data)
}

/// Point the menu item currently bound to `orderFrontStandardAboutPanel:` at
/// our delegate's `showAbout:`.
unsafe fn retarget_about(menu: &NSMenu, delegate: &AppDelegate) {
    use objc2::runtime::Sel;
    use std::ffi::CString;
    let standard = Sel::register(&CString::new("orderFrontStandardAboutPanel:").unwrap());
    let custom = Sel::register(&CString::new("showAbout:").unwrap());

    let items: Retained<NSArray<objc2_app_kit::NSMenuItem>> = menu.itemArray();
    for item in items.iter() {
        if item.action() == Some(standard) {
            item.setTarget(Some(delegate));
            item.setAction(Some(custom));
        }
    }
}
