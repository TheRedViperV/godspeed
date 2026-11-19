//! Things to launch rather than files: installed apps as the Start menu lists them
//! (classic programs and Store apps alike), Settings pages, and Windows' admin
//! tools, so "remote desktop", "wallpaper" or "group policy" find the real thing.

use std::collections::HashSet;
use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const SETTINGS_APP: &str = "windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel";
const SECURITY_APP: &str = "Microsoft.SecHealthUI_8wekyb3d8bbwe!SecHealthUI";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    App,
    Setting,
    Tool,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::App => "App",
            Kind::Setting => "Settings",
            Kind::Tool => "System tool",
        }
    }
}

#[derive(Clone)]
pub struct StartApp {
    pub name: String,
    pub lower: String,
    /// Name plus other words people use for it ("wallpaper" for Background).
    words: String,
    /// App ID, settings URI, or a program (with arguments) to run.
    target: String,
    pub kind: Kind,
}

/// Settings pages: (name, other words, URI).
const SETTINGS: &[(&str, &str, &str)] = &[
    ("Display", "screen resolution brightness scale monitor", "ms-settings:display"),
    ("Advanced display", "refresh rate hz monitor", "ms-settings:display-advanced"),
    ("Graphics", "gpu preference hardware accelerated scheduling", "ms-settings:display-advancedgraphics"),
    ("HDR", "high dynamic range", "ms-settings:display-hdr"),
    ("Night light", "blue light warm", "ms-settings:nightlight"),
    ("Sound", "audio volume speakers microphone output input", "ms-settings:sound"),
    ("Notifications", "alerts", "ms-settings:notifications"),
    ("Focus", "do not disturb quiet", "ms-settings:quiethours"),
    ("Power & battery", "sleep battery power mode", "ms-settings:powersleep"),
    ("Storage", "disk space cleanup storage sense", "ms-settings:storagesense"),
    ("Multitasking", "snap windows alt tab", "ms-settings:multitasking"),
    ("Clipboard", "clipboard history copy paste", "ms-settings:clipboard"),
    ("Remote Desktop settings", "rdp remote access", "ms-settings:remotedesktop"),
    ("About this PC", "pc name rename specs system info", "ms-settings:about"),
    ("Bluetooth & devices", "bluetooth pair headphones", "ms-settings:bluetooth"),
    ("Printers & scanners", "printer scanner print", "ms-settings:printers"),
    ("Mouse", "cursor pointer speed scroll", "ms-settings:mousetouchpad"),
    ("Touchpad", "trackpad gestures", "ms-settings:devices-touchpad"),
    ("Typing", "keyboard autocorrect spelling", "ms-settings:typing"),
    ("AutoPlay", "usb removable", "ms-settings:autoplay"),
    ("Network & internet", "network internet connection", "ms-settings:network"),
    ("Wi-Fi", "wifi wireless internet network", "ms-settings:network-wifi"),
    ("Ethernet", "wired network cable", "ms-settings:network-ethernet"),
    ("VPN", "virtual private network", "ms-settings:network-vpn"),
    ("Proxy", "proxy server", "ms-settings:network-proxy"),
    ("Airplane mode", "flight mode", "ms-settings:network-airplanemode"),
    ("Mobile hotspot", "hotspot tethering share internet", "ms-settings:network-mobilehotspot"),
    ("Personalization", "customize look", "ms-settings:personalization"),
    ("Background", "wallpaper desktop picture", "ms-settings:personalization-background"),
    ("Colors", "dark mode light mode theme accent color", "ms-settings:colors"),
    ("Themes", "theme", "ms-settings:themes"),
    ("Lock screen", "lockscreen", "ms-settings:lockscreen"),
    ("Start menu settings", "start pins recommended", "ms-settings:personalization-start"),
    ("Taskbar", "taskbar icons tray", "ms-settings:taskbar"),
    ("Fonts", "font typeface", "ms-settings:fonts"),
    ("Installed apps", "uninstall remove programs apps features", "ms-settings:appsfeatures"),
    ("Default apps", "open with default browser file association", "ms-settings:defaultapps"),
    ("Startup apps", "startup boot launch at login", "ms-settings:startupapps"),
    ("Optional features", "add feature", "ms-settings:optionalfeatures"),
    ("Your info", "account profile picture", "ms-settings:yourinfo"),
    ("Sign-in options", "password pin windows hello fingerprint face", "ms-settings:signinoptions"),
    ("Email & accounts", "email accounts microsoft account", "ms-settings:emailandaccounts"),
    ("Date & time", "clock time zone timezone", "ms-settings:dateandtime"),
    ("Language & region", "language region keyboard layout", "ms-settings:regionlanguage"),
    ("Speech", "voice speech recognition", "ms-settings:speech"),
    ("Game Mode", "gaming game mode performance", "ms-settings:gaming-gamemode"),
    ("Game Bar", "xbox game bar overlay", "ms-settings:gaming-gamebar"),
    ("Captures", "record screen recording clips", "ms-settings:gaming-gamedvr"),
    ("Accessibility", "ease of access", "ms-settings:easeofaccess"),
    ("Text size", "bigger text font size", "ms-settings:easeofaccess-display"),
    ("Privacy & security", "privacy permissions", "ms-settings:privacy"),
    ("Location", "gps location services", "ms-settings:privacy-location"),
    ("Camera privacy", "webcam camera permissions", "ms-settings:privacy-webcam"),
    ("Microphone privacy", "mic microphone permissions", "ms-settings:privacy-microphone"),
    ("Windows Security", "antivirus virus defender protection firewall", "windowsdefender:"),
    ("Windows Update", "updates update patch", "ms-settings:windowsupdate"),
    ("Recovery", "reset this pc restore", "ms-settings:recovery"),
    ("Activation", "product key license activate", "ms-settings:activation"),
    ("For developers", "developer mode sudo", "ms-settings:developers"),
    ("Troubleshoot", "fix problems troubleshooter", "ms-settings:troubleshoot"),
    ("Windows Backup", "backup onedrive", "ms-settings:backup"),
];

/// Admin tools and classic control panels: (name, other words, program in
/// System32 or Windows, optional arguments).
const TOOLS: &[(&str, &str, &str, &str)] = &[
    ("Edit group policy", "gpedit group policy local policies", "System32\\gpedit.msc", ""),
    ("Local Security Policy", "secpol security policy", "System32\\secpol.msc", ""),
    ("Device Manager", "devmgmt drivers hardware devices", "System32\\devmgmt.msc", ""),
    ("Disk Management", "diskmgmt partition format volume drive", "System32\\diskmgmt.msc", ""),
    ("Services", "services.msc windows services", "System32\\services.msc", ""),
    ("Event Viewer", "eventvwr logs errors events", "System32\\eventvwr.msc", ""),
    ("Task Scheduler", "taskschd scheduled tasks", "System32\\taskschd.msc", ""),
    ("Computer Management", "compmgmt", "System32\\compmgmt.msc", ""),
    ("Local Users and Groups", "lusrmgr users accounts groups", "System32\\lusrmgr.msc", ""),
    ("Certificates", "certmgr certificate", "System32\\certmgr.msc", ""),
    ("Performance Monitor", "perfmon performance", "System32\\perfmon.msc", ""),
    ("Registry Editor", "regedit registry", "regedit.exe", ""),
    ("Task Manager", "taskmgr processes kill", "System32\\taskmgr.exe", ""),
    ("System Configuration", "msconfig boot safe mode startup", "System32\\msconfig.exe", ""),
    ("System Information", "msinfo32 specs hardware", "System32\\msinfo32.exe", ""),
    ("Resource Monitor", "resmon cpu memory disk network usage", "System32\\resmon.exe", ""),
    ("Control Panel", "control panel classic", "System32\\control.exe", ""),
    ("Programs and Features", "appwiz uninstall add remove programs", "System32\\appwiz.cpl", ""),
    ("Network Connections", "ncpa adapters network adapter", "System32\\ncpa.cpl", ""),
    ("System Properties", "sysdm advanced system settings computer name", "System32\\sysdm.cpl", ""),
    ("Environment Variables", "env path variables", "System32\\rundll32.exe", "sysdm.cpl,EditEnvironmentVariables"),
    ("Power Options", "powercfg power plan", "System32\\powercfg.cpl", ""),
    ("Sound devices", "mmsys playback recording devices", "System32\\mmsys.cpl", ""),
    ("Mouse properties", "main.cpl pointer", "System32\\main.cpl", ""),
    ("Internet Options", "inetcpl internet properties", "System32\\inetcpl.cpl", ""),
    ("Windows Defender Firewall", "firewall ports", "System32\\firewall.cpl", ""),
    ("Windows Features", "optionalfeatures turn windows features on off hyper-v wsl", "System32\\optionalfeatures.exe", ""),
    ("Credential Manager", "passwords saved credentials", "System32\\control.exe", "/name Microsoft.CredentialManager"),
    ("Disk Cleanup", "cleanmgr free space junk", "System32\\cleanmgr.exe", ""),
    ("Defragment and Optimize Drives", "defrag dfrgui optimize trim", "System32\\dfrgui.exe", ""),
];

fn start_apps() -> Vec<StartApp> {
    let script = "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
                  Get-StartApps | ForEach-Object { $_.Name + \"`t\" + $_.AppID }";
    let Ok(out) = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut apps = Vec::new();
    for line in text.lines() {
        let Some((name, id)) = line.split_once('\t') else { continue };
        let (name, id) = (name.trim(), id.trim());
        let lower = name.to_lowercase();
        if name.is_empty() || id.is_empty() || lower.starts_with("uninstall") {
            continue;
        }
        apps.push(StartApp { name: name.into(), words: lower.clone(), lower, target: id.into(), kind: Kind::App });
    }
    apps
}

/// Installed apps, then Settings pages and tools (skipping any the Start menu
/// already lists under the same name).
pub fn load() -> Vec<StartApp> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let mut seen = HashSet::new();
    let mut all: Vec<StartApp> = start_apps().into_iter().filter(|a| seen.insert(a.lower.clone())).collect();
    for &(name, words, uri) in SETTINGS {
        let lower = name.to_lowercase();
        if seen.insert(lower.clone()) {
            let words = format!("{lower} {words}");
            all.push(StartApp { name: name.into(), lower, words, target: uri.into(), kind: Kind::Setting });
        }
    }
    for &(name, words, program, args) in TOOLS {
        let lower = name.to_lowercase();
        let path = format!("{windir}\\{program}");
        if !std::path::Path::new(&path).exists() || !seen.insert(lower.clone()) {
            continue; // e.g. gpedit isn't on Windows Home
        }
        let target = if args.is_empty() { path } else { format!("{path}|{args}") };
        let words = format!("{lower} {words}");
        all.push(StartApp { name: name.into(), lower, words, target, kind: Kind::Tool });
    }
    all
}

fn shell_path(id: &str) -> String {
    format!("shell:AppsFolder\\{id}")
}

/// Where the shell can find this item's icon.
pub fn icon_path(a: &StartApp) -> String {
    match a.kind {
        Kind::App => shell_path(&a.target),
        Kind::Setting if a.target.starts_with("windowsdefender") => shell_path(SECURITY_APP),
        Kind::Setting => shell_path(SETTINGS_APP),
        Kind::Tool => a.target.split('|').next().unwrap_or_default().to_string(),
    }
}

/// Settings pages all share the Settings icon.
pub fn icon_key(a: &StartApp) -> String {
    match a.kind {
        Kind::Setting => format!("app:{}", icon_path(a)),
        _ => format!("app:{}", a.target),
    }
}

/// Opens it the way the Start menu does. Going through Explorer means it runs
/// without Godspeed's admin rights; tools that need them ask for them.
pub fn launch(a: &StartApp) {
    let _ = match (a.kind, a.target.split_once('|')) {
        (Kind::App, _) => Command::new("explorer.exe").raw_arg(shell_path(&a.target)).spawn(),
        (Kind::Tool, Some((program, args))) => Command::new(program).raw_arg(args).spawn(),
        _ => Command::new("explorer.exe").raw_arg(format!("\"{}\"", a.target)).spawn(),
    };
}

/// The key `History` remembers an app by.
pub fn history_key(a: &StartApp) -> String {
    format!("app:{}", a.lower)
}

/// First letters of each word: "Remote Desktop Connection" → "rdc".
fn initials(lower: &str) -> String {
    lower.split(|c: char| !c.is_alphanumeric()).filter_map(|w| w.chars().next()).collect()
}

/// Items whose name or other words contain every word typed (or whose initials
/// start with it), best first. `boost` lifts ones you've opened before.
pub fn find(apps: &[StartApp], text: &str, boost: &dyn Fn(&StartApp) -> u8) -> Vec<usize> {
    let q = text.trim().to_lowercase();
    if q.chars().count() < 2 || q.contains([':', '*', '?', '\\', '/', '!', '"']) {
        return Vec::new();
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    let acronym = |a: &StartApp| words.len() == 1 && q.len() >= 2 && initials(&a.lower).starts_with(q.as_str());
    let mut found: Vec<(i32, u8, usize, usize)> = apps
        .iter()
        .enumerate()
        .filter(|(_, a)| words.iter().all(|w| a.words.contains(w)) || acronym(a))
        .map(|(i, a)| {
            let in_name = words.iter().all(|w| a.lower.contains(w));
            let word_start = a.lower.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(words[0]));
            let rank = if a.lower.starts_with(&q) {
                0
            } else if (in_name && word_start) || acronym(a) {
                1
            } else if in_name {
                2
            } else {
                3 // matched only on other words
            };
            // On a tie, the app itself comes before its settings page.
            (rank * 20 - boost(a) as i32, a.kind as u8, a.name.len(), i)
        })
        .collect();
    found.sort();
    found.into_iter().take(8).map(|f| f.3).collect()
}
