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
