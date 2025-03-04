use std::{
    collections::HashMap,
    error::Error,
    fs,
    io,
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Span, Spans},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

///////////////////////////////////////////////////////////////////////////////
// Configuration and VM Listing
///////////////////////////////////////////////////////////////////////////////

struct Config {
    remote_app: String,      // e.g. "remmina" (or native client on Windows/macOS)
    quickemu_dir: PathBuf,   // Directory with VM config files
    default_spice_port: u16, // Default SPICE port if not specified in VM config
    os_type: String,         // "windows", "macos", or "linux"
    // Override mapping: key = VM config file stem (lowercase), value = path to Remmina profile.
    remmina_overrides: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().expect("Unable to get home directory");
        let os_type = if cfg!(target_os = "windows") {
            "windows".to_string()
        } else if cfg!(target_os = "macos") {
            "macos".to_string()
        } else {
            "linux".to_string()
        };
        let remote_app = match os_type.as_str() {
            "windows" => "mstsc.exe".to_string(),
            "macos" => "open".to_string(),
            _ => "remmina".to_string(),
        };
        Self {
            remote_app,
            quickemu_dir: home.join(".quickemu"),
            default_spice_port: 5930,
            os_type,
            remmina_overrides: HashMap::new(),
        }
    }
}

/// Loads configuration from ~/.quick-cli.conf.
/// Lines starting with "override=" are interpreted as:
///     override=vm_stem, /path/to/remmina_profile.remmina
fn load_config() -> Config {
    let home = dirs::home_dir().expect("Unable to get home directory");
    let config_path = home.join(".quick-cli.conf");
    let os_type = if cfg!(target_os = "windows") {
        "windows".to_string()
    } else if cfg!(target_os = "macos") {
        "macos".to_string()
    } else {
        "linux".to_string()
    };
    let default_remote_app = match os_type.as_str() {
        "windows" => "mstsc.exe".to_string(),
        "macos" => "open".to_string(),
        _ => "remmina".to_string(),
    };
    let mut config = Config {
        remote_app: default_remote_app.clone(),
        quickemu_dir: home.join(".quickemu"),
        default_spice_port: 5930,
        os_type: os_type.clone(),
        remmina_overrides: HashMap::new(),
    };
    if !config_path.exists() {
        let default_config = format!(
            "remote_app={}\nquickemu_dir={}\ndefault_spice_port=5930\nos_type={}\n",
            default_remote_app,
            home.join(".quickemu").to_string_lossy(),
            os_type
        );
        let _ = fs::write(&config_path, default_config);
        return config;
    }
    let contents = fs::read_to_string(&config_path).unwrap_or_default();
    for line in contents.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "remote_app" => config.remote_app = value.trim().to_string(),
                "quickemu_dir" => config.quickemu_dir = PathBuf::from(value.trim()),
                "default_spice_port" => {
                    if let Ok(p) = value.trim().parse::<u16>() {
                        config.default_spice_port = p;
                    }
                }
                "os_type" => config.os_type = value.trim().to_string(),
                "override" => {
                    // Expected format: override=vm_stem, /path/to/remmina_profile.remmina
                    let parts: Vec<&str> = value.split(',').map(|s| s.trim()).collect();
                    if parts.len() == 2 {
                        config.remmina_overrides.insert(parts[0].to_lowercase(), parts[1].to_string());
                    }
                }
                _ => {}
            }
        }
    }
    config
}

/// List all VM configuration files (ending with ".conf") in the quickemu directory.
fn list_vms(config: &Config) -> Vec<PathBuf> {
    let mut vms = Vec::new();
    if let Ok(entries) = fs::read_dir(&config.quickemu_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "conf" {
                        vms.push(path);
                    }
                }
            }
        }
    }
    vms
}

///////////////////////////////////////////////////////////////////////////////
// Protocol Parsing and Running Detection
///////////////////////////////////////////////////////////////////////////////

enum RemoteProtocol {
    Rdp(u16),
    Vnc(u16),
    Spice(u16),
}

/// Parse the VM configuration.
/// If a "port_forwards" line is found for guest port 3389 or 5900, return Rdp or Vnc.
/// Otherwise, assume SPICE.
fn parse_vm_config(vm_conf: &Path, config: &Config) -> RemoteProtocol {
    if let Ok(contents) = fs::read_to_string(vm_conf) {
        for line in contents.lines() {
            if line.contains("port_forwards") {
                if let (Some(start), Some(end)) = (line.find('('), line.rfind(')')) {
                    let forwards_str = &line[start + 1..end];
                    let parts: Vec<&str> = forwards_str.split('"')
                        .filter(|s| !s.trim().is_empty())
                        .collect();
                    for mapping in parts {
                        let split: Vec<&str> = mapping.split(':').collect();
                        if split.len() == 2 {
                            if let Ok(guest_port) = split[1].parse::<u16>() {
                                if guest_port == 3389 {
                                    if let Ok(host_port) = split[0].parse::<u16>() {
                                        return RemoteProtocol::Rdp(host_port);
                                    }
                                } else if guest_port == 5900 {
                                    if let Ok(host_port) = split[0].parse::<u16>() {
                                        return RemoteProtocol::Vnc(host_port);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    RemoteProtocol::Spice(config.default_spice_port)
}

/// Check if a given host:port is open.
fn is_port_open(host: &str, port: u16, timeout: Duration) -> bool {
    let addr = format!("{}:{}", host, port);
    let socket_addr: SocketAddr = match addr.parse() {
        Ok(sa) => sa,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&socket_addr, timeout).is_ok()
}

#[cfg(unix)]
fn is_spice_vm_running(vm_conf: &Path, config: &Config) -> bool {
    let vm_stem = vm_conf.file_stem().unwrap().to_string_lossy();
    let socket_path = config.quickemu_dir.join(vm_stem.as_ref())
        .join(format!("{}-monitor.socket", vm_stem));
    if let Ok(meta) = fs::metadata(&socket_path) {
        if meta.mode() & 0o170000 == 0o140000 {
            if let Ok(modified) = meta.modified() {
                return modified.elapsed().unwrap_or(Duration::from_secs(100)) < Duration::from_secs(10);
            }
        }
    }
    false
}

#[cfg(not(unix))]
fn is_spice_vm_running(vm_conf: &Path, config: &Config) -> bool {
    is_port_open("127.0.0.1", config.default_spice_port, Duration::from_millis(200))
}

/// Determine if the VM is running.
fn is_vm_running(vm_conf: &Path, config: &Config) -> bool {
    match parse_vm_config(vm_conf, config) {
        RemoteProtocol::Rdp(port) | RemoteProtocol::Vnc(port) => is_port_open("127.0.0.1", port, Duration::from_millis(200)),
        RemoteProtocol::Spice(_) => is_spice_vm_running(vm_conf, config),
    }
}

///////////////////////////////////////////////////////////////////////////////
// Remmina Profile Override and Auto-Detection
///////////////////////////////////////////////////////////////////////////////

/// Returns a Remmina profile for the given VM.
/// First checks for an override mapping (exact match on the VM config’s stem, lowercase).
/// If not found, scans the default Remmina directory for files whose stem contains the VM stem.
/// If there is exactly one match or an exact match, that is returned.
fn remmina_profile_for_vm(vm_conf: &Path, config: &Config) -> Option<PathBuf> {
    let vm_stem = vm_conf.file_stem()?.to_string_lossy().to_lowercase();
    // Check for explicit override.
    if let Some(override_path) = config.remmina_overrides.get(&vm_stem) {
        return Some(PathBuf::from(override_path));
    }
    // Auto-detect: scan Remmina directory.
    let home = dirs::home_dir()?;
    let remmina_dir = home.join(".local/share/remmina");
    let mut matches = Vec::new();
    if let Ok(entries) = fs::read_dir(remmina_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "remmina" {
                        if let Some(stem) = path.file_stem() {
                            let profile_stem = stem.to_string_lossy().to_lowercase();
                            if profile_stem.contains(&vm_stem) {
                                matches.push(path);
                            }
                        }
                    }
                }
            }
        }
    }
    if matches.len() == 1 {
        return Some(matches.remove(0));
    }
    for m in &matches {
        if let Some(stem) = m.file_stem() {
            if stem.to_string_lossy().to_lowercase() == vm_stem {
                return Some(m.clone());
            }
        }
    }
    matches.into_iter().next()
}

///////////////////////////////////////////////////////////////////////////////
// VM Launching and Connection
///////////////////////////////////////////////////////////////////////////////

fn get_quickemu_cmd(config: &Config) -> String {
    if config.os_type == "windows" {
        "quickemu.exe".to_string()
    } else {
        "quickemu".to_string()
    }
}

fn start_vm(vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) {
    let vm_arg = vm_conf.as_os_str();
    let quickemu_cmd = get_quickemu_cmd(config);
    let mut cmd = match parse_vm_config(vm_conf, config) {
        RemoteProtocol::Rdp(_) | RemoteProtocol::Vnc(_) => {
            let mut l = logs.lock().unwrap();
            l.push(format!("Launching VM {} headless...", vm_conf.display()));
            drop(l);
            let mut command = Command::new(&quickemu_cmd);
            command.arg("--vm").arg(vm_arg).arg("--display").arg("none");
            command
        },
        _ => {
            let mut l = logs.lock().unwrap();
            l.push(format!("Launching VM {} normally...", vm_conf.display()));
            drop(l);
            let mut command = Command::new(&quickemu_cmd);
            command.arg("--vm").arg(vm_arg);
            command
        }
    };
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| thread::sleep(Duration::from_secs(2)))
        .map_err(|e| {
            let mut l = logs.lock().unwrap();
            l.push(format!("Error launching VM {}: {}", vm_conf.display(), e));
        });
}

/// Force a SPICE connection regardless of protocol.
fn force_spice_connect(vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) {
    let spice_port = config.default_spice_port;
    if config.os_type == "windows" {
        connect_spice_windows(spice_port, vm_conf, config, logs);
    } else if config.os_type == "macos" {
        connect_spice_macos(spice_port, vm_conf, config, logs);
    } else {
        connect_spice_linux(spice_port, vm_conf, config, logs);
    }
}

/// Connect to the VM.
/// First, if an override or auto-detected Remmina profile exists, launch Remmina with it
/// (using the "-c" flag) and return immediately.
/// Otherwise, use protocol-specific connection.
fn connect_vm(vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) {
    if let Some(profile_path) = remmina_profile_for_vm(vm_conf, config) {
        let mut l = logs.lock().unwrap();
        l.push(format!(
            "Profile found for {}. Launching Remmina with profile: {}",
            vm_conf.display(),
            profile_path.display()
        ));
        drop(l);
        let result = Command::new(&config.remote_app)
            .env("DISPLAY", ":0")
            .arg("-c")
            .arg(profile_path.to_str().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if result.is_ok() {
            return;
        } else {
            let mut l = logs.lock().unwrap();
            l.push("Failed to launch Remmina with profile; falling back to normal connection.".into());
            drop(l);
        }
    }
    let _ = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    let vm_name = vm_conf.file_stem().unwrap().to_string_lossy();
    match parse_vm_config(vm_conf, config) {
        RemoteProtocol::Rdp(host_port) => {
            if config.os_type == "windows" {
                if !connect_rdp_windows(host_port, &vm_name, logs) {
                    connect_spice_windows(config.default_spice_port, vm_conf, config, logs);
                }
            } else if config.os_type == "macos" {
                if !connect_rdp_macos(host_port, &vm_name, logs) {
                    connect_spice_macos(config.default_spice_port, vm_conf, config, logs);
                }
            } else {
                if !connect_rdp_linux(host_port, vm_conf, config, logs) {
                    connect_spice_linux(config.default_spice_port, vm_conf, config, logs);
                }
            }
        },
        RemoteProtocol::Vnc(host_port) => {
            if config.os_type == "windows" {
                if !connect_vnc_windows(host_port, &vm_name, logs) {
                    connect_spice_windows(config.default_spice_port, vm_conf, config, logs);
                }
            } else if config.os_type == "macos" {
                if !connect_vnc_macos(host_port, &vm_name, logs) {
                    connect_spice_macos(config.default_spice_port, vm_conf, config, logs);
                }
            } else {
                if !connect_vnc_linux(host_port, vm_conf, config, logs) {
                    connect_spice_linux(config.default_spice_port, vm_conf, config, logs);
                }
            }
        },
        RemoteProtocol::Spice(spice_port) => {
            if config.os_type == "windows" {
                connect_spice_windows(spice_port, vm_conf, config, logs);
            } else if config.os_type == "macos" {
                connect_spice_macos(spice_port, vm_conf, config, logs);
            } else {
                connect_spice_linux(spice_port, vm_conf, config, logs);
            }
        },
    }
}

///////////////////////////////////////////////////////////////////////////////
// Platform-Specific Connection Functions
///////////////////////////////////////////////////////////////////////////////

fn connect_rdp_windows(host_port: u16, _vm_name: &str, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let mut l = logs.lock().unwrap();
    l.push(format!("Connecting via Windows RDP to port {}", host_port));
    drop(l);
    let result = Command::new("mstsc.exe")
        .arg(format!("/v:127.0.0.1:{}", host_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    result.is_ok()
}

fn connect_rdp_macos(host_port: u16, _vm_name: &str, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let mut l = logs.lock().unwrap();
    l.push("Connecting via macOS RDP (Microsoft Remote Desktop)".into());
    drop(l);
    let url = format!("rdp://127.0.0.1:{}", host_port);
    let result = Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    result.is_ok()
}

fn connect_rdp_linux(host_port: u16, vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let _ = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    if let Some(profile_path) = remmina_profile_for_vm(vm_conf, config) {
        let mut l = logs.lock().unwrap();
        l.push(format!("Connecting using Remmina profile: {}", profile_path.display()));
        drop(l);
        let result = Command::new(&config.remote_app)
            .env("DISPLAY", ":0")
            .arg("--quiet")
            .arg("-c")
            .arg(profile_path.to_str().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if result.is_ok() { return true; }
    }
    let url = format!("rdp://127.0.0.1:{}", host_port);
    {
        let mut l = logs.lock().unwrap();
        l.push(format!("Connecting via RDP URL: {}", url));
    }
    let result = Command::new(&config.remote_app)
        .env("DISPLAY", ":0")
        .arg("--quiet")
        .arg("-p")
        .arg("rdp")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if result.is_ok() { return true; }
    let freerdp_result = Command::new("xfreerdp")
        .env("DISPLAY", ":0")
        .arg(format!("/v:127.0.0.1:{}", host_port))
        .arg("/f")
        .arg("/dynamic-resolution")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    freerdp_result.is_ok()
}

fn connect_vnc_windows(host_port: u16, _vm_name: &str, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let mut l = logs.lock().unwrap();
    l.push(format!("Connecting via Windows VNC to port {}", host_port));
    drop(l);
    let result = Command::new("tvnviewer")
        .arg(format!("127.0.0.1:{}", host_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if result.is_ok() { return true; }
    let result = Command::new("vncviewer")
        .arg(format!("127.0.0.1:{}", host_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    result.is_ok()
}

fn connect_vnc_macos(host_port: u16, _vm_name: &str, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let mut l = logs.lock().unwrap();
    l.push("Connecting via macOS Screen Sharing (VNC)".into());
    drop(l);
    let url = format!("vnc://127.0.0.1:{}", host_port);
    let result = Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    result.is_ok()
}

fn connect_vnc_linux(host_port: u16, vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let _ = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    if let Some(profile_path) = remmina_profile_for_vm(vm_conf, config) {
        let mut l = logs.lock().unwrap();
        l.push(format!("Connecting using Remmina profile: {}", profile_path.display()));
        drop(l);
        let result = Command::new(&config.remote_app)
            .env("DISPLAY", ":0")
            .arg("--quiet")
            .arg("-c")
            .arg(profile_path.to_str().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if result.is_ok() { return true; }
    }
    let url = format!("vnc://127.0.0.1:{}", host_port);
    {
        let mut l = logs.lock().unwrap();
        l.push(format!("Connecting via VNC URL: {}", url));
    }
    let result = Command::new(&config.remote_app)
        .env("DISPLAY", ":0")
        .arg("--quiet")
        .arg("-p")
        .arg("vnc")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if result.is_ok() { return true; }
    let result = Command::new("vncviewer")
        .env("DISPLAY", ":0")
        .arg(format!("127.0.0.1:{}", host_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    result.is_ok()
}

fn connect_spice_windows(spice_port: u16, vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let mut l = logs.lock().unwrap();
    l.push(format!("Connecting via SPICE on Windows to port {}", spice_port));
    drop(l);
    // If an override exists, try Remmina with it.
    if let Some(profile_path) = remmina_profile_for_vm(vm_conf, config) {
        let mut l = logs.lock().unwrap();
        l.push(format!("Using override Remmina profile for SPICE: {}", profile_path.display()));
        drop(l);
        let result = Command::new(&config.remote_app)
            .env("DISPLAY", ":0")
            .arg("-c")
            .arg(profile_path.to_str().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if result.is_ok() { return true; }
    }
    // Otherwise, use virt-viewer.
    let result = Command::new("virt-viewer")
        .arg(format!("spice://127.0.0.1:{}", spice_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    result.is_ok()
}

fn connect_spice_macos(spice_port: u16, vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let mut l = logs.lock().unwrap();
    l.push("Connecting via SPICE on macOS using Remote Viewer".into());
    drop(l);
    // If an override exists, use it.
    if let Some(profile_path) = remmina_profile_for_vm(vm_conf, config) {
        let mut l = logs.lock().unwrap();
        l.push(format!("Using override Remmina profile for SPICE: {}", profile_path.display()));
        drop(l);
        let result = Command::new(&config.remote_app)
            .env("DISPLAY", ":0")
            .arg("-c")
            .arg(profile_path.to_str().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if result.is_ok() { return true; }
    }
    let url = format!("spice://127.0.0.1:{}", spice_port);
    let result = Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    result.is_ok()
}

fn connect_spice_linux(spice_port: u16, vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) -> bool {
    let _ = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    if let Ok(result) = Command::new(&config.remote_app)
        .env("DISPLAY", ":0")
        .arg("--quiet")
        .arg("-p")
        .arg("spice")
        .arg(format!("spice://127.0.0.1:{}", spice_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if result.id() > 0 {
            return true;
        }
    }
    {
        let mut l = logs.lock().unwrap();
        l.push("Remmina SPICE launch failed, trying spicy...".into());
    }
    let result = Command::new("spicy")
        .env("DISPLAY", ":0")
        .arg("--title")
        .arg(vm_conf.file_stem().unwrap().to_string_lossy().as_ref())
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-p")
        .arg(spice_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if result.is_ok() { return true; }
    {
        let mut l = logs.lock().unwrap();
        l.push("spicy launch failed, trying remote-viewer...".into());
    }
    let result = Command::new("remote-viewer")
        .env("DISPLAY", ":0")
        .arg(format!("spice://127.0.0.1:{}", spice_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    result.is_ok()
}

///////////////////////////////////////////////////////////////////////////////
// Stop VM and App UI
///////////////////////////////////////////////////////////////////////////////

fn stop_vm(vm_conf: &Path, _config: &Config, logs: &Arc<Mutex<Vec<String>>>) {
    {
        let mut l = logs.lock().unwrap();
        l.push(format!("Stopping VM {}...", vm_conf.display()));
    }
    let vm_arg = vm_conf.as_os_str();
    let quickemu_cmd = if cfg!(target_os = "windows") {
        "quickemu.exe"
    } else {
        "quickemu"
    };
    let result = Command::new(quickemu_cmd)
        .arg("--kill")
        .arg("--vm")
        .arg(vm_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        Ok(_) => {
            let mut l = logs.lock().unwrap();
            l.push(format!("Stop command issued for {}.", vm_conf.display()));
        }
        Err(e) => {
            let mut l = logs.lock().unwrap();
            l.push(format!("Error stopping VM {}: {}", vm_conf.display(), e));
        }
    }
}

///////////////////////////////////////////////////////////////////////////////
// App UI
///////////////////////////////////////////////////////////////////////////////

use tui::widgets::ListState;

struct App {
    vm_list: Vec<PathBuf>,
    list_state: ListState,
    logs: Arc<Mutex<Vec<String>>>,
    spinner_index: usize,
}

impl App {
    fn new(vm_list: Vec<PathBuf>) -> Self {
        let mut list_state = ListState::default();
        if !vm_list.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            vm_list,
            list_state,
            logs: Arc::new(Mutex::new(vec!["Application started.".into()])),
            spinner_index: 0,
        }
    }
    fn update_spinner(&mut self) {
        self.spinner_index = (self.spinner_index + 1) % SPINNER_FRAMES.len();
    }
}

const SPINNER_FRAMES: [&str; 4] = ["-", "\\", "|", "/"];

///////////////////////////////////////////////////////////////////////////////
// Main Function
///////////////////////////////////////////////////////////////////////////////

fn main() -> Result<(), Box<dyn Error>> {
    let config = load_config();
    let vm_list = list_vms(&config);
    let mut app = App::new(vm_list);
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(200);
    loop {
        if last_tick.elapsed() >= tick_rate {
            app.update_spinner();
            last_tick = Instant::now();
        }
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(60),
                    Constraint::Percentage(30),
                    Constraint::Percentage(10),
                ].as_ref())
                .split(f.size());
            let items: Vec<ListItem> = app.vm_list.iter().map(|vm_conf| {
                let name = vm_conf.file_stem().unwrap().to_string_lossy().to_string();
                let mut display_text = name.clone();
                if is_vm_running(vm_conf, &config) {
                    let spinner = SPINNER_FRAMES[app.spinner_index];
                    display_text = format!("{} {}", spinner, name);
                }
                let span = if is_vm_running(vm_conf, &config) {
                    Span::styled(display_text, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
                } else {
                    Span::raw(display_text)
                };
                ListItem::new(Spans::from(span))
            }).collect();
            let vm_list_widget = List::new(items)
                .block(Block::default().title("Quick-CLI - VMs").borders(Borders::ALL))
                .highlight_symbol(">> ");
            f.render_stateful_widget(vm_list_widget, chunks[0], &mut app.list_state);
            let log_lines: Vec<Spans> = {
                let logs = app.logs.lock().unwrap();
                logs.iter().map(|line| Spans::from(Span::raw(line.clone()))).collect()
            };
            let logs_widget = Paragraph::new(log_lines)
                .block(Block::default().title("Logs").borders(Borders::ALL));
            f.render_widget(logs_widget, chunks[1]);
            let footer_text = Spans::from(vec![
                Span::raw("Keybindings: "),
                Span::styled("[r] Start", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("[Enter] Start & Connect", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("[c] Connect running", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("[v] Force Spice Connect", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("[s] Stop", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("[j/k] Navigate", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("[q] Quit", Style::default().fg(Color::Yellow)),
            ]);
            let footer_widget = Paragraph::new(footer_text)
                .block(Block::default().title("Footer").borders(Borders::ALL));
            f.render_widget(footer_widget, chunks[2]);
        })?;
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = match app.list_state.selected() {
                            Some(i) if i >= app.vm_list.len() - 1 => 0,
                            Some(i) => i + 1,
                            None => 0,
                        };
                        app.list_state.select(Some(i));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = match app.list_state.selected() {
                            Some(0) | None => app.vm_list.len() - 1,
                            Some(i) => i - 1,
                        };
                        app.list_state.select(Some(i));
                    }
                    KeyCode::Char('r') => {
                        if let Some(i) = app.list_state.selected() {
                            let vm_conf = &app.vm_list[i];
                            start_vm(vm_conf, &config, &app.logs);
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(i) = app.list_state.selected() {
                            let vm_conf = &app.vm_list[i];
                            start_vm(vm_conf, &config, &app.logs);
                            connect_vm(vm_conf, &config, &app.logs);
                        }
                    }
                    KeyCode::Char('c') => {
                        if let Some(i) = app.list_state.selected() {
                            let vm_conf = &app.vm_list[i];
                            if is_vm_running(vm_conf, &config) {
                                connect_vm(vm_conf, &config, &app.logs);
                            } else {
                                let mut l = app.logs.lock().unwrap();
                                l.push(format!("VM {} is not running; cannot connect.", vm_conf.display()));
                            }
                        }
                    }
                    KeyCode::Char('v') => {
                        if let Some(i) = app.list_state.selected() {
                            let vm_conf = &app.vm_list[i];
                            let mut l = app.logs.lock().unwrap();
                            l.push(format!("Force SPICE connect for {}.", vm_conf.display()));
                            drop(l);
                            force_spice_connect(vm_conf, &config, &app.logs);
                        }
                    }
                    KeyCode::Char('s') => {
                        if let Some(i) = app.list_state.selected() {
                            let vm_conf = &app.vm_list[i];
                            stop_vm(vm_conf, &config, &app.logs);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
