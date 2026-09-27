//! Browser fingerprint hardening for the CDP renderer: launch flags, a
//! fingerprint-patching script run in every document, a user agent without
//! "HeadlessChrome" (with matching client hints), and per-page mobile
//! emulation.

use chromiumoxide::cdp::browser_protocol::emulation::{
    ScreenOrientation, ScreenOrientationType, SetDeviceMetricsOverrideParams,
    SetTouchEmulationEnabledParams, SetUserAgentOverrideParams, UserAgentBrandVersion,
    UserAgentMetadata,
};
use chromiumoxide::Page;

/// Chrome launch flags used instead of chromiumoxide's defaults: the same
/// list minus `--enable-automation` (which sets `navigator.webdriver` and
/// the automation infobar) and `--disable-popup-blocking` (popups opened by
/// scripts or actions stay blocked).
pub(crate) const LAUNCH_ARGS: &[&str] = &[
    "--disable-background-networking",
    "--enable-features=NetworkService,NetworkServiceInProcess",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-extensions",
    "--disable-features=TranslateUI",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--disable-sync",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--no-first-run",
    "--password-store=basic",
    "--use-mock-keychain",
    "--lang=en-US",
    // Drops the `navigator.webdriver = true` Blink sets for automation.
    "--disable-blink-features=AutomationControlled",
];

/// Accept-Language sent with the patched user agent (and matching
/// `navigator.languages`).
const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// Fingerprint patches run in every frame before any page script
/// (`Page.addScriptToEvaluateOnNewDocument`). Patched functions are wrapped
/// in `Proxy`s so `Function.prototype.toString` still reports native code.
const STEALTH_SCRIPT: &str = r#"(() => {
  const define = (obj, prop, get) => {
    try { Object.defineProperty(obj, prop, { get, configurable: true, enumerable: true }); } catch (_) {}
  };
  // navigator.webdriver: undefined, as in a browser nobody automates.
  define(Navigator.prototype, 'webdriver', () => undefined);
  // Languages matching the Accept-Language header.
  define(Navigator.prototype, 'languages', () => Object.freeze(['en-US', 'en']));
  // Headless builds may expose no plugins; real Chrome lists the PDF viewers.
  try {
    if (!navigator.plugins || navigator.plugins.length === 0) {
      const names = ['PDF Viewer', 'Chrome PDF Viewer', 'Chromium PDF Viewer', 'Microsoft Edge PDF Viewer', 'WebKit built-in PDF'];
      const mime = { type: 'application/pdf', suffixes: 'pdf', description: 'Portable Document Format' };
      const plugins = names.map((name) => ({ name, filename: 'internal-pdf-viewer', description: 'Portable Document Format', length: 1, 0: mime, item: () => mime, namedItem: () => mime }));
      const list = Object.assign(Object.create(PluginArray.prototype), plugins, {
        length: plugins.length,
        item: (i) => plugins[i] || null,
        namedItem: (n) => plugins.find((p) => p.name === n) || null,
        refresh: () => {},
      });
      define(Navigator.prototype, 'plugins', () => list);
    }
  } catch (_) {}
  // window.chrome exists in real Chrome (headless builds lack parts of it).
  try {
    if (!window.chrome) {
      Object.defineProperty(window, 'chrome', { value: {}, writable: true, configurable: true });
    }
    if (!window.chrome.runtime) window.chrome.runtime = {};
    if (!window.chrome.app) {
      window.chrome.app = { isInstalled: false, InstallState: { DISABLED: 'disabled', INSTALLED: 'installed', NOT_INSTALLED: 'not_installed' }, RunningState: { CANNOT_RUN: 'cannot_run', READY_TO_RUN: 'ready_to_run', RUNNING: 'running' } };
    }
    if (!window.chrome.csi) window.chrome.csi = () => ({ onloadT: Date.now(), startE: Date.now(), pageT: performance.now(), tran: 15 });
    if (!window.chrome.loadTimes) window.chrome.loadTimes = () => ({});
  } catch (_) {}
  // Notification permission queries answer like a real browser.
  try {
    const query = Permissions.prototype.query;
    Permissions.prototype.query = new Proxy(query, {
      apply(target, self, args) {
        const p = args[0];
        if (p && p.name === 'notifications') {
          return Promise.resolve({ state: Notification.permission, onchange: null });
        }
        return Reflect.apply(target, self, args);
      },
    });
  } catch (_) {}
  // WebGL vendor/renderer: a real GPU instead of SwiftShader.
  const patchWebGl = (proto) => {
    if (!proto) return;
    proto.getParameter = new Proxy(proto.getParameter, {
      apply(target, self, args) {
        if (args[0] === 37445) return __WEBGL_VENDOR__;
        if (args[0] === 37446) return __WEBGL_RENDERER__;
        return Reflect.apply(target, self, args);
      },
    });
  };
  try { patchWebGl(window.WebGLRenderingContext && WebGLRenderingContext.prototype); } catch (_) {}
  try { patchWebGl(window.WebGL2RenderingContext && WebGL2RenderingContext.prototype); } catch (_) {}
})();"#;

/// The fingerprint-patching script for a browser presenting `identity`
/// (the WebGL vendor/renderer match its platform).
pub(crate) fn stealth_script(identity: Option<&BrowserIdentity>) -> String {
    let (vendor, renderer) = match identity.map(|i| i.hints_platform) {
        Some("Android") => ("Qualcomm", "Adreno (TM) 740"),
        Some("macOS") => (
            "Google Inc. (Apple)",
            "ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unsupported)",
        ),
        Some("Windows") => (
            "Google Inc. (Intel)",
            "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)",
        ),
        _ => (
            "Google Inc. (Intel)",
            "ANGLE (Intel, Mesa Intel(R) UHD Graphics 630 (CFL GT2), OpenGL 4.6)",
        ),
    };
    let quote = |s: &str| serde_json::to_string(s).expect("strings serialize");
    STEALTH_SCRIPT
        .replace("__WEBGL_VENDOR__", &quote(vendor))
        .replace("__WEBGL_RENDERER__", &quote(renderer))
}

/// The browser's user agent with "HeadlessChrome" replaced by "Chrome".
pub(crate) fn normalize_user_agent(ua: &str) -> String {
    ua.replace("HeadlessChrome", "Chrome")
}

/// Chrome's major version from a user agent (`Chrome/140.0...` → `140`).
pub(crate) fn chrome_major(ua: &str) -> Option<String> {
    let rest = &ua[ua.find("Chrome/")? + "Chrome/".len()..];
    let major: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!major.is_empty()).then_some(major)
}

/// Identity the browser presents: user agent plus the client hints and
/// `navigator.platform` that go with it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BrowserIdentity {
    pub user_agent: String,
    pub platform: &'static str,
    pub hints_platform: &'static str,
    pub platform_version: &'static str,
    pub architecture: &'static str,
    pub model: &'static str,
    pub mobile: bool,
    pub major: String,
}

impl BrowserIdentity {
    /// The desktop identity for `ua` (the browser's own, normalized).
    pub(crate) fn desktop(ua: &str) -> Self {
        let user_agent = normalize_user_agent(ua);
        let major = chrome_major(&user_agent).unwrap_or_else(|| "140".to_string());
        let (platform, hints_platform, platform_version, architecture) =
            if user_agent.contains("Windows") {
                ("Win32", "Windows", "10.0.0", "x86")
            } else if user_agent.contains("Macintosh") {
                ("MacIntel", "macOS", "14.0.0", "arm")
            } else {
                ("Linux x86_64", "Linux", "6.5.0", "x86")
            };
        Self {
            user_agent,
            platform,
            hints_platform,
            platform_version,
            architecture,
            model: "",
            mobile: false,
            major,
        }
    }

    /// An Android phone running the same Chrome version, in Chrome's
    /// reduced user-agent format.
    pub(crate) fn mobile(major: &str) -> Self {
        Self {
            user_agent: format!(
                "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Mobile Safari/537.36"
            ),
            platform: "Linux armv81",
            hints_platform: "Android",
            platform_version: "14.0.0",
            architecture: "",
            model: "Pixel 8",
            mobile: true,
            major: major.to_string(),
        }
    }

    fn metadata(&self) -> UserAgentMetadata {
        let brands = vec![
            UserAgentBrandVersion::new("Chromium", self.major.clone()),
            UserAgentBrandVersion::new("Google Chrome", self.major.clone()),
            UserAgentBrandVersion::new("Not=A?Brand", "24"),
        ];
        let full = brands
            .iter()
            .map(|b| {
                let version = if b.version.contains('.') {
                    b.version.clone()
                } else {
                    format!("{}.0.0.0", b.version)
                };
                UserAgentBrandVersion::new(b.brand.clone(), version)
            })
            .collect();
        UserAgentMetadata {
            brands: Some(brands),
            full_version_list: Some(full),
            platform: self.hints_platform.to_string(),
            platform_version: self.platform_version.to_string(),
            architecture: self.architecture.to_string(),
            model: self.model.to_string(),
            mobile: self.mobile,
            bitness: Some(if self.mobile { "" } else { "64" }.to_string()),
            wow64: Some(false),
        }
    }

    /// Apply to one page (`Emulation.setUserAgentOverride`).
    pub(crate) async fn apply(&self, page: &Page) -> Result<(), String> {
        let mut params = SetUserAgentOverrideParams::new(self.user_agent.clone());
        params.accept_language = Some(ACCEPT_LANGUAGE.to_string());
        params.platform = Some(self.platform.to_string());
        params.user_agent_metadata = Some(self.metadata());
        page.execute(params)
            .await
            .map(|_| ())
            .map_err(|e| format!("set user agent: {e}"))
    }
}

/// Phone viewport used for mobile emulation (Pixel 8 class).
pub(crate) const MOBILE_WIDTH: i64 = 412;
pub(crate) const MOBILE_HEIGHT: i64 = 915;
const MOBILE_SCALE: f64 = 2.625;

/// Pin a desktop page's viewport to `width`x`height` CSS pixels (no touch,
/// scale 1). Without it the viewport is the headless window's content area,
/// which is smaller than `--window-size` on some platforms.
pub(crate) async fn emulate_desktop(page: &Page, width: u32, height: u32) -> Result<(), String> {
    page.execute(SetDeviceMetricsOverrideParams::new(
        i64::from(width),
        i64::from(height),
        1.0,
        false,
    ))
    .await
    .map(|_| ())
    .map_err(|e| format!("device metrics: {e}"))
}

/// Emulate a phone on this page: mobile device metrics, touch input.
pub(crate) async fn emulate_mobile(page: &Page) -> Result<(), String> {
    let mut metrics =
        SetDeviceMetricsOverrideParams::new(MOBILE_WIDTH, MOBILE_HEIGHT, MOBILE_SCALE, true);
    metrics.screen_width = Some(MOBILE_WIDTH);
    metrics.screen_height = Some(MOBILE_HEIGHT);
    metrics.screen_orientation = Some(ScreenOrientation::new(
        ScreenOrientationType::PortraitPrimary,
        0,
    ));
    page.execute(metrics)
        .await
        .map_err(|e| format!("mobile device metrics: {e}"))?;
    let mut touch = SetTouchEmulationEnabledParams::new(true);
    touch.max_touch_points = Some(5);
    page.execute(touch)
        .await
        .map_err(|e| format!("touch emulation: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADLESS_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/149.0.0.0 Safari/537.36";

    #[test]
    fn user_agent_drops_headless_marker() {
        let id = BrowserIdentity::desktop(HEADLESS_MAC);
        assert!(!id.user_agent.contains("Headless"), "{}", id.user_agent);
        assert!(id.user_agent.contains("Chrome/149.0.0.0"));
        assert_eq!(id.major, "149");
        assert_eq!(id.platform, "MacIntel");
        assert!(!id.mobile);
    }

    #[test]
    fn desktop_platform_follows_the_user_agent() {
        let linux = BrowserIdentity::desktop(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/131.0.6778.85 Safari/537.36",
        );
        assert_eq!(
            (linux.platform, linux.hints_platform),
            ("Linux x86_64", "Linux")
        );
        assert_eq!(linux.major, "131");
        let win = BrowserIdentity::desktop(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        );
        assert_eq!(win.platform, "Win32");
    }

    #[test]
    fn mobile_identity_is_android_chrome() {
        let id = BrowserIdentity::mobile("149");
        assert!(id.user_agent.contains("Android"));
        assert!(id.user_agent.contains("Mobile Safari"));
        assert!(id.user_agent.contains("Chrome/149.0.0.0"));
        assert!(id.mobile);
        let meta = id.metadata();
        assert!(meta.mobile);
        assert_eq!(meta.platform, "Android");
    }

    #[test]
    fn launch_args_never_enable_automation() {
        assert!(!LAUNCH_ARGS.contains(&"--enable-automation"));
        assert!(LAUNCH_ARGS.contains(&"--disable-blink-features=AutomationControlled"));
    }

    #[test]
    fn stealth_script_matches_platform() {
        let mac = BrowserIdentity::desktop(HEADLESS_MAC);
        let script = stealth_script(Some(&mac));
        assert!(script.contains("Apple M1"));
        assert!(!script.contains("__WEBGL"));
        assert!(stealth_script(Some(&BrowserIdentity::mobile("149"))).contains("Adreno"));
        assert!(stealth_script(None).contains("Mesa"));
    }

    #[test]
    fn chrome_major_parses() {
        assert_eq!(chrome_major("x Chrome/123.4 y").as_deref(), Some("123"));
        assert_eq!(chrome_major("Firefox/1"), None);
    }
}
