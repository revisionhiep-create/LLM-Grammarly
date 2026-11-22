#import <ApplicationServices/ApplicationServices.h>
#import <Carbon/Carbon.h>
#import <Cocoa/Cocoa.h>
#import <UniformTypeIdentifiers/UniformTypeIdentifiers.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

typedef void (*MacHotkeyCallback)(uint32_t hotkey_id, void *user_data);
typedef void (*MacShellCallback)(uint32_t cmd_id, void *user_data);

static const OSType kHotkeySignature = 0x4C47524D; // 'LGRM'

static atomic_bool g_shutting_down = false;

static _Atomic(MacHotkeyCallback) g_hotkey_cb = NULL;
static _Atomic(void *) g_hotkey_user_data = NULL;
static EventHandlerRef g_hotkey_handler_ref = NULL;
static EventHotKeyRef g_hotkey_refs[4] = {NULL, NULL, NULL, NULL};

@interface LlmGrammarlyTrayTarget : NSObject
- (void)onShow:(id)sender;
- (void)onToggle:(id)sender;
- (void)onQuit:(id)sender;
@end

static _Atomic(MacShellCallback) g_shell_cb = NULL;
static _Atomic(void *) g_shell_user_data = NULL;
static LlmGrammarlyTrayTarget *g_tray_target = nil;
static NSStatusItem *g_status_item = nil;
static NSMenuItem *g_status_menu_item = nil;
static NSMenuItem *g_toggle_menu_item = nil;
static NSImage *g_icon_off = nil;
static NSImage *g_icon_on = nil;
static __weak NSWindow *g_window = nil;

@implementation LlmGrammarlyTrayTarget
- (void)dispatchCommand:(uint32_t)cmd_id {
    MacShellCallback cb = atomic_load(&g_shell_cb);
    void *ud = atomic_load(&g_shell_user_data);
    if (cb != NULL) {
        cb(cmd_id, ud);
    }
}

- (void)onToggle:(id)sender {
    (void)sender;
    [self dispatchCommand:0];
}

- (void)onShow:(id)sender {
    (void)sender;
    [self dispatchCommand:1];
}

- (void)onQuit:(id)sender {
    (void)sender;
    [self dispatchCommand:2];
}
@end

static void run_on_main_async(dispatch_block_t block) {
    if (!block) {
        return;
    }
    if ([NSThread isMainThread]) {
        @autoreleasepool {
            block();
        }
    } else {
        dispatch_async(dispatch_get_main_queue(), ^{
            @autoreleasepool {
                block();
            }
        });
    }
}

// Only safe from detached worker threads that are never joined during shutdown.
static void run_on_main_sync(dispatch_block_t block) {
    if (!block) {
        return;
    }
    if ([NSThread isMainThread]) {
        @autoreleasepool {
            block();
        }
    } else {
        dispatch_sync(dispatch_get_main_queue(), ^{
            @autoreleasepool {
                block();
            }
        });
    }
}

static bool run_on_main_with_timeout(int timeout_ms, dispatch_block_t block) {
    if (!block) {
        return false;
    }
    if ([NSThread isMainThread]) {
        @autoreleasepool {
            block();
        }
        return true;
    }
    if (atomic_load(&g_shutting_down)) {
        return false;
    }

    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    dispatch_async(dispatch_get_main_queue(), ^{
        @autoreleasepool {
            if (!atomic_load(&g_shutting_down)) {
                block();
            }
        }
        dispatch_semaphore_signal(sem);
    });

    int elapsed_ms = 0;
    const int step_ms = 25;
    while (elapsed_ms < timeout_ms) {
        if (atomic_load(&g_shutting_down)) {
            return false;
        }
        dispatch_time_t wait_until =
            dispatch_time(DISPATCH_TIME_NOW, (int64_t)step_ms * NSEC_PER_MSEC);
        if (dispatch_semaphore_wait(sem, wait_until) == 0) {
            return true;
        }
        elapsed_ms += step_ms;
    }
    return false;
}

static NSWindow *resolve_window(void *ns_view_ptr) {
    if (ns_view_ptr != NULL) {
        NSView *view = (__bridge NSView *)ns_view_ptr;
        if (view.window != nil) {
            g_window = view.window;
            return view.window;
        }
    }
    NSWindow *win = g_window;
    if (win != nil) {
        return win;
    }
    win = [NSApp mainWindow];
    if (win != nil) {
        g_window = win;
    }
    return win;
}

static OSStatus carbon_hotkey_handler(
    EventHandlerCallRef next_handler,
    EventRef event,
    void *user_data
) {
    (void)next_handler;
    (void)user_data;

    EventHotKeyID hk_id = {0, 0};
    OSStatus err = GetEventParameter(
        event,
        kEventParamDirectObject,
        typeEventHotKeyID,
        NULL,
        sizeof(hk_id),
        NULL,
        &hk_id
    );
    if (err == noErr && hk_id.signature == kHotkeySignature) {
        MacHotkeyCallback cb = atomic_load(&g_hotkey_cb);
        void *ud = atomic_load(&g_hotkey_user_data);
        if (cb != NULL) {
            cb((uint32_t)hk_id.id, ud);
        }
    }
    return noErr;
}

static NSImage *image_from_rgba(const uint8_t *rgba, uint32_t size, CGFloat pt_size) {
    if (rgba == NULL || size == 0 || size > 4096) {
        return nil;
    }
    NSInteger dim = (NSInteger)size;
    NSBitmapImageRep *rep = [[NSBitmapImageRep alloc]
        initWithBitmapDataPlanes:NULL
                      pixelsWide:dim
                      pixelsHigh:dim
                   bitsPerSample:8
                 samplesPerPixel:4
                        hasAlpha:YES
                        isPlanar:NO
                  colorSpaceName:NSCalibratedRGBColorSpace
                    bitmapFormat:0
                     bytesPerRow:dim * 4
                    bitsPerPixel:32];
    if (rep == nil) {
        return nil;
    }

    uint8_t *dst = [rep bitmapData];
    if (dst == NULL) {
        return nil;
    }
    size_t total_px = (size_t)size * (size_t)size;
    for (size_t i = 0; i < total_px; i++) {
        size_t off = i * 4;
        uint16_t r = rgba[off + 0];
        uint16_t g = rgba[off + 1];
        uint16_t b = rgba[off + 2];
        uint16_t a = rgba[off + 3];
        // NSBitmapImageRep with bitmapFormat 0 expects premultiplied RGBA.
        dst[off + 0] = (uint8_t)((r * a + 127) / 255);
        dst[off + 1] = (uint8_t)((g * a + 127) / 255);
        dst[off + 2] = (uint8_t)((b * a + 127) / 255);
        dst[off + 3] = (uint8_t)a;
    }

    NSImage *img = [[NSImage alloc] initWithSize:NSMakeSize(pt_size, pt_size)];
    [img addRepresentation:rep];
    [img setTemplate:NO];
    return img;
}

static NSURL *url_from_cstr(const char *path_utf8) {
    if (path_utf8 == NULL || path_utf8[0] == '\0') {
        return nil;
    }
    NSString *path = [NSString stringWithUTF8String:path_utf8];
    if (path == nil || path.length == 0) {
        return nil;
    }
    return [NSURL fileURLWithPath:path isDirectory:YES];
}

void macos_begin_shutdown(void) {
    atomic_store(&g_shutting_down, true);
}

bool macos_hotkey_init(MacHotkeyCallback cb, void *user_data) {
    __block bool ok = false;
    bool completed = run_on_main_with_timeout(2000, ^{
        atomic_store(&g_hotkey_cb, cb);
        atomic_store(&g_hotkey_user_data, user_data);
        if (g_hotkey_handler_ref != NULL) {
            ok = true;
            return;
        }
        EventTypeSpec spec = {kEventClassKeyboard, kEventHotKeyPressed};
        OSStatus status = InstallApplicationEventHandler(
            &carbon_hotkey_handler,
            1,
            &spec,
            NULL,
            &g_hotkey_handler_ref
        );
        ok = (status == noErr);
    });
    return completed && ok;
}

int32_t macos_hotkey_register(uint32_t id, uint32_t keycode, uint32_t carbon_mods) {
    if (id < 1 || id > 3) {
        return -50; // paramErr
    }
    __block OSStatus status = -1;
    bool completed = run_on_main_with_timeout(2000, ^{
        if (g_hotkey_refs[id] != NULL) {
            UnregisterEventHotKey(g_hotkey_refs[id]);
            g_hotkey_refs[id] = NULL;
        }
        EventHotKeyID hk_id = {kHotkeySignature, id};
        EventHotKeyRef ref = NULL;
        status = RegisterEventHotKey(
            keycode,
            carbon_mods,
            hk_id,
            GetApplicationEventTarget(),
            0,
            &ref
        );
        if (status == noErr) {
            g_hotkey_refs[id] = ref;
        }
    });
    return completed ? (int32_t)status : -1;
}

void macos_hotkey_unregister(uint32_t id) {
    if (id < 1 || id > 3) {
        return;
    }
    run_on_main_async(^{
        if (g_hotkey_refs[id] != NULL) {
            UnregisterEventHotKey(g_hotkey_refs[id]);
            g_hotkey_refs[id] = NULL;
        }
    });
}

void macos_hotkey_shutdown(void) {
    atomic_store(&g_hotkey_cb, NULL);
    atomic_store(&g_hotkey_user_data, NULL);
    run_on_main_async(^{
        for (int i = 1; i <= 3; i++) {
            if (g_hotkey_refs[i] != NULL) {
                UnregisterEventHotKey(g_hotkey_refs[i]);
                g_hotkey_refs[i] = NULL;
            }
        }
        if (g_hotkey_handler_ref != NULL) {
            RemoveEventHandler(g_hotkey_handler_ref);
            g_hotkey_handler_ref = NULL;
        }
    });
}

bool macos_is_accessibility_trusted(bool prompt) {
    if (!prompt) {
        return AXIsProcessTrusted() ? true : false;
    }
    @autoreleasepool {
        NSDictionary *opts = @{
            (__bridge id)kAXTrustedCheckOptionPrompt : @YES,
        };
        return AXIsProcessTrustedWithOptions((__bridge CFDictionaryRef)opts) ? true : false;
    }
}

void macos_open_accessibility_settings(void) {
    run_on_main_async(^{
        NSURL *url = [NSURL URLWithString:
            @"x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"];
        if (url != nil) {
            [[NSWorkspace sharedWorkspace] openURL:url];
        }
    });
}

bool macos_is_key_down(uint16_t keycode) {
    return CGEventSourceKeyState(kCGEventSourceStateCombinedSessionState, (CGKeyCode)keycode)
        ? true
        : false;
}

static void post_key_with_flags(uint16_t keycode, CGEventFlags flags) {
    CGEventSourceRef source = CGEventSourceCreate(kCGEventSourceStatePrivate);
    if (source == NULL) {
        return;
    }
    CGEventRef down = CGEventCreateKeyboardEvent(source, (CGKeyCode)keycode, true);
    CGEventRef up = CGEventCreateKeyboardEvent(source, (CGKeyCode)keycode, false);
    if (down != NULL && up != NULL) {
        CGEventSetFlags(down, flags);
        CGEventSetFlags(up, flags);
        CGEventPost(kCGHIDEventTap, down);
        usleep(8000);
        CGEventPost(kCGHIDEventTap, up);
    }
    if (down != NULL) {
        CFRelease(down);
    }
    if (up != NULL) {
        CFRelease(up);
    }
    CFRelease(source);
}

void macos_send_cmd_key(uint16_t keycode) {
    post_key_with_flags(keycode, kCGEventFlagMaskCommand);
}

void macos_tap_key(uint16_t keycode) {
    post_key_with_flags(keycode, 0);
}

int32_t macos_frontmost_pid(void) {
    @autoreleasepool {
        NSRunningApplication *app = [[NSWorkspace sharedWorkspace] frontmostApplication];
        return app != nil ? (int32_t)app.processIdentifier : 0;
    }
}

int64_t macos_clipboard_change_count(void) {
    @autoreleasepool {
        return (int64_t)[[NSPasteboard generalPasteboard] changeCount];
    }
}

char *macos_clipboard_read_text(void) {
    @autoreleasepool {
        NSString *text = [[NSPasteboard generalPasteboard] stringForType:NSPasteboardTypeString];
        if (text == nil) {
            return NULL;
        }
        const char *utf8 = [text UTF8String];
        return utf8 != NULL ? strdup(utf8) : NULL;
    }
}

bool macos_clipboard_write_text(const char *utf8) {
    if (utf8 == NULL) {
        return false;
    }
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        [pb clearContents];
        NSString *str = [NSString stringWithUTF8String:utf8];
        if (str == nil) {
            return false;
        }
        return [pb setString:str forType:NSPasteboardTypeString] ? true : false;
    }
}

void macos_free_string(char *ptr) {
    free(ptr);
}

bool macos_shell_install(
    void *ns_view_ptr,
    const uint8_t *icon_off_rgba,
    const uint8_t *icon_on_rgba,
    uint32_t icon_size,
    const char *initial_tip_utf8,
    MacShellCallback cb,
    void *user_data
) {
    if (![NSThread isMainThread]) {
        return false;
    }
    @autoreleasepool {
        resolve_window(ns_view_ptr);
        atomic_store(&g_shell_cb, cb);
        atomic_store(&g_shell_user_data, user_data);

        g_icon_off = image_from_rgba(icon_off_rgba, icon_size, 18.0);
        g_icon_on = image_from_rgba(icon_on_rgba, icon_size, 18.0);
        if (g_tray_target == nil) {
            g_tray_target = [[LlmGrammarlyTrayTarget alloc] init];
        }

        if (g_status_item == nil) {
            g_status_item = [[NSStatusBar systemStatusBar]
                statusItemWithLength:NSSquareStatusItemLength];
        }
        if (g_status_item == nil) {
            return false;
        }

        NSString *tip = initial_tip_utf8 != NULL
            ? [NSString stringWithUTF8String:initial_tip_utf8]
            : @"LLM Grammarly — Off";
        if (tip == nil) {
            tip = @"LLM Grammarly — Off";
        }

        if (g_status_item.button != nil) {
            g_status_item.button.image = g_icon_off;
            g_status_item.button.toolTip = tip;
        }

        NSMenu *menu = [[NSMenu alloc] initWithTitle:@"LLM Grammarly"];
        [menu setAutoenablesItems:NO];

        g_status_menu_item = [[NSMenuItem alloc] initWithTitle:tip action:nil keyEquivalent:@""];
        [g_status_menu_item setEnabled:NO];
        [menu addItem:g_status_menu_item];

        [menu addItem:[NSMenuItem separatorItem]];

        NSMenuItem *show_item = [[NSMenuItem alloc] initWithTitle:@"Show Window"
                                                           action:@selector(onShow:)
                                                    keyEquivalent:@""];
        [show_item setTarget:g_tray_target];
        [show_item setEnabled:YES];
        [menu addItem:show_item];

        g_toggle_menu_item = [[NSMenuItem alloc] initWithTitle:@"Turn On"
                                                        action:@selector(onToggle:)
                                                 keyEquivalent:@""];
        [g_toggle_menu_item setTarget:g_tray_target];
        [g_toggle_menu_item setEnabled:YES];
        [menu addItem:g_toggle_menu_item];

        [menu addItem:[NSMenuItem separatorItem]];

        NSMenuItem *quit_item = [[NSMenuItem alloc] initWithTitle:@"Quit LLM Grammarly"
                                                           action:@selector(onQuit:)
                                                    keyEquivalent:@""];
        [quit_item setTarget:g_tray_target];
        [quit_item setEnabled:YES];
        [menu addItem:quit_item];

        g_status_item.menu = menu;
        return true;
    }
}

void macos_shell_sync_state(
    bool is_on,
    bool is_busy,
    const char *tip_utf8,
    const char *toggle_label_utf8
) {
    @autoreleasepool {
        NSString *tip = tip_utf8 != NULL
            ? [NSString stringWithUTF8String:tip_utf8]
            : @"LLM Grammarly";
        NSString *toggle_lbl = toggle_label_utf8 != NULL
            ? [NSString stringWithUTF8String:toggle_label_utf8]
            : (is_on ? @"Turn Off" : @"Turn On");

        run_on_main_async(^{
            if (g_status_item != nil && g_status_item.button != nil) {
                NSImage *img = is_on ? g_icon_on : g_icon_off;
                if (img != nil) {
                    g_status_item.button.image = img;
                }
                if (tip != nil) {
                    g_status_item.button.toolTip = tip;
                }
            }
            if (g_status_menu_item != nil && tip != nil) {
                [g_status_menu_item setTitle:tip];
            }
            if (g_toggle_menu_item != nil && toggle_lbl != nil) {
                [g_toggle_menu_item setTitle:toggle_lbl];
                [g_toggle_menu_item setEnabled:!is_busy];
            }
        });
    }
}

void macos_shell_set_dock_badge(const char *badge_utf8_or_null) {
    @autoreleasepool {
        NSString *badge = nil;
        if (badge_utf8_or_null != NULL && badge_utf8_or_null[0] != '\0') {
            badge = [NSString stringWithUTF8String:badge_utf8_or_null];
        }
        run_on_main_async(^{
            [[NSApp dockTile] setBadgeLabel:badge];
        });
    }
}

void macos_shell_show_window(void) {
    run_on_main_async(^{
        [NSApp unhide:nil];
        [NSApp activateIgnoringOtherApps:YES];
        NSWindow *win = resolve_window(NULL);
        if (win != nil) {
            if ([win isMiniaturized]) {
                [win deminiaturize:nil];
            }
            [win makeKeyAndOrderFront:nil];
        }
    });
}

void macos_shell_hide_window(void) {
    run_on_main_async(^{
        NSWindow *win = resolve_window(NULL);
        if (win != nil) {
            [win orderOut:nil];
        }
    });
}

void macos_shell_close_window(void) {
    run_on_main_async(^{
        NSWindow *win = resolve_window(NULL);
        if (win != nil) {
            if (![win isVisible]) {
                [win orderFront:nil];
            }
            [win performClose:nil];
        }
    });
}

void macos_shell_shutdown(void) {
    atomic_store(&g_shell_cb, NULL);
    atomic_store(&g_shell_user_data, NULL);
    run_on_main_async(^{
        if (g_status_item != nil) {
            [[NSStatusBar systemStatusBar] removeStatusItem:g_status_item];
            g_status_item = nil;
        }
        g_status_menu_item = nil;
        g_toggle_menu_item = nil;
        g_tray_target = nil;
        g_icon_off = nil;
        g_icon_on = nil;
        [[NSApp dockTile] setBadgeLabel:nil];
    });
}

void macos_set_dark_titlebar(void *ns_view_ptr, bool dark) {
    if ([NSThread isMainThread]) {
        @autoreleasepool {
            NSWindow *win = resolve_window(ns_view_ptr);
            if (win != nil) {
                NSAppearanceName name = dark ? NSAppearanceNameDarkAqua : NSAppearanceNameAqua;
                win.appearance = [NSAppearance appearanceNamed:name];
            }
        }
        return;
    }
    // Off-main calls must not capture raw ns_view_ptr asynchronously; use weak g_window.
    run_on_main_async(^{
        NSWindow *win = resolve_window(NULL);
        if (win != nil) {
            NSAppearanceName name = dark ? NSAppearanceNameDarkAqua : NSAppearanceNameAqua;
            win.appearance = [NSAppearance appearanceNamed:name];
        }
    });
}

bool macos_get_window_placement(
    void *ns_view_ptr,
    int32_t *out_x,
    int32_t *out_y,
    bool *out_min,
    bool *out_max
) {
    if (![NSThread isMainThread]) {
        return false;
    }
    @autoreleasepool {
        NSWindow *win = resolve_window(ns_view_ptr);
        if (win == nil || ![win isVisible]) {
            return false;
        }
        NSRect frame = [win frame];
        if (out_x != NULL) {
            *out_x = (int32_t)llround(frame.origin.x);
        }
        if (out_y != NULL) {
            *out_y = (int32_t)llround(frame.origin.y);
        }
        if (out_min != NULL) {
            *out_min = [win isMiniaturized] ? true : false;
        }
        if (out_max != NULL) {
            *out_max = [win isZoomed] ? true : false;
        }
        return true;
    }
}

static void restore_window_position_on_main(NSWindow *win, int32_t x, int32_t y) {
    if (win == nil) {
        return;
    }
    NSArray<NSScreen *> *screens = [NSScreen screens];
    if (screens == nil || screens.count == 0) {
        [win setFrameOrigin:NSMakePoint((CGFloat)x, (CGFloat)y)];
        return;
    }

    NSRect frame = [win frame];
    CGFloat win_w = frame.size.width > 200.0 ? frame.size.width : 495.0;
    CGFloat win_h = frame.size.height > 100.0 ? frame.size.height : 625.0;

    NSPoint probe = NSMakePoint((CGFloat)x + 100.0, (CGFloat)y + 20.0);
    NSScreen *target_screen = nil;
    for (NSScreen *scr in screens) {
        if (NSPointInRect(probe, [scr visibleFrame])) {
            target_screen = scr;
            break;
        }
    }
    if (target_screen == nil) {
        CGFloat best_dist = CGFLOAT_MAX;
        for (NSScreen *scr in screens) {
            NSRect vis = [scr visibleFrame];
            CGFloat cx = NSMidX(vis);
            CGFloat cy = NSMidY(vis);
            CGFloat dx = probe.x - cx;
            CGFloat dy = probe.y - cy;
            CGFloat dist = dx * dx + dy * dy;
            if (dist < best_dist) {
                best_dist = dist;
                target_screen = scr;
            }
        }
    }

    CGFloat target_x = (CGFloat)x;
    CGFloat target_y = (CGFloat)y;
    if (target_screen != nil) {
        NSRect vis = [target_screen visibleFrame];
        CGFloat max_x = vis.origin.x + fmax(0.0, vis.size.width - win_w);
        CGFloat max_y = vis.origin.y + fmax(0.0, vis.size.height - win_h);
        if (target_x < vis.origin.x) {
            target_x = vis.origin.x;
        } else if (target_x > max_x) {
            target_x = max_x;
        }
        if (target_y < vis.origin.y) {
            target_y = vis.origin.y;
        } else if (target_y > max_y) {
            target_y = max_y;
        }
    }

    [win setFrameOrigin:NSMakePoint(target_x, target_y)];
}

void macos_restore_window_position(void *ns_view_ptr, int32_t x, int32_t y) {
    if ([NSThread isMainThread]) {
        @autoreleasepool {
            restore_window_position_on_main(resolve_window(ns_view_ptr), x, y);
        }
        return;
    }
    run_on_main_async(^{
        restore_window_position_on_main(resolve_window(NULL), x, y);
    });
}

char *macos_pick_folder(const char *initial_dir_or_null) {
    @autoreleasepool {
        NSURL *init_url = url_from_cstr(initial_dir_or_null);
        __block char *result = NULL;
        run_on_main_sync(^{
            [NSApp activateIgnoringOtherApps:YES];
            NSOpenPanel *panel = [NSOpenPanel openPanel];
            [panel setCanChooseFiles:NO];
            [panel setCanChooseDirectories:YES];
            [panel setCanCreateDirectories:YES];
            [panel setAllowsMultipleSelection:NO];
            if (init_url != nil) {
                [panel setDirectoryURL:init_url];
            }
            if ([panel runModal] == NSModalResponseOK) {
                NSString *path = [[panel URL] path];
                if (path != nil) {
                    const char *utf8 = [path UTF8String];
                    if (utf8 != NULL) {
                        result = strdup(utf8);
                    }
                }
            }
        });
        return result;
    }
}

char *macos_pick_document(const char *initial_dir_or_null) {
    @autoreleasepool {
        NSURL *init_url = url_from_cstr(initial_dir_or_null);
        __block char *result = NULL;
        run_on_main_sync(^{
            [NSApp activateIgnoringOtherApps:YES];
            NSOpenPanel *panel = [NSOpenPanel openPanel];
            [panel setCanChooseFiles:YES];
            [panel setCanChooseDirectories:NO];
            [panel setAllowsMultipleSelection:NO];

            NSMutableArray<UTType *> *types = [NSMutableArray array];
            for (NSString *ext in @[@"txt", @"md", @"markdown", @"docx"]) {
                UTType *ut = [UTType typeWithFilenameExtension:ext];
                if (ut != nil) {
                    [types addObject:ut];
                }
            }
            if (types.count > 0) {
                [panel setAllowedContentTypes:types];
            }
            if (init_url != nil) {
                [panel setDirectoryURL:init_url];
            }
            if ([panel runModal] == NSModalResponseOK) {
                NSString *path = [[panel URL] path];
                if (path != nil) {
                    const char *utf8 = [path UTF8String];
                    if (utf8 != NULL) {
                        result = strdup(utf8);
                    }
                }
            }
        });
        return result;
    }
}

char *macos_save_document(
    const char *initial_dir_or_null,
    const char *default_name,
    const char *default_ext
) {
    @autoreleasepool {
        NSURL *init_url = url_from_cstr(initial_dir_or_null);
        NSString *name_str = (default_name != NULL && default_name[0] != '\0')
            ? [NSString stringWithUTF8String:default_name]
            : nil;
        NSString *ext_str = (default_ext != NULL && default_ext[0] != '\0')
            ? [NSString stringWithUTF8String:default_ext]
            : nil;
        __block char *result = NULL;
        run_on_main_sync(^{
            [NSApp activateIgnoringOtherApps:YES];
            NSSavePanel *panel = [NSSavePanel savePanel];
            if (init_url != nil) {
                [panel setDirectoryURL:init_url];
            }
            if (name_str != nil) {
                [panel setNameFieldStringValue:name_str];
            }
            if (ext_str != nil) {
                NSString *clean_ext = [ext_str hasPrefix:@"."]
                    ? [ext_str substringFromIndex:1]
                    : ext_str;
                if (clean_ext.length > 0) {
                    UTType *ut = [UTType typeWithFilenameExtension:clean_ext];
                    if (ut != nil) {
                        [panel setAllowedContentTypes:@[ut]];
                    }
                }
            }
            if ([panel runModal] == NSModalResponseOK) {
                NSString *path = [[panel URL] path];
                if (path != nil) {
                    const char *utf8 = [path UTF8String];
                    if (utf8 != NULL) {
                        result = strdup(utf8);
                    }
                }
            }
        });
        return result;
    }
}
