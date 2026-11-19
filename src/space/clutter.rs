//! Finds space that can usually be reclaimed: caches, build output, leftovers.

use crate::space::fmt;
use crate::space::tree::*;
use crate::space::win;
use std::collections::HashSet;
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Items smaller than this aren't worth listing.
const MIN_ITEM: u64 = 1 << 20;
const DAY: u64 = 864_000_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Safety {
    Safe,
    Review,
    Info,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Delete the folder or file itself.
    Item,
    /// Empty the folder but keep it.
    Contents,
    EmptyBin,
    /// Shown for information only.
    None,
}

#[derive(Clone, PartialEq)]
pub enum Status {
    Pending,
    Working,
    Done(String),
    Failed(String),
}

pub struct Item {
    pub label: String,
    pub note: &'static str,
    pub path: String,
    pub node: u32,
    pub alloc: u64,
    pub selected: bool,
    pub status: Status,
}

pub struct Group {
    pub title: &'static str,
    pub blurb: &'static str,
    pub safety: Safety,
    pub action: Action,
    pub items: Vec<Item>,
    pub open: bool,
}

impl Group {
    fn new(title: &'static str, blurb: &'static str, safety: Safety, action: Action) -> Group {
        Group { title, blurb, safety, action, items: Vec::new(), open: false }
    }

    pub fn deletable(&self) -> bool {
        self.action != Action::None
    }

    pub fn total(&self, tree: &Tree) -> u64 {
        self.live(tree).map(|it| it.alloc).sum()
    }

    /// Items still on disk.
    pub fn live<'a>(&'a self, tree: &'a Tree) -> impl Iterator<Item = &'a Item> + 'a {
        self.items
            .iter()
            .filter(move |it| !matches!(it.status, Status::Done(_)) && tree.node(it.node).kind != NONE)
    }
}

struct Collector<'a> {
    tree: &'a Tree,
    claimed: HashSet<u32>,
}

impl Collector<'_> {
    fn add(&mut self, g: &mut Group, label: impl Into<String>, path: &str) -> bool {
        match self.tree.find(path) {
            Some(n) => self.add_node(g, label, "", n),
            None => false,
        }
    }

    fn add_node(&mut self, g: &mut Group, label: impl Into<String>, note: &'static str, n: u32) -> bool {
        let nd = self.tree.node(n);
        if nd.alloc < MIN_ITEM || self.tree.ancestors(n).iter().any(|a| self.claimed.contains(a)) {
            return false;
        }
        self.claimed.insert(n);
        g.items.push(Item {
            label: label.into(),
            note,
            path: self.tree.path(n),
            node: n,
            alloc: nd.alloc,
            selected: false,
            status: Status::Pending,
        });
        true
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim_end_matches('\\').to_string())
}

pub fn detect(tree: &Tree) -> Vec<Group> {
    let home = env("USERPROFILE").unwrap_or_default();
    let local = env("LOCALAPPDATA").unwrap_or_else(|| format!("{home}\\AppData\\Local"));
    let roaming = env("APPDATA").unwrap_or_else(|| format!("{home}\\AppData\\Roaming"));
    let windir = env("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let progdata = env("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
    let temp = env("TEMP").unwrap_or_else(|| format!("{local}\\Temp"));
    let root = tree.node(tree.root).name.trim_end_matches('\\').to_string();

    let mut c = Collector { tree, claimed: HashSet::new() };

    let mut bin = Group::new(
        "Recycle Bin",
        "Files you've already deleted. Emptying the bin frees the space for good.",
        Safety::Safe,
        Action::EmptyBin,
    );
    c.add(&mut bin, "Recycle Bin", &format!("{root}\\$Recycle.Bin"));

    let mut temp_g = Group::new(
        "Temporary files",
        "Left behind by installers and apps. Anything still in use is skipped.",
        Safety::Safe,
        Action::Contents,
    );
    c.add(&mut temp_g, "Your temp folder", &temp);
    c.add(&mut temp_g, "Windows temp folder", &format!("{windir}\\Temp"));

    let mut browsers = Group::new(
        "Browser caches",
        "Pages and media browsers keep for faster loading. Close the browser first; it rebuilds the cache as you browse.",
        Safety::Safe,
        Action::Contents,
    );
    let chromium = [
        ("Chrome", format!("{local}\\Google\\Chrome\\User Data")),
        ("Edge", format!("{local}\\Microsoft\\Edge\\User Data")),
        ("Brave", format!("{local}\\BraveSoftware\\Brave-Browser\\User Data")),
        ("Vivaldi", format!("{local}\\Vivaldi\\User Data")),
        ("Opera", format!("{roaming}\\Opera Software\\Opera Stable")),
        ("Opera", format!("{local}\\Opera Software\\Opera Stable")),
    ];
    for (name, base) in &chromium {
        let Some(b) = tree.find(base) else { continue };
        for sub in ["ShaderCache", "GrShaderCache", "GraphiteDawnCache", "Cache", "Code Cache"] {
            c.add(&mut browsers, format!("{name} · {sub}"), &format!("{base}\\{sub}"));
        }
        let profiles: Vec<u32> = tree.kids(b).filter(|&k| tree.node(k).kind == DIR).collect();
        for prof in profiles {
            let pname = &tree.node(prof).name;
            let ppath = tree.path(prof);
            for sub in ["Cache", "Code Cache", "GPUCache", "Service Worker\\CacheStorage"] {
                let label = format!("{name} · {pname} · {}", sub.replace('\\', " "));
                c.add(&mut browsers, label, &format!("{ppath}\\{sub}"));
            }
        }
    }
    if let Some(ff) = tree.find(&format!("{local}\\Mozilla\\Firefox\\Profiles")) {
        let profiles: Vec<u32> = tree.kids(ff).collect();
        for prof in profiles {
            let label = format!("Firefox · {}", tree.node(prof).name);
            c.add(&mut browsers, label, &format!("{}\\cache2", tree.path(prof)));
        }
    }

    let mut apps = Group::new(
        "App caches",
        "Cached data from apps like Discord, VS Code and Spotify, plus graphics shader caches. Apps rebuild these when needed.",
        Safety::Safe,
        Action::Contents,
    );
    for (label, path) in [
        ("Internet cache", format!("{local}\\Microsoft\\Windows\\INetCache")),
        ("DirectX shader cache", format!("{local}\\D3DSCache")),
        ("NVIDIA DirectX cache", format!("{local}\\NVIDIA\\DXCache")),
        ("NVIDIA OpenGL cache", format!("{local}\\NVIDIA\\GLCache")),
        ("AMD shader cache", format!("{local}\\AMD\\DxCache")),
        ("Discord cache", format!("{roaming}\\discord\\Cache")),
        ("Discord code cache", format!("{roaming}\\discord\\Code Cache")),
        ("VS Code cache", format!("{roaming}\\Code\\Cache")),
        ("VS Code cached data", format!("{roaming}\\Code\\CachedData")),
        ("VS Code extension downloads", format!("{roaming}\\Code\\CachedExtensionVSIXs")),
        ("Slack cache", format!("{roaming}\\Slack\\Cache")),
        ("Spotify cache", format!("{local}\\Spotify\\Data")),
    ] {
        c.add(&mut apps, label, &path);
    }

    let mut dev = Group::new(
        "Developer caches",
        "Package manager downloads. They're fetched again the next time a project needs them.",
        Safety::Safe,
        Action::Contents,
    );
    for (label, path) in [
        ("npm cache", format!("{local}\\npm-cache")),
        ("Yarn cache", format!("{local}\\Yarn\\Cache")),
        ("pnpm store", format!("{local}\\pnpm\\store")),
        ("Bun cache", format!("{home}\\.bun\\install\\cache")),
        ("pip cache", format!("{local}\\pip\\Cache")),
        ("uv cache", format!("{local}\\uv\\cache")),
        ("NuGet packages", format!("{home}\\.nuget\\packages")),
        ("Gradle caches", format!("{home}\\.gradle\\caches")),
        ("Maven repository", format!("{home}\\.m2\\repository")),
        ("Cargo registry cache", format!("{home}\\.cargo\\registry\\cache")),
        ("Go build cache", format!("{local}\\go-build")),
        ("Conda packages", format!("{home}\\anaconda3\\pkgs")),
        ("Conda packages", format!("{home}\\miniconda3\\pkgs")),
        ("Electron downloads", format!("{local}\\electron\\Cache")),
        ("node-gyp headers", format!("{local}\\node-gyp\\Cache")),
        ("Composer cache", format!("{local}\\Composer")),
    ] {
        c.add(&mut dev, label, &path);
    }

    let mut crash = Group::new(
        "Crash dumps and error reports",
        "Memory dumps and reports from crashes. Only useful if you're debugging one. System locations need administrator rights.",
        Safety::Safe,
        Action::Contents,
    );
    for (label, path) in [
        ("App crash dumps", format!("{local}\\CrashDumps")),
        ("Error reports", format!("{local}\\Microsoft\\Windows\\WER")),
        ("System error reports", format!("{progdata}\\Microsoft\\Windows\\WER")),
        ("System minidumps", format!("{windir}\\Minidump")),
        ("Kernel crash reports", format!("{windir}\\LiveKernelReports")),
        ("Full memory dump", format!("{windir}\\MEMORY.DMP")),
    ] {
        c.add(&mut crash, label, &path);
    }

    let mut updates = Group::new(
        "Windows Update leftovers",
        "Update files Windows has already installed. Deleting them needs administrator rights.",
        Safety::Safe,
        Action::Contents,
    );
    c.add(&mut updates, "Downloaded updates", &format!("{windir}\\SoftwareDistribution\\Download"));
    c.add(
        &mut updates,
        "Delivery Optimization cache",
        &format!("{windir}\\ServiceProfiles\\NetworkService\\AppData\\Local\\Microsoft\\Windows\\DeliveryOptimization\\Cache"),
    );

    let mut models = Group::new(
        "AI models",
        "Downloaded model weights. You'll need to download them again to use them.",
        Safety::Review,
        Action::Contents,
    );
    for (label, path) in [
        ("Hugging Face cache", format!("{home}\\.cache\\huggingface")),
        ("Ollama models", format!("{home}\\.ollama\\models")),
        ("LM Studio models", format!("{home}\\.lmstudio\\models")),
        ("LM Studio models", format!("{home}\\.cache\\lm-studio\\models")),
    ] {
        c.add(&mut models, label, &path);
    }

    let mut downloads = Group::new(
        "Old downloads",
        "Installers, archives and disk images in Downloads that haven't changed in 30 days.",
        Safety::Review,
        Action::Item,
    );
    if let Some(dl) = tree.find(&format!("{home}\\Downloads")) {
        let cutoff = fmt::now_filetime().saturating_sub(30 * DAY);
        let mut buf = [0u8; 16];
        let mut stack = vec![dl];
        let mut found = Vec::new();
        while let Some(n) = stack.pop() {
            for k in tree.kids(n) {
                let nd = tree.node(k);
                if nd.kind == DIR {
                    stack.push(k);
                } else if nd.mtime < cutoff
                    && matches!(
                        lower_ext(&nd.name, &mut buf),
                        "exe" | "msi" | "msix" | "iso" | "img" | "zip" | "7z" | "rar" | "dmg" | "appx" | "tar" | "gz"
                    )
                {
                    found.push(k);
                }
            }
        }
        for k in found {
            c.add_node(&mut downloads, tree.node(k).name.to_string(), "", k);
        }
    }

    let mut vdisks = Group::new(
        "Virtual disks",
        "Disks for virtual machines, Docker and WSL. They grow as you use them and rarely shrink on their own. Remove a VM from its own app rather than deleting its disk. Docker and WSL disks can be compacted with  wsl --manage <distro> --set-sparse true.",
        Safety::Info,
        Action::None,
    );
    let mut buf = [0u8; 16];
    for i in 0..tree.nodes.len() {
        let nd = &tree.nodes[i];
        if nd.kind != FILE || nd.alloc < 512 << 20 {
            continue;
        }
        let ext = lower_ext(&nd.name, &mut buf);
        if ext == "vmem" {
            let label = format!("VMware suspended memory · {}", tree.node(nd.parent).name);
            c.add_node(&mut vdisks, label, "Saved RAM of a suspended or snapshotted VM. Discard it from VMware.", i as u32);
        } else if matches!(ext, "vhdx" | "vhd" | "vmdk" | "vdi" | "qcow2") {
            let path = tree.path(i as u32).to_ascii_lowercase();
            let label = if path.contains("docker") {
                "Docker disk".to_string()
            } else if path.contains("\\packages\\") || path.contains("wsl") {
                "WSL distro disk".to_string()
            } else {
                let vm = tree.node(nd.parent).name.to_string();
                match ext {
                    "vmdk" => format!("VMware · {vm}"),
                    "vdi" => format!("VirtualBox · {vm}"),
                    _ => format!("Virtual disk · {vm}"),
                }
            };
            c.add_node(&mut vdisks, label, "", i as u32);
        }
    }

    let mut system = Group::new(
        "System files",
        "Managed by Windows and can't be deleted here, but you can shrink or remove some of them in settings.",
        Safety::Info,
        Action::None,
    );
    for (name, label, note) in [
        ("hiberfil.sys", "Hibernation file", "Remove it by turning off hibernation: powercfg /h off"),
        ("pagefile.sys", "Page file", "Size it in System > About > Advanced system settings > Performance"),
        ("swapfile.sys", "Swap file", "Managed by Windows"),
        ("Windows.old", "Previous Windows installation", "Remove it in Settings > System > Storage > Temporary files"),
        (
            "System Volume Information",
            "Restore points and shadow copies",
            "Limit their size in System Protection (search Start for \"Create a restore point\")",
        ),
    ] {
        if let Some(n) = tree.find(&format!("{root}\\{name}")) {
            c.add_node(&mut system, label, note, n);
        }
    }

    let mut node_modules = Group::new(
        "node_modules folders",
        "Installed JavaScript dependencies. Run npm install (or your package manager) to bring a project's back.",
        Safety::Safe,
        Action::Item,
    );
    let mut build = Group::new(
        "Build output",
        "Compiler output and tool caches inside your projects. Building the project again recreates them.",
        Safety::Safe,
        Action::Item,
    );
    let mut venvs = Group::new(
        "Python virtual environments",
        "Created with venv. Make sure a project can recreate its environment (requirements.txt or pyproject.toml) before deleting it.",
        Safety::Review,
        Action::Item,
    );
    scan_projects(&mut c, tree.find(&home), &mut node_modules, &mut build, &mut venvs);

    let mut groups = vec![
        bin, temp_g, browsers, apps, dev, node_modules, build, crash, updates, venvs, models, downloads, vdisks,
        system,
    ];
    groups.retain(|g| !g.items.is_empty());
    for g in &mut groups {
        g.items.sort_unstable_by(|a, b| b.alloc.cmp(&a.alloc));
    }
    let rank = |s: Safety| match s {
        Safety::Safe => 0,
        Safety::Review => 1,
        Safety::Info => 2,
    };
    groups.sort_by(|a, b| rank(a.safety).cmp(&rank(b.safety)).then(b.total(tree).cmp(&a.total(tree))));
    groups
}

/// Walks user folders for project build artifacts. Skips Windows, Program Files and
/// AppData, where apps ship their own node_modules and bin folders.
fn scan_projects(c: &mut Collector, home: Option<u32>, nm: &mut Group, build: &mut Group, venvs: &mut Group) {
    let tree = c.tree;
    let mut stack = vec![(tree.root, 0u32)];
    while let Some((dir, depth)) = stack.pop() {
        let kids: Vec<u32> = tree.kids(dir).filter(|&k| tree.node(k).kind == DIR).collect();
        for k in kids {
            if c.claimed.contains(&k) {
                continue;
            }
            let name = tree.node(k).name.to_ascii_lowercase();
            if depth == 0
                && matches!(
                    name.as_str(),
                    "windows" | "program files" | "program files (x86)" | "programdata" | "recovery"
                )
            {
                continue;
            }
            if matches!(name.as_str(), "appdata" | ".git" | "$recycle.bin" | "system volume information")
                || name.starts_with('$')
            {
                continue;
            }
            // Dot-folders in the profile (.vscode, .lmstudio, .android…) hold tools' own files.
            if Some(dir) == home && name.starts_with('.') {
                continue;
            }
            let project = project_label(tree, dir);
            let sibling = |pred: &dyn Fn(&str) -> bool| {
                tree.kids(dir)
                    .any(|s| tree.node(s).kind == FILE && pred(&tree.node(s).name.to_ascii_lowercase()))
            };
            let found = match name.as_str() {
                "node_modules" => Some((&mut *nm, project.clone())),
                "target" if sibling(&|s| s == "cargo.toml") => Some((&mut *build, format!("{project} · Rust target"))),
                "bin" | "obj" if sibling(&|s| s.ends_with(".csproj") || s.ends_with(".fsproj") || s.ends_with(".vbproj")) => {
                    Some((&mut *build, format!("{project} · .NET {name}")))
                }
                "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".tox" => {
                    Some((&mut *build, format!("{project} · {name}")))
                }
                ".next" | ".nuxt" | ".turbo" | ".parcel-cache" | ".svelte-kit" | ".angular" | ".expo"
                    if sibling(&|s| s == "package.json") =>
                {
                    Some((&mut *build, format!("{project} · {name}")))
                }
                "build" | ".gradle" if sibling(&|s| s.starts_with("build.gradle") || s.starts_with("settings.gradle")) => {
                    Some((&mut *build, format!("{project} · Gradle {name}")))
                }
                _ if tree.kids(k).any(|s| tree.node(s).kind == FILE && eq_ci(&tree.node(s).name, "pyvenv.cfg")) => {
                    Some((&mut *venvs, format!("{project} · {}", tree.node(k).name)))
                }
                _ => None,
            };
            match found {
                Some((g, label)) => {
                    c.add_node(g, label, "", k);
                }
                None => stack.push((k, depth + 1)),
            }
        }
    }
}

/// A project's folder name, with parents prepended while the name is generic
/// ("boondi › android › app" rather than just "app").
fn project_label(tree: &Tree, dir: u32) -> String {
    let generic = |n: &str| {
        matches!(
            n.to_ascii_lowercase().as_str(),
            "app" | "apps" | "src" | "client" | "server" | "web" | "frontend" | "backend" | "android" | "ios"
                | "packages" | "lib" | "core" | "api" | "ui"
        )
    };
    let mut parts = vec![tree.node(dir).name.to_string()];
    let mut cur = dir;
    while generic(&parts[0]) && cur != tree.root && parts.len() < 3 {
        cur = tree.node(cur).parent;
        parts.insert(0, tree.node(cur).name.to_string());
    }
    parts.join(" › ")
}

pub struct Outcome {
    /// Entries that couldn't be removed (usually because they're in use).
    pub failed: u64,
    pub error: Option<String>,
}

pub fn delete(path: &str, action: Action, permanent: bool) -> Outcome {
    let p = Path::new(path);
    let ok = |failed| Outcome { failed, error: None };
    match action {
        Action::None => Outcome { failed: 0, error: Some("Can't be deleted here".into()) },
        Action::EmptyBin => match win::empty_recycle_bin(&path[..path.len().min(3)]) {
            Ok(()) => ok(0),
            Err(e) => Outcome { failed: 0, error: Some(e) },
        },
        Action::Item => {
            if permanent {
                ok(remove_tree(p))
            } else {
                match trash::delete(p) {
                    Ok(()) => ok(0),
                    Err(e) => Outcome { failed: 1, error: Some(e.to_string()) },
                }
            }
        }
        Action::Contents => {
            if fs::symlink_metadata(p).map(|m| !m.is_dir()).unwrap_or(false) {
                return delete(path, Action::Item, permanent);
            }
            let entries: Vec<PathBuf> = fs::read_dir(p)
                .map(|rd| rd.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            if permanent {
                return ok(entries.iter().map(|e| remove_tree(e)).sum());
            }
            if trash::delete_all(&entries).is_ok() {
                return ok(0);
            }
            // Batch failed (something is locked); go one by one so the rest still moves.
            ok(entries.iter().filter(|e| e.exists() && trash::delete(e).is_err()).count() as u64)
        }
    }
}

/// Deletes a file or folder tree, carrying on past files that are in use.
/// Never follows symlinks or junctions. Returns how many entries were left behind.
fn remove_tree(p: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(p) else { return 0 };
    let is_dir = md.file_attributes() & 0x10 != 0;
    if md.file_type().is_symlink() {
        let r = if is_dir { fs::remove_dir(p) } else { fs::remove_file(p) };
        return r.is_err() as u64;
    }
    if is_dir {
        let mut failed = 0;
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() {
                failed += remove_tree(&e.path());
            }
        }
        if fs::remove_dir(p).is_err() && failed == 0 {
            failed += 1;
        }
        return failed;
    }
    if fs::remove_file(p).is_ok() {
        return 0;
    }
    // Read-only files (common in node_modules and .git) need the flag cleared first.
    let mut perms = md.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    if fs::set_permissions(p, perms).is_ok() && fs::remove_file(p).is_ok() {
        0
    } else {
        1
    }
}
