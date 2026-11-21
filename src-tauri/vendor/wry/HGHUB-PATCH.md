# wry `platform_webview_version` patch (iOS 16.6.1 launch crash)

Vendored from `wry 0.57.1` (upstream sources and their original licenses retained).
Only `src/wkwebview/mod.rs::platform_webview_version` differs from upstream.

## The crash

User device: iPhone 13 Pro Max, iOS 16.6.1. App `红苹果影视` 0.0.12 and
0.0.14 both died at launch, before any UI appeared, with the same frames:

```
main → run → Builder::run → Wry::init → wry::webview_version
  → NSBundle::bundleWithIdentifier("com.apple.WebKit")
  → _CFBundleGetBundleWithIdentifier
  → _CFBundleEnsureBundleExistsForImagePath
  → __CFBundleCopyFrameworkURLForExecutablePath → CFRelease(NULL)
EXC_BREAKPOINT (SIGTRAP), asi: "*** CFRelease() called with NULL ***"
```

`tauri-runtime-wry` calls `wry::webview_version()` inside `Wry::init` and
uses the result only as a boolean (`webview_runtime_installed`), but the
call itself traps: at that point no WKWebView exists yet, so no CFBundle
object for WebKit is registered, and CoreFoundation's identifier lookup
falls into its ensure-by-image-path path, whose framework-URL construction
returns NULL on this iOS version and is released without a NULL check.
WebKit is linked (LC_LOAD_DYLIB present); the failure is CFBundle
registration, not dyld loading.

## Why pre-registration did not fix it (v0.0.13/v0.0.14)

The first fix (v0.0.13) called `NSBundle::bundleWithPath` for
`/System/Library/Frameworks/WebKit.framework` from `run()` before
`Builder::run()`, on the assumption that a registered bundle would make
the later identifier lookup hit the table and skip the failing path.
Disassembly of the shipped 0.0.14 binary proved the preload call was
inlined into `run()` and executed before `Builder::default()` — yet the
0.0.14 crash log shows the identical frames. The path-based registration
does not reliably make the identifier lookup succeed on iOS 16.6.1, so
relying on it is abandoned (the preload code was removed in the same
commit as this patch).

## The patch

`platform_webview_version` never calls `bundleWithIdentifier` anymore.
It reads the version through the path-based lookup instead:

1. `NSBundle::bundleWithPath("/System/Library/Frameworks/WebKit.framework")`
   → `infoDictionary` → `CFBundleVersion`. This lookup never takes the
   identifier ensure path (proven on the same device: the v0.0.14 preload
   called it before `Builder::run()` without crashing).
2. If the bundle cannot be located at all, return `Ok("unknown")` instead
   of `Err`. The version string is only used as a boolean
   (`webview_runtime_installed`) by tauri-runtime-wry; returning `Err`
   would make it refuse to create the webview on iOS
   (`create_webview` returns `Error::WebviewRuntimeNotInstalled` on
   non-macOS targets), trading a launch crash for an app with no UI.
   Nothing in this app parses the version string on Apple targets
   (`windows_login.rs` parses it, but that file is Windows-only).

## Keep this patch when upgrading

Until upstream changes `platform_webview_version` to avoid
`bundleWithIdentifier` (or the minimum supported iOS moves past the
affected versions), re-apply this change to any newer vendored wry.
Regression coverage is the device test: the app must launch on iOS 16.6.1
and show its UI. If it still crashes, the new `.ips` tells whether the
crash moved past `Wry::init`.

---

# iOS WebView background: black fallback instead of system background (v0.0.16)

Second change in this vendored copy, same file
(`src/wkwebview/mod.rs`, iOS `WebView::new` branch): when no
`background_color` attribute is set, the fallback was
`UIColor::systemBackgroundColor()`. That follows the phone's appearance,
so on a phone in light mode the WebView (and its parent view) is white.

## Why it mattered

v0.0.15 launched fine on the user's iPhone 13 Pro Max (iOS 16.6.1) and
played video, but the screenshot showed a pure-white strip, 80pt tall
(115px in a 612×1326 screenshot, 129px in the original 685×1484 one),
below the bottom tab bar. The shipped page already had `html { background:
#000 }` and `#root` at `100dvh`, yet the strip stayed white, so it is not
the page background: it is the WebView's own background showing where the
page does not paint. Measured geometry: the tab bar is 40pt + 34pt
(`--sab`, i.e. `env(safe-area-inset-bottom)` = 34), so the WebView does
reach the screen bottom, but the page ends at ~846pt of a 926pt screen —
`100dvh` in this WKWebView is inset by the top (47pt) + bottom (34pt)
safe areas. The companion fix is `100dvh` → `100lvh` in `src/index.css`
(the large viewport is the full viewport); this black fallback is the
second layer, so any area the page still does not paint reads as black
and blends with the dark UI instead of a white bar.

Keep this change on upgrade for the same reason: the app is dark-themed
and must never show a system-light background.
