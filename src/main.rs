use std::{
    error::Error,
    fs,
    io,
    net::TcpStream,
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
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

struct Config {
    remote_app: String,
    quickemu_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().expect("Unable to get home directory");
        Self {
            remote_app: "remmina".to_string(),
            quickemu_dir: home.join(".quickemu"),
        }
    }
}

fn load_config() -> Config {
    let home = dirs::home_dir().expect("Unable to get home directory");
    let config_path = home.join(".quick-cli.conf");
    if !config_path.exists() {
        let default_config = format!(
            "remote_app=remmina\nquickemu_dir={}\n",
            home.join(".quickemu").to_string_lossy()
        );
        if let Err(e) = fs::write(&config_path, default_config) {
            eprintln!("Failed to create default config file: {}", e);
        }
        return Config::default();
    }
    let contents = fs::read_to_string(&config_path).unwrap_or_default();
    let mut config = Config::default();
    for line in contents.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "remote_app" => config.remote_app = value.trim().to_string(),
                "quickemu_dir" => config.quickemu_dir = PathBuf::from(value.trim()),
                _ => {}
            }
        }
    }
    config
}

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

enum RemoteProtocol {
    Rdp(u16),
    Vnc(u16),
}

fn parse_vm_config(vm_conf: &Path) -> Option<RemoteProtocol> {
    if let Ok(contents) = fs::read_to_string(vm_conf) {
        for line in contents.lines() {
            if line.contains("port_forwards") {
                if let (Some(start), Some(end)) = (line.find('('), line.rfind(')')) {
                    let forwards_str = &line[start + 1..end];
                    let parts: Vec<&str> = forwards_str
                        .split('"')
                        .filter(|s| !s.trim().is_empty())
                        .collect();
                    for mapping in parts {
                        let split: Vec<&str> = mapping.split(':').collect();
                        if split.len() == 2 {
                            if let Ok(guest_port) = split[1].parse::<u16>() {
                                if guest_port == 3389 {
                                    if let Ok(host_port) = split[0].parse::<u16>() {
                                        return Some(RemoteProtocol::Rdp(host_port));
                                    }
                                } else if guest_port == 5900 {
                                    if let Ok(host_port) = split[0].parse::<u16>() {
                                        return Some(RemoteProtocol::Vnc(host_port));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn is_port_open(host: &str, port: u16, timeout: Duration) -> bool {
    let addr = format!("{}:{}", host, port);
    if let Ok(address) = addr.parse() {
        TcpStream::connect_timeout(&address, timeout).is_ok()
    } else {
        false
    }
}

fn remmina_profile_for_vm(vm_conf: &Path) -> Option<PathBuf> {
    let vm_name = vm_conf.file_stem()?.to_string_lossy().to_lowercase();
    let home = dirs::home_dir()?;
    let remmina_dir = home.join(".local/share/remmina");
    if let Ok(entries) = fs::read_dir(remmina_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "remmina" {
                        let profile_name = path.file_stem()?.to_string_lossy().to_lowercase();
                        if vm_name.contains("win") && profile_name.contains("win-11-vm") {
                            return Some(path);
                        } else if vm_name.contains("mac") && profile_name.contains("mac-os-vm") {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }
    None
}

fn is_vm_running(vm_conf: &Path, _config: &Config) -> bool {
    if let Some(protocol) = parse_vm_config(vm_conf) {
        let port = match protocol {
            RemoteProtocol::Rdp(p) | RemoteProtocol::Vnc(p) => p,
        };
        is_port_open("127.0.0.1", port, Duration::from_millis(200))
    } else {
        false
    }
}

fn start_vm(vm_conf: &Path, _config: &Config, logs: &Arc<Mutex<Vec<String>>>) {
    let vm_arg = vm_conf.as_os_str();
    let mut cmd = if parse_vm_config(vm_conf).is_some() {
        {
            let mut l = logs.lock().unwrap();
            l.push(format!("Launching VM {} headless...", vm_conf.display()));
        }
        let mut command = Command::new("quickemu");
        command.arg("--vm")
            .arg(vm_arg)
            .arg("--display")
            .arg("none");
        command
    } else {
        {
            let mut l = logs.lock().unwrap();
            l.push(format!("Launching VM {} normally...", vm_conf.display()));
        }
        let mut command = Command::new("quickemu");
        command.arg("--vm")
            .arg(vm_arg);
        command
    };
    let _ = cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| thread::sleep(Duration::from_secs(2)))
        .map_err(|e| {
            let mut l = logs.lock().unwrap();
            l.push(format!("Error launching VM {}: {}", vm_conf.display(), e));
        });
}

fn connect_vm(vm_conf: &Path, config: &Config, logs: &Arc<Mutex<Vec<String>>>) {
    let display_var = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    if let Some(protocol) = parse_vm_config(vm_conf) {
        let (host_port, proto_str) = match protocol {
            RemoteProtocol::Rdp(port) => (port, "rdp"),
            RemoteProtocol::Vnc(port) => (port, "vnc"),
        };
        if let Some(profile_path) = remmina_profile_for_vm(vm_conf) {
            {
                let mut l = logs.lock().unwrap();
                l.push(format!("Connecting using Remmina profile: {}", profile_path.display()));
            }
            // Launch remmina directly
            let _ = Command::new(&config.remote_app)
                .env("DISPLAY", &display_var)
                .env("G_MESSAGES_DEBUG", "none")
                .env("FREERDP_LOG_LEVEL", "OFF")
                .arg("-c")
                .arg(profile_path.to_str().unwrap())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| {
                    let mut l = logs.lock().unwrap();
                    l.push(format!("Error launching Remmina with profile: {}", e));
                });
        } else {
            let url = format!("{}://localhost:{}", proto_str, host_port);
            {
                let mut l = logs.lock().unwrap();
                l.push(format!("Connecting via URL: {}", url));
            }
            let _ = Command::new(&config.remote_app)
                .env("DISPLAY", &display_var)
                .env("G_MESSAGES_DEBUG", "none")
                .env("FREERDP_LOG_LEVEL", "OFF")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| {
                    let mut l = logs.lock().unwrap();
                    l.push(format!("Error launching Remmina: {}", e));
                });
        }
    } else {
        let vm_name = vm_conf.file_stem().unwrap().to_string_lossy();
        {
            let mut l = logs.lock().unwrap();
            l.push(format!("Connecting to SPICE VM {} using spicy...", vm_name));
        }
        let _ = Command::new("spicy")
            .env("DISPLAY", &display_var)
            .arg("--title")
            .arg(vm_name.as_ref())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                let mut l = logs.lock().unwrap();
                l.push(format!("Error launching spicy: {}", e));
            });
    }
}

fn stop_vm(vm_conf: &Path, _config: &Config, logs: &Arc<Mutex<Vec<String>>>) {
    {
        let mut l = logs.lock().unwrap();
        l.push(format!("Stopping VM {}...", vm_conf.display()));
    }
    let vm_arg = vm_conf.as_os_str();
    let result = Command::new("quickemu")
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

/// Main application state.
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
                Span::styled("[s] Stop", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("[j/k] Navigate", Style::default().fg(Color::Yellow)),
                Span::raw(" | "),
                Span::styled("[q] Quit", Style::default().fg(Color::Yellow)),
            ]);
            let footer_widget = Paragraph::new(footer_text)
                .block(Block::default().borders(Borders::ALL));
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
