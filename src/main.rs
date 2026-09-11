#![cfg_attr(windows, windows_subsystem = "windows")]

// The public executable deliberately does not link the backend library.
// Keep this module list restricted to installation, verification, UI and launch.
#[path = "game_manifest.rs"]
mod game_manifest;
#[path = "launcher_update.rs"]
mod launcher_update;
#[path = "p2p/mod.rs"]
mod p2p;
#[cfg(unix)]
#[path = "process.rs"]
mod process;
#[cfg(windows)]
#[path = "process_windows.rs"]
mod process;
#[path = "windows_setup.rs"]
mod windows_setup;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

const SPLASH_LOGO: &[u8] = include_bytes!("../assets/project-knockout-logo.png");
const SETUP_FONT: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");
const SPLASH_SHOW_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(windows)]
fn launcher_log_path() -> Result<PathBuf> {
    Ok(std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is unavailable")?
        .join(WINDOWS_INSTALL_DIRECTORY)
        .join("logs/launcher.log"))
}

#[cfg(target_os = "linux")]
fn launcher_log_path() -> Result<PathBuf> {
    Ok(linux_user_directory("XDG_STATE_HOME", ".local/state")?
        .join("ProjectKNOCKOUT/logs/launcher.log"))
}

fn redact_launcher_tokens(value: &str) -> String {
    let mut redacted = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(start) = remaining.find("dkl~") {
        redacted.push_str(&remaining[..start]);
        redacted.push_str("[redacted-launch-ticket]");
        let token = &remaining[start..];
        let length = token
            .char_indices()
            .take_while(|(_, value)| {
                value.is_ascii_alphanumeric() || matches!(value, '~' | '-' | '_')
            })
            .map(|(index, value)| index + value.len_utf8())
            .last()
            .unwrap_or(4);
        remaining = &token[length..];
    }
    redacted.push_str(remaining);
    redacted
}

fn append_launcher_log(message: impl AsRef<str>) {
    let Ok(path) = launcher_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "[{timestamp}] {}",
            redact_launcher_tokens(&message.as_ref().replace('\0', " "))
        );
    }
}

fn begin_launcher_log() {
    if let Ok(path) = launcher_log_path() {
        if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 4 * 1024 * 1024) {
            let previous = path.with_extension("previous.log");
            let _ = fs::remove_file(&previous);
            let _ = fs::rename(&path, previous);
        }
    }
    append_launcher_log(format!(
        "launcher start version={} platform={} verbose=true",
        env!("KNOCKOUT_LAUNCHER_VERSION"),
        if cfg!(windows) { "windows" } else { "linux" }
    ));
}

#[cfg(windows)]
fn apply_embedded_window_icon(window: &minifb::Window) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        LoadIconW, SendMessageW, ICON_BIG, ICON_SMALL, WM_SETICON,
    };

    unsafe {
        let icon = LoadIconW(GetModuleHandleW(std::ptr::null()), 1usize as *const u16);
        if !icon.is_null() {
            let handle = window.get_window_handle();
            SendMessageW(handle, WM_SETICON, ICON_SMALL as usize, icon as isize);
            SendMessageW(handle, WM_SETICON, ICON_BIG as usize, icon as isize);
        }
    }
}

#[cfg(not(windows))]
fn apply_embedded_window_icon(_window: &minifb::Window) {}

#[cfg(windows)]
fn remove_splash_window_chrome(window: &minifb::Window) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
        DWMWCP_DONOTROUND,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowLongPtrW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, WS_POPUP,
    };

    unsafe {
        let handle = window.get_window_handle();
        SetWindowLongPtrW(handle, GWL_STYLE, WS_POPUP as isize);
        SetWindowPos(
            handle,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        );

        // Windows 11 can draw a DWM outline and rounded corners even around a
        // popup. Older Windows versions safely ignore unsupported attributes.
        let border_color = 0xffff_fffeu32;
        let _ = DwmSetWindowAttribute(
            handle,
            DWMWA_BORDER_COLOR as u32,
            std::ptr::from_ref(&border_color).cast(),
            std::mem::size_of_val(&border_color) as u32,
        );
        let corner_preference = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            handle,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            std::ptr::from_ref(&corner_preference).cast(),
            std::mem::size_of_val(&corner_preference) as u32,
        );
    }
}

#[cfg(not(windows))]
fn remove_splash_window_chrome(_window: &minifb::Window) {}

#[cfg(windows)]
fn center_splash_window(window: &mut minifb::Window, width: usize, height: usize) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    unsafe {
        let mut cursor = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut cursor);
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return;
        }
        let work_width = info.rcWork.right - info.rcWork.left;
        let work_height = info.rcWork.bottom - info.rcWork.top;
        let x = info.rcWork.left + (work_width - width as i32).max(0) / 2;
        let y = info.rcWork.top + (work_height - height as i32).max(0) / 2;
        window.set_position(x as isize, y as isize);
    }
}

#[cfg(not(windows))]
fn center_splash_window(_window: &mut minifb::Window, _width: usize, _height: usize) {
    // The X11 backend centers new windows using the current screen dimensions.
}

#[cfg(windows)]
fn refresh_windows_shell_icons() {
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
}

enum SplashEvent {
    Progress(String, f32),
    #[cfg(windows)]
    ShowNow,
    Close,
}

struct Splash {
    sender: mpsc::Sender<SplashEvent>,
    #[cfg(windows)]
    opened: std::sync::Mutex<mpsc::Receiver<()>>,
}

impl Splash {
    fn start() -> Self {
        let (sender, receiver) = mpsc::channel();
        let (opened_sender, _opened) = mpsc::channel();
        std::thread::spawn(move || run_splash(receiver, opened_sender));
        Self {
            sender,
            #[cfg(windows)]
            opened: std::sync::Mutex::new(_opened),
        }
    }

    fn progress(&self, message: impl Into<String>, progress: f32) {
        let _ = self.sender.send(SplashEvent::Progress(
            message.into(),
            progress.clamp(0.0, 1.0),
        ));
    }

    fn close(&self) {
        let _ = self.sender.send(SplashEvent::Close);
    }

    #[cfg(windows)]
    fn show_now_and_wait(&self) {
        let _ = self.sender.send(SplashEvent::ShowNow);
        if let Ok(opened) = self.opened.lock() {
            let _ = opened.recv();
        }
    }
}

enum SetupEvent {
    #[cfg(windows)]
    PickGame(mpsc::Sender<Result<PathBuf>>),
    Progress(String),
    Success,
    Close,
}

enum SetupAction {
    Open,
    Done,
    Canceled,
}

struct SetupFlow {
    sender: Option<mpsc::Sender<SetupEvent>>,
    actions: Option<mpsc::Receiver<SetupAction>>,
}

impl SetupFlow {
    fn start() -> Self {
        #[cfg(target_os = "linux")]
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Self {
                sender: None,
                actions: None,
            };
        }

        let (sender, receiver) = mpsc::channel();
        let (action_sender, actions) = mpsc::channel();
        std::thread::spawn(move || run_setup_window(receiver, action_sender));
        Self {
            sender: Some(sender),
            actions: Some(actions),
        }
    }

    fn wait_for_open(&self) -> Result<()> {
        let Some(actions) = &self.actions else {
            return Ok(());
        };
        match actions.recv() {
            Ok(SetupAction::Open) | Err(_) => Ok(()),
            Ok(SetupAction::Done | SetupAction::Canceled) => {
                bail!("DivineKnockout.exe selection was canceled")
            }
        }
    }

    #[cfg(windows)]
    fn select_windows_game(&self) -> Result<PathBuf> {
        let Some(sender) = &self.sender else {
            return crate::process::select_game_executable_candidate();
        };
        let (reply, result) = mpsc::channel();
        sender
            .send(SetupEvent::PickGame(reply))
            .context("the setup window closed before opening the file picker")?;
        result
            .recv()
            .context("the setup window closed during game selection")?
    }

    fn progress(&self, message: impl Into<String>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(SetupEvent::Progress(message.into()));
        }
    }

    fn finish(&self) -> Result<()> {
        let (Some(sender), Some(actions)) = (&self.sender, &self.actions) else {
            return Ok(());
        };
        let _ = sender.send(SetupEvent::Success);
        match actions.recv() {
            Ok(SetupAction::Done) | Err(_) => Ok(()),
            Ok(SetupAction::Open) => Ok(()),
            Ok(SetupAction::Canceled) => Ok(()),
        }
    }
}

impl Drop for SetupFlow {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(SetupEvent::Close);
        }
    }
}

fn draw_setup_text(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    text: &str,
    scale: usize,
    color: u32,
) {
    static FONT: std::sync::OnceLock<fontdue::Font> = std::sync::OnceLock::new();
    let font = FONT.get_or_init(|| {
        fontdue::Font::from_bytes(SETUP_FONT, fontdue::FontSettings::default())
            .expect("embedded setup font is valid")
    });
    let size = match scale {
        0 | 1 => 13.0,
        2 => 20.0,
        _ => 28.0,
    };
    let baseline = y as isize + size as isize;
    let mut pen_x = x as f32;
    for character in text.chars() {
        let (metrics, bitmap) = font.rasterize(character, size);
        let glyph_x = pen_x as isize + metrics.xmin as isize;
        let glyph_y = baseline - metrics.height as isize - metrics.ymin as isize;
        for glyph_row in 0..metrics.height {
            for glyph_column in 0..metrics.width {
                let destination_x = glyph_x + glyph_column as isize;
                let destination_y = glyph_y + glyph_row as isize;
                if destination_x < 0
                    || destination_x >= width as isize
                    || destination_y < 0
                    || destination_y >= height as isize
                {
                    continue;
                }
                let alpha = bitmap[glyph_row * metrics.width + glyph_column] as u32;
                if alpha == 0 {
                    continue;
                }
                let destination =
                    &mut buffer[destination_y as usize * width + destination_x as usize];
                let background = *destination;
                let blend = |shift: u32| {
                    let foreground = (color >> shift) & 0xff;
                    let back = (background >> shift) & 0xff;
                    ((foreground * alpha + back * (255 - alpha)) / 255) << shift
                };
                *destination = blend(16) | blend(8) | blend(0);
            }
        }
        pen_x += metrics.advance_width;
    }
}

fn draw_centered_setup_text(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    y: usize,
    text: &str,
    scale: usize,
    color: u32,
) {
    static FONT: std::sync::OnceLock<fontdue::Font> = std::sync::OnceLock::new();
    let font = FONT.get_or_init(|| {
        fontdue::Font::from_bytes(SETUP_FONT, fontdue::FontSettings::default())
            .expect("embedded setup font is valid")
    });
    let size = match scale {
        0 | 1 => 13.0,
        2 => 20.0,
        _ => 28.0,
    };
    let text_width = text
        .chars()
        .map(|character| font.metrics(character, size).advance_width)
        .sum::<f32>() as usize;
    draw_setup_text(
        buffer,
        width,
        height,
        width.saturating_sub(text_width) / 2,
        y,
        text,
        scale,
        color,
    );
}

fn draw_setup_logo(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    logo: &image::DynamicImage,
    target_width: usize,
    start_y: usize,
) {
    use image::GenericImageView;

    let (source_width, source_height) = logo.dimensions();
    let target_height = target_width * source_height as usize / source_width as usize;
    let start_x = width.saturating_sub(target_width) / 2;
    for y in 0..target_height {
        if start_y + y >= height {
            break;
        }
        for x in 0..target_width {
            if start_x + x >= width {
                break;
            }
            let pixel = logo.get_pixel(
                (x * source_width as usize / target_width) as u32,
                (y * source_height as usize / target_height) as u32,
            );
            let alpha = pixel[3] as u32;
            if alpha == 0 {
                continue;
            }
            let destination = &mut buffer[(start_y + y) * width + start_x + x];
            let background = *destination;
            let blend = |foreground: u8, shift: u32| {
                let back = (background >> shift) & 0xff;
                ((foreground as u32 * alpha + back * (255 - alpha)) / 255) << shift
            };
            *destination = blend(pixel[0], 16) | blend(pixel[1], 8) | blend(pixel[2], 0);
        }
    }
}

fn draw_setup_welcome(buffer: &mut [u32], logo: Option<&image::DynamicImage>, hovering: bool) {
    const WIDTH: usize = 760;
    const HEIGHT: usize = 560;
    const BUTTON_X: usize = 135;
    const BUTTON_Y: usize = 406;
    const BUTTON_WIDTH: usize = 490;
    const BUTTON_HEIGHT: usize = 64;
    buffer.fill(0x00070b10);
    for y in 0..5 {
        for x in 0..WIDTH {
            buffer[y * WIDTH + x] = 0x00c4f66c;
        }
    }
    if let Some(logo) = logo {
        draw_setup_logo(buffer, WIDTH, HEIGHT, logo, 430, 48);
    } else {
        draw_centered_setup_text(buffer, WIDTH, HEIGHT, 78, "Project Knockout", 3, 0x00f2f5f7);
    }
    draw_centered_setup_text(buffer, WIDTH, HEIGHT, 198, "Launcher setup", 2, 0x00c4f66c);
    draw_centered_setup_text(
        buffer,
        WIDTH,
        HEIGHT,
        244,
        "Choose your DivineKnockout.exe",
        2,
        0x00dce2e7,
    );
    draw_centered_setup_text(
        buffer,
        WIDTH,
        HEIGHT,
        284,
        "Select it from your Divine Knockout game folder.",
        1,
        0x00c4f66c,
    );
    draw_centered_setup_text(
        buffer,
        WIDTH,
        HEIGHT,
        323,
        "We'll verify your game files and finish",
        1,
        0x008f9aa5,
    );
    draw_centered_setup_text(
        buffer,
        WIDTH,
        HEIGHT,
        342,
        "setting up the launcher for you.",
        1,
        0x008f9aa5,
    );
    let button_color = if hovering { 0x00d8ff91 } else { 0x00c4f66c };
    for y in BUTTON_Y..BUTTON_Y + BUTTON_HEIGHT {
        for x in BUTTON_X..BUTTON_X + BUTTON_WIDTH {
            buffer[y * WIDTH + x] = button_color;
        }
    }
    draw_centered_setup_text(buffer, WIDTH, HEIGHT, BUTTON_Y + 19, "Open", 2, 0x000b0e08);
    draw_centered_setup_text(
        buffer,
        WIDTH,
        HEIGHT,
        520,
        "Press Enter to continue",
        1,
        0x005f6974,
    );
}

fn draw_setup_line(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    from: (f32, f32),
    to: (f32, f32),
    progress: f32,
    thickness: usize,
    color: u32,
) {
    let progress = progress.clamp(0.0, 1.0);
    let end = (
        from.0 + (to.0 - from.0) * progress,
        from.1 + (to.1 - from.1) * progress,
    );
    let steps = ((end.0 - from.0).abs().max((end.1 - from.1).abs()) as usize).max(1);
    for step in 0..=steps {
        let amount = step as f32 / steps as f32;
        let center_x = (from.0 + (end.0 - from.0) * amount) as isize;
        let center_y = (from.1 + (end.1 - from.1) * amount) as isize;
        let radius = thickness as isize / 2;
        for offset_y in -radius..=radius {
            for offset_x in -radius..=radius {
                let x = center_x + offset_x;
                let y = center_y + offset_y;
                if x >= 0 && x < width as isize && y >= 0 && y < height as isize {
                    buffer[y as usize * width + x as usize] = color;
                }
            }
        }
    }
}

fn draw_setup_spinner(buffer: &mut [u32], width: usize, height: usize, elapsed: f32) {
    let active = (elapsed * 9.0) as usize % 12;
    for spoke in 0..12 {
        let age = (spoke + 12 - active) % 12;
        let intensity = 1.0 - age as f32 / 14.0;
        let angle = spoke as f32 / 12.0 * std::f32::consts::TAU;
        let color = |component: u32| (component as f32 * intensity) as u32;
        let color = (color(0xc4) << 16) | (color(0xf6) << 8) | color(0x6c);
        draw_setup_line(
            buffer,
            width,
            height,
            (380.0 + angle.cos() * 28.0, 244.0 + angle.sin() * 28.0),
            (380.0 + angle.cos() * 42.0, 244.0 + angle.sin() * 42.0),
            1.0,
            6,
            color,
        );
    }
}

fn draw_setup_loading(
    buffer: &mut [u32],
    logo: Option<&image::DynamicImage>,
    message: &str,
    elapsed: f32,
) {
    const WIDTH: usize = 760;
    const HEIGHT: usize = 560;
    buffer.fill(0x00070b10);
    for y in 0..5 {
        for x in 0..WIDTH {
            buffer[y * WIDTH + x] = 0x00c4f66c;
        }
    }
    if let Some(logo) = logo {
        draw_setup_logo(buffer, WIDTH, HEIGHT, logo, 430, 48);
    } else {
        draw_centered_setup_text(buffer, WIDTH, HEIGHT, 78, "Project Knockout", 3, 0x00f2f5f7);
    }
    draw_setup_spinner(buffer, WIDTH, HEIGHT, elapsed);
    draw_centered_setup_text(buffer, WIDTH, HEIGHT, 315, message, 2, 0x00dce2e7);
    draw_centered_setup_text(
        buffer,
        WIDTH,
        HEIGHT,
        363,
        "Please keep this window open.",
        1,
        0x008f9aa5,
    );
}

fn draw_setup_success(
    buffer: &mut [u32],
    logo: Option<&image::DynamicImage>,
    elapsed: f32,
    hovering: bool,
) {
    const WIDTH: usize = 760;
    const HEIGHT: usize = 560;
    const BUTTON_X: usize = 230;
    const BUTTON_Y: usize = 430;
    const BUTTON_WIDTH: usize = 300;
    const BUTTON_HEIGHT: usize = 58;
    buffer.fill(0x00070b10);
    for y in 0..5 {
        for x in 0..WIDTH {
            buffer[y * WIDTH + x] = 0x00c4f66c;
        }
    }
    if let Some(logo) = logo {
        draw_setup_logo(buffer, WIDTH, HEIGHT, logo, 360, 38);
    }
    let pulse = ((elapsed * 3.2).sin() * 0.5 + 0.5) * 0.35 + 0.65;
    let glow = (45.0 * pulse) as u32;
    let glow_color = (glow << 16) | ((glow * 5 / 4) << 8) | (glow / 2);
    for y in 172..292 {
        for x in 320..440 {
            let dx = x as f32 - 380.0;
            let dy = y as f32 - 232.0;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance < 58.0 && distance > 53.0 {
                buffer[y * WIDTH + x] = glow_color;
            }
        }
    }
    draw_setup_line(
        buffer,
        WIDTH,
        HEIGHT,
        (347.0, 232.0),
        (373.0, 257.0),
        (elapsed / 0.55).clamp(0.0, 1.0),
        8,
        0x00c4f66c,
    );
    draw_setup_line(
        buffer,
        WIDTH,
        HEIGHT,
        (373.0, 257.0),
        (417.0, 207.0),
        ((elapsed - 0.42) / 0.65).clamp(0.0, 1.0),
        8,
        0x00c4f66c,
    );
    if elapsed >= 0.35 {
        draw_centered_setup_text(buffer, WIDTH, HEIGHT, 306, "Setup complete", 3, 0x00f2f5f7);
    }
    if elapsed >= 0.55 {
        draw_centered_setup_text(
            buffer,
            WIDTH,
            HEIGHT,
            354,
            "Project Knockout is ready to launch.",
            2,
            0x00c4f66c,
        );
        draw_centered_setup_text(
            buffer,
            WIDTH,
            HEIGHT,
            394,
            "Return to the website and choose Launch game.",
            1,
            0x008f9aa5,
        );
    }
    if elapsed >= 0.75 {
        let button_color = if hovering { 0x00d8ff91 } else { 0x00c4f66c };
        for y in BUTTON_Y..BUTTON_Y + BUTTON_HEIGHT {
            for x in BUTTON_X..BUTTON_X + BUTTON_WIDTH {
                buffer[y * WIDTH + x] = button_color;
            }
        }
        draw_centered_setup_text(buffer, WIDTH, HEIGHT, BUTTON_Y + 17, "Done", 2, 0x000b0e08);
    }
}

enum SetupStage {
    Welcome,
    Loading {
        message: String,
        started: std::time::Instant,
    },
    Success {
        started: std::time::Instant,
    },
}

fn run_setup_window(receiver: mpsc::Receiver<SetupEvent>, actions: mpsc::Sender<SetupAction>) {
    use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};

    const WIDTH: usize = 760;
    const HEIGHT: usize = 560;
    let Ok(mut window) = Window::new(
        "Project Knockout — Launcher setup",
        WIDTH,
        HEIGHT,
        WindowOptions {
            resize: false,
            ..WindowOptions::default()
        },
    ) else {
        let _ = actions.send(SetupAction::Open);
        return;
    };
    apply_embedded_window_icon(&window);
    window.set_target_fps(60);
    let logo = image::load_from_memory(SPLASH_LOGO).ok();
    let mut stage = SetupStage::Welcome;
    let mut buffer = vec![0x00070b10; WIDTH * HEIGHT];
    let mut mouse_was_down = false;
    while window.is_open() {
        while let Ok(event) = receiver.try_recv() {
            match event {
                #[cfg(windows)]
                SetupEvent::PickGame(reply) => {
                    let selected = crate::process::select_game_executable_candidate_with_owner(
                        window.get_window_handle(),
                    );
                    let _ = reply.send(selected);
                }
                SetupEvent::Progress(message) => {
                    stage = SetupStage::Loading {
                        message,
                        started: std::time::Instant::now(),
                    };
                }
                SetupEvent::Success => {
                    stage = SetupStage::Success {
                        started: std::time::Instant::now(),
                    };
                    window.set_title("Project Knockout — Setup complete");
                }
                SetupEvent::Close => return,
            }
        }

        let mouse = window.get_mouse_pos(MouseMode::Discard);
        let hovering = match stage {
            SetupStage::Welcome => mouse
                .is_some_and(|(x, y)| (135.0..625.0).contains(&x) && (406.0..470.0).contains(&y)),
            SetupStage::Success { .. } => mouse
                .is_some_and(|(x, y)| (230.0..530.0).contains(&x) && (430.0..488.0).contains(&y)),
            SetupStage::Loading { .. } => false,
        };
        let mouse_down = window.get_mouse_down(MouseButton::Left);
        let activated = (hovering && mouse_down && !mouse_was_down)
            || (matches!(stage, SetupStage::Welcome | SetupStage::Success { .. })
                && window.is_key_pressed(Key::Enter, KeyRepeat::No));
        mouse_was_down = mouse_down;
        if activated {
            match stage {
                SetupStage::Welcome => {
                    stage = SetupStage::Loading {
                        message: "Opening your game folder".to_owned(),
                        started: std::time::Instant::now(),
                    };
                    window.set_title("Project Knockout — Opening game file");
                    let _ = actions.send(SetupAction::Open);
                }
                SetupStage::Success { .. } => {
                    let _ = actions.send(SetupAction::Done);
                    return;
                }
                SetupStage::Loading { .. } => {}
            }
        }

        match &stage {
            SetupStage::Welcome => draw_setup_welcome(&mut buffer, logo.as_ref(), hovering),
            SetupStage::Loading { message, started } => {
                draw_setup_loading(
                    &mut buffer,
                    logo.as_ref(),
                    message,
                    started.elapsed().as_secs_f32(),
                );
            }
            SetupStage::Success { started } => draw_setup_success(
                &mut buffer,
                logo.as_ref(),
                started.elapsed().as_secs_f32(),
                hovering,
            ),
        }
        if window.update_with_buffer(&buffer, WIDTH, HEIGHT).is_err() {
            let _ = actions.send(SetupAction::Canceled);
            return;
        }
    }
    let _ = actions.send(SetupAction::Canceled);
}

fn run_splash(receiver: mpsc::Receiver<SplashEvent>, opened: mpsc::Sender<()>) {
    use image::GenericImageView;
    use minifb::{Window, WindowOptions};

    const WIDTH: usize = 720;
    const HEIGHT: usize = 360;
    const CONTENT_OFFSET_Y: usize = 28;
    let mut message = "Starting launcher".to_owned();
    let mut progress = 0.03f32;
    let show_at = std::time::Instant::now() + SPLASH_SHOW_DELAY;
    loop {
        let now = std::time::Instant::now();
        if now >= show_at {
            break;
        }
        match receiver.recv_timeout(show_at.saturating_duration_since(now)) {
            Ok(SplashEvent::Progress(next_message, value)) => {
                message = next_message;
                progress = value;
            }
            #[cfg(windows)]
            Ok(SplashEvent::ShowNow) => break,
            Ok(SplashEvent::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }
    let Ok(mut window) = Window::new(
        "Project KNOCKOUT — Preparing game",
        WIDTH,
        HEIGHT,
        WindowOptions {
            borderless: cfg!(windows),
            title: !cfg!(windows),
            none: cfg!(windows),
            resize: false,
            ..WindowOptions::default()
        },
    ) else {
        let _ = opened.send(());
        return;
    };
    apply_embedded_window_icon(&window);
    remove_splash_window_chrome(&window);
    center_splash_window(&mut window, WIDTH, HEIGHT);
    window.set_target_fps(60);
    let logo = image::load_from_memory(SPLASH_LOGO).ok();
    let mut buffer = vec![0x00070b10; WIDTH * HEIGHT];
    let mut opened = Some(opened);
    while window.is_open() {
        match receiver.recv_timeout(std::time::Duration::from_millis(16)) {
            Ok(SplashEvent::Progress(next_message, value)) => {
                message = next_message;
                progress = value;
            }
            #[cfg(windows)]
            Ok(SplashEvent::ShowNow) => {}
            Ok(SplashEvent::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        loop {
            match receiver.try_recv() {
                Ok(SplashEvent::Progress(next_message, value)) => {
                    message = next_message;
                    progress = value;
                }
                #[cfg(windows)]
                Ok(SplashEvent::ShowNow) => {}
                Ok(SplashEvent::Close) | Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        window.set_title(&format!("Project KNOCKOUT — {message}"));
        buffer.fill(0x00070b10);
        for y in 0..4 {
            for x in 0..WIDTH {
                buffer[y * WIDTH + x] = 0x00c4f66c;
            }
        }
        if let Some(logo) = &logo {
            let (source_width, source_height) = logo.dimensions();
            let target_width = 450usize;
            let target_height = target_width * source_height as usize / source_width as usize;
            let start_x = (WIDTH - target_width) / 2;
            let start_y = 82usize - CONTENT_OFFSET_Y;
            for y in 0..target_height {
                for x in 0..target_width {
                    let pixel = logo.get_pixel(
                        (x * source_width as usize / target_width) as u32,
                        (y * source_height as usize / target_height) as u32,
                    );
                    let alpha = pixel[3] as u32;
                    if alpha == 0 {
                        continue;
                    }
                    let destination = &mut buffer[(start_y + y) * WIDTH + start_x + x];
                    let background = *destination;
                    let blend = |foreground: u8, shift: u32| {
                        let back = (background >> shift) & 0xff;
                        ((foreground as u32 * alpha + back * (255 - alpha)) / 255) << shift
                    };
                    *destination = blend(pixel[0], 16) | blend(pixel[1], 8) | blend(pixel[2], 0);
                }
            }
        }
        draw_centered_setup_text(
            &mut buffer,
            WIDTH,
            HEIGHT,
            265 - CONTENT_OFFSET_Y,
            &message,
            1,
            0x00dce2e7,
        );
        draw_centered_setup_text(
            &mut buffer,
            WIDTH,
            HEIGHT,
            289 - CONTENT_OFFSET_Y,
            &format!("{}%", (progress * 100.0).round() as u32),
            1,
            0x008f9aa5,
        );
        let bar_x = 110usize;
        let bar_y = 316usize - CONTENT_OFFSET_Y;
        let bar_width = WIDTH - bar_x * 2;
        for y in bar_y..bar_y + 8 {
            for x in bar_x..bar_x + bar_width {
                buffer[y * WIDTH + x] = 0x00242c35;
            }
            for x in bar_x..bar_x + (bar_width as f32 * progress) as usize {
                buffer[y * WIDTH + x] = 0x00c4f66c;
            }
        }
        if window.update_with_buffer(&buffer, WIDTH, HEIGHT).is_err() {
            if let Some(opened) = opened.take() {
                let _ = opened.send(());
            }
            break;
        }
        if let Some(opened) = opened.take() {
            let _ = opened.send(());
        }
    }
    if let Some(opened) = opened.take() {
        let _ = opened.send(());
    }
}

#[derive(Parser)]
#[command(
    name = "dko-launcher",
    version = env!("KNOCKOUT_LAUNCHER_VERSION"),
    about = "Launch Divine Knockout from the Project KNOCKOUT portal"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install this executable as the per-user browser protocol handler.
    InstallProtocol {
        /// HTTPS origin of the Project KNOCKOUT server this launcher may trust.
        #[arg(long)]
        server_url: String,
        /// Steam installation root. The platform's normal user location is used by default.
        #[arg(long, default_value_os_t = default_steam_root())]
        steam_root: PathBuf,
    },
    #[command(name = "__protocol-launch", hide = true)]
    InternalProtocolLaunch { uri: String },
    /// Choose and validate a different DivineKnockout.exe without changing the portal URL.
    #[cfg(windows)]
    SelectGame,
    /// Verify the installed browser handler, game, and configured server.
    Doctor,
    /// Verify and download the server manifest into the managed game runtime.
    Update,
}

#[cfg(target_os = "linux")]
#[derive(Serialize, Deserialize)]
struct LinuxLauncherSettings {
    server_url: String,
    steam_root: PathBuf,
    game_executable: PathBuf,
}

const PROTOCOL_SCHEME: &str = "project-knockout";
const LAUNCH_HOMEDIR: &str = env!("KNOCKOUT_GAME_HOMEDIR");
#[cfg(windows)]
const WINDOWS_INSTALL_DIRECTORY: &str = "ProjectKNOCKOUT";
#[cfg(windows)]
const WINDOWS_EXECUTABLE_NAME: &str = "Project-KNOCKOUT.exe";

fn default_steam_root() -> PathBuf {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        use winreg::RegKey;

        let current_user = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(steam) = current_user.open_subkey(r"Software\Valve\Steam") {
            if let Ok(path) = steam.get_value::<String, _>("SteamPath") {
                return PathBuf::from(path);
            }
        }
        let local_machine = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Ok(steam) = local_machine.open_subkey(r"Software\WOW6432Node\Valve\Steam") {
            if let Ok(path) = steam.get_value::<String, _>("InstallPath") {
                return PathBuf::from(path);
            }
        }
        std::env::var_os("PROGRAMFILES(X86)")
            .or_else(|| std::env::var_os("PROGRAMFILES"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"))
            .join("Steam")
    }
    #[cfg(target_os = "linux")]
    {
        let mut candidates = Vec::new();
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            candidates.push(PathBuf::from(data_home).join("Steam"));
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            candidates.push(home.join(".local/share/Steam"));
            candidates.push(home.join(".steam/steam"));
            candidates.push(home.join(".steam/root"));
            candidates.push(home.join(".steam/debian-install"));
            candidates.push(home.join(".var/app/com.valvesoftware.Steam/data/Steam"));
            candidates.push(home.join("snap/steam/common/.local/share/Steam"));
        }
        candidates
            .iter()
            .find(|candidate| candidate.join("steamapps").is_dir())
            .cloned()
            .or_else(|| candidates.into_iter().next())
            .unwrap_or_else(|| PathBuf::from(".local/share/Steam"))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    PathBuf::from(".local/share/Steam")
}

struct ProtocolLaunch {
    ticket: String,
    username: String,
    command_args: Vec<String>,
}

fn without_auth_password(arguments: &[String]) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut arguments = arguments.iter().peekable();
    while let Some(argument) = arguments.next() {
        let normalized = argument.trim_start_matches('-').to_ascii_lowercase();
        if normalized == "auth_password" {
            if arguments
                .peek()
                .is_some_and(|value| !value.starts_with('-'))
            {
                arguments.next();
            }
        } else if !normalized.starts_with("auth_password=")
            && !normalized.starts_with("auth_password ")
        {
            filtered.push(argument.clone());
        }
    }
    filtered
}

fn validate_player_command_arguments(arguments: &[String]) -> Result<()> {
    const BLOCKED: [&str; 11] = [
        "-auth_",
        "-rallyhereurl",
        "-homedir",
        "-abslog",
        "-logcmds",
        "-oss",
        "-nosteam",
        "-noeac",
        "-hirezenv",
        "-ini:",
        "-ini=",
    ];
    if arguments.len() > 16 || arguments.iter().map(String::len).sum::<usize>() > 1024 {
        bail!("too many custom launch arguments");
    }
    for argument in arguments {
        let normalized = argument.to_ascii_lowercase();
        if argument.is_empty()
            || argument.len() > 256
            || !argument.starts_with('-')
            || argument.chars().any(char::is_control)
            || BLOCKED.iter().any(|prefix| normalized.starts_with(prefix))
        {
            bail!("custom launch argument is not allowed");
        }
    }
    Ok(())
}

fn parse_protocol_uri(uri: &str) -> Result<ProtocolLaunch> {
    let parsed = url::Url::parse(uri).context("parse DKO launcher URI")?;
    if parsed.scheme() != PROTOCOL_SCHEME
        || parsed.host_str() != Some("launch")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.fragment().is_some()
    {
        bail!("invalid DKO launcher URI");
    }
    let mut ticket = None;
    let mut username = None;
    let mut mode_seen = false;
    let mut command_args = Vec::new();
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "ticket" if ticket.is_none() => ticket = Some(value.into_owned()),
            "username" if username.is_none() => username = Some(value.into_owned()),
            // Existing portal links carry this field. It cannot bypass updates.
            "mode" if !mode_seen && value == "patched" => mode_seen = true,
            "arg" => command_args.push(value.into_owned()),
            _ => bail!("invalid or repeated DKO launcher URI field"),
        }
    }
    let ticket = ticket.context("DKO launcher URI omitted its ticket")?;
    if !ticket.starts_with("dkl~")
        || ticket.len() > 128
        || !ticket
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '~' | '-' | '_'))
    {
        bail!("DKO launcher ticket has an invalid format");
    }
    let username = username.context("DKO launcher URI omitted its username")?;
    if username.is_empty() || username.len() > 64 || username.chars().any(char::is_control) {
        bail!("DKO launcher username has an invalid format");
    }
    let command_args = without_auth_password(&command_args);
    validate_player_command_arguments(&command_args)?;
    Ok(ProtocolLaunch {
        ticket,
        username,
        command_args,
    })
}

fn is_repair_protocol_uri(uri: &str) -> Result<bool> {
    let parsed = url::Url::parse(uri).context("parse DKO launcher URI")?;
    if parsed.host_str() != Some("repair") {
        return Ok(false);
    }
    // Repair only opens local configuration. Never accept a server, executable,
    // credentials, or launch arguments supplied by a website.
    if !matches!(
        uri,
        "project-knockout://repair" | "project-knockout://repair/"
    ) {
        bail!("invalid DKO repair URI");
    }
    Ok(true)
}

fn validate_pinned_server_url(value: &str) -> Result<url::Url> {
    let mut parsed = url::Url::parse(value).context("parse trusted DKO server URL")?;
    let loopback_http = parsed.scheme() == "http"
        && parsed
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|host| host.is_loopback());
    if parsed.scheme() != "https" && !loopback_http {
        bail!("the external launcher requires HTTPS (HTTP is allowed only for loopback testing)");
    }
    if parsed.host_str().is_none() || parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("trusted DKO server URL must be an absolute origin without query or fragment");
    }
    let path = parsed.path().trim_end_matches('/').to_owned();
    parsed.set_path(&path);
    Ok(parsed)
}

fn remove_upgrade_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove stale launcher {}", path.display()))
        }
    }
}

fn replace_installed_launcher(current: &Path, installed: &Path) -> Result<()> {
    let staged = installed.with_extension("new");
    let previous = installed.with_extension("old");
    remove_upgrade_file(&staged)?;
    remove_upgrade_file(&previous)?;
    if current == installed {
        return Ok(());
    }
    std::fs::copy(current, &staged).with_context(|| {
        format!(
            "stage launcher upgrade from {} to {}",
            current.display(),
            staged.display()
        )
    })?;
    if installed.is_file() {
        std::fs::rename(installed, &previous).with_context(|| {
            format!(
                "move installed launcher {} to {}",
                installed.display(),
                previous.display()
            )
        })?;
    }
    if let Err(error) = std::fs::rename(&staged, installed) {
        if previous.is_file() && !installed.exists() {
            let _ = std::fs::rename(&previous, installed);
        }
        return Err(error).with_context(|| {
            format!(
                "activate launcher upgrade from {} to {}",
                staged.display(),
                installed.display()
            )
        });
    }
    if let Err(error) = remove_upgrade_file(&previous) {
        #[cfg(windows)]
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(windows)]
fn installed_launcher_executable() -> Result<PathBuf> {
    Ok(std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is unavailable")?
        .join(WINDOWS_INSTALL_DIRECTORY)
        .join(WINDOWS_EXECUTABLE_NAME))
}

#[cfg(target_os = "linux")]
fn installed_launcher_executable() -> Result<PathBuf> {
    linux_installed_executable()
}

fn download_verified_with_progress(
    server: &url::Url,
    path: &str,
    expected_size: u64,
    expected_sha256: &str,
    mut progress: impl FnMut(u64, u64),
) -> Result<Vec<u8>> {
    let mut url = server.clone();
    url.set_path(path);
    url.set_query(None);
    let mut response = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(300))
        .build()?
        .get(url.clone())
        .send()
        .map_err(|error| error.without_url())
        .context("download managed game file")?;
    if !response.status().is_success() {
        bail!("managed file server returned HTTP {}", response.status());
    }
    let mut body = Vec::new();
    let mut digest = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut downloaded = 0u64;
    progress(0, expected_size);
    loop {
        let count = response.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        downloaded = downloaded.saturating_add(count as u64);
        if downloaded > expected_size {
            bail!("downloaded managed file exceeds its declared size");
        }
        digest.update(&chunk[..count]);
        body.extend_from_slice(&chunk[..count]);
        progress(downloaded, expected_size);
    }
    let actual_sha256 = hex::encode(digest.finalize());
    if body.len() as u64 != expected_size || actual_sha256 != expected_sha256.to_ascii_lowercase() {
        bail!(
            "downloaded managed file failed integrity verification: expected size {expected_size} and SHA-256 {expected_sha256}, received size {} and SHA-256 {actual_sha256}",
            body.len()
        );
    }
    Ok(body)
}

fn format_download_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn fetch_launcher_manifest(server: &url::Url) -> Result<crate::launcher_update::LauncherManifest> {
    let mut url = server.clone();
    url.set_path(crate::launcher_update::MANIFEST_PATH);
    url.set_query(None);
    let response = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .get(url.clone())
        .send()
        .with_context(|| format!("download launcher manifest from {url}"))?;
    if !response.status().is_success() {
        bail!("launcher update server returned HTTP {}", response.status());
    }
    let manifest = response.json::<crate::launcher_update::LauncherManifest>()?;
    crate::launcher_update::validate_manifest(&manifest)?;
    Ok(manifest)
}

fn acknowledge_protocol_launch(server: &url::Url, ticket: &str) -> Result<()> {
    let mut url = server.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.set_path(&ticket_routed_path(
        ticket,
        env!("KNOCKOUT_LAUNCH_ACK_PATH"),
    ));
    let response = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .post(url.clone())
        .send()
        .with_context(|| format!("acknowledge browser launch with {url}"))?;
    if !response.status().is_success() {
        bail!("launch acknowledgement returned HTTP {}", response.status());
    }
    Ok(())
}

fn report_protocol_launch_status(
    server: &url::Url,
    ticket: &str,
    stage: &str,
    failure_code: Option<&str>,
) -> Result<()> {
    let mut url = server.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.set_path(&ticket_routed_path(
        ticket,
        env!("KNOCKOUT_LAUNCH_STATUS_PATH"),
    ));
    let response = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(5))
        .build()?
        .post(url)
        .json(&serde_json::json!({ "stage": stage, "failure_code": failure_code }))
        .send()?;
    // Updated launchers can reach an older server during a rolling release.
    // Preserve failure reporting until that server accepts the Proton code.
    if response.status() == reqwest::StatusCode::BAD_REQUEST && failure_code == Some("proton") {
        return report_protocol_launch_status(server, ticket, stage, Some("preflight"));
    }
    if !response.status().is_success() {
        bail!("launcher status server returned HTTP {}", response.status());
    }
    Ok(())
}

fn launcher_failure_code(error: &anyhow::Error) -> &'static str {
    let details = format!("{error:#}").to_ascii_lowercase();
    if details.contains("proton") || details.contains("compatibility tool") {
        "proton"
    } else if details.contains("executable missing")
        || details.contains("divine knockout was not found")
    {
        "game_missing"
    } else if details.contains("supported stock") || details.contains("supported dko build") {
        "game_version"
    } else if details.contains("ticket") || details.contains("acknowledgement") {
        "ticket"
    } else if details.contains("manifest") || details.contains("integrity verification") {
        "game_update"
    } else if details.contains("start divine knockout") || details.contains("spawn") {
        "process_start"
    } else if details.contains("http")
        || details.contains("connect")
        || details.contains("download")
    {
        "network_or_update"
    } else {
        "preflight"
    }
}

fn ensure_launcher_current(server: &url::Url, uri: &str, splash: &Splash) -> Result<bool> {
    splash.progress("Checking for a launcher update", 0.07);
    let manifest = fetch_launcher_manifest(server)?;
    #[cfg(windows)]
    let platform = "windows-x86_64";
    #[cfg(target_os = "linux")]
    let platform = "linux-x86_64";
    let artifact = manifest
        .launchers
        .iter()
        .find(|artifact| artifact.platform == platform)
        .with_context(|| format!("server did not publish a launcher for {platform}"))?;
    let installed = installed_launcher_executable()?;
    fs::create_dir_all(
        installed
            .parent()
            .context("installed launcher path has no parent")?,
    )
    .with_context(|| format!("create launcher directory for {}", installed.display()))?;
    let _ = remove_upgrade_file(&installed.with_extension("old"));
    splash.progress("Verifying the installed launcher", 0.1);
    if crate::game_manifest::sha256_file(&installed).is_ok_and(|hash| hash == artifact.sha256) {
        let current = std::env::current_exe().context("resolve running launcher executable")?;
        if crate::game_manifest::sha256_file(&current).is_ok_and(|hash| hash == artifact.sha256) {
            splash.progress("Launcher is up to date", 0.14);
            return Ok(false);
        }

        // If a launch was invoked through a stale non-canonical executable,
        // hand the same ticket to the verified canonical launcher before any
        // game-file or managed-runtime preflight runs.
        splash.progress("Switching to the current launcher", 0.22);
        std::process::Command::new(&installed)
            .arg("__protocol-launch")
            .arg(uri)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("restart current launcher {}", installed.display()))?;
        return Ok(true);
    }
    let body = download_verified_with_progress(
        server,
        &format!(
            "{}{}",
            crate::launcher_update::FILE_PATH_PREFIX,
            artifact.sha256
        ),
        artifact.size,
        &artifact.sha256,
        |downloaded, total| {
            let fraction = if total == 0 {
                0.0
            } else {
                downloaded as f32 / total as f32
            };
            splash.progress(
                format!(
                    "Downloading launcher update — {} / {}",
                    format_download_size(downloaded),
                    format_download_size(total)
                ),
                0.14 + 0.06 * fraction,
            );
        },
    )?;
    splash.progress("Verifying and installing launcher update", 0.205);
    let downloaded = installed.with_extension("download");
    fs::write(&downloaded, body)
        .with_context(|| format!("stage launcher update {}", downloaded.display()))?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&downloaded, fs::Permissions::from_mode(0o700))?;
    }
    replace_installed_launcher(&downloaded, &installed)?;
    remove_upgrade_file(&downloaded)?;
    splash.progress("Launcher updated — restarting", 0.22);
    std::process::Command::new(&installed)
        .arg("__protocol-launch")
        .arg(uri)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("restart updated launcher {}", installed.display()))?;
    Ok(true)
}

#[cfg(windows)]
fn windows_message_box(title: &str, message: &str, error: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND,
    };

    let title = title
        .replace('\0', " ")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let message = message
        .replace('\0', " ")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let icon = if error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_SETFOREGROUND | icon,
        );
    }
}

#[cfg(windows)]
fn external_launcher_settings() -> Result<(url::Url, PathBuf)> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let settings = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\DKOPreservation")
        .context("DKO launcher is not configured; run setup again")?;
    let server_url: String = settings.get_value("ServerUrl")?;
    let steam_root: String = settings.get_value("SteamRoot")?;
    Ok((
        validate_pinned_server_url(&server_url)?,
        PathBuf::from(steam_root),
    ))
}

#[cfg(windows)]
fn install_protocol(server_url: &str, steam_root: &Path) -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let server_url = validate_pinned_server_url(server_url)?;
    let setup = SetupFlow::start();
    setup.wait_for_open()?;
    setup.progress("Choose DivineKnockout.exe in the file window");
    let selected = setup.select_windows_game()?;
    setup.progress("Verifying original game files");
    let (_, pak) = retail_paths(&selected)?;
    verify_retail_build(&selected, &pak)?;
    setup.progress("Installing Project Knockout launcher");
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is unavailable")?;
    let install_dir = local_app_data.join(WINDOWS_INSTALL_DIRECTORY);
    std::fs::create_dir_all(&install_dir)
        .with_context(|| format!("create launcher directory {}", install_dir.display()))?;
    let installed_executable = install_dir.join(WINDOWS_EXECUTABLE_NAME);
    let current = std::env::current_exe().context("resolve downloaded launcher executable")?;
    replace_installed_launcher(&current, &installed_executable)?;
    for legacy_name in ["Project_KNOCKOUT_Launcher.exe", "dko-launcher.exe"] {
        let legacy = install_dir.join(legacy_name);
        if legacy != installed_executable {
            remove_upgrade_file(&legacy)?;
        }
    }

    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let (settings, _) = current_user.create_subkey(r"Software\DKOPreservation")?;
    crate::process::remember_game_executable(&selected)?;
    settings.set_value("ServerUrl", &server_url.as_str())?;
    settings.set_value("SteamRoot", &steam_root.to_string_lossy().as_ref())?;
    match current_user.delete_subkey_all(r"Software\Classes\project-knockout") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context("remove competing project-knockout protocol handler")
        }
    }
    let (protocol, _) = current_user.create_subkey(r"Software\Classes\project-knockout")?;
    protocol.set_value("", &"URL:Project KNOCKOUT Launcher")?;
    protocol.set_value("URL Protocol", &"")?;
    let (icon, _) = protocol.create_subkey("DefaultIcon")?;
    icon.set_value("", &format!("\"{}\",0", installed_executable.display()))?;
    let (command, _) = protocol.create_subkey(r"shell\open\command")?;
    command.set_value(
        "",
        &format!(
            "\"{}\" __protocol-launch \"%1\"",
            installed_executable.display()
        ),
    )?;
    match current_user.delete_subkey_all(r"Software\Classes\dko-preservation") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove legacy dko-preservation protocol handler"),
    }
    refresh_windows_shell_icons();
    setup.finish()
}

#[cfg(windows)]
fn selected_game_executable() -> Result<PathBuf> {
    crate::process::selected_game_executable()
}

#[cfg(target_os = "linux")]
fn selected_game_executable() -> Result<PathBuf> {
    Ok(linux_launcher_settings()?.game_executable)
}

#[cfg(windows)]
fn game_data_directory() -> Result<PathBuf> {
    Ok(std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is unavailable")?
        .join(WINDOWS_INSTALL_DIRECTORY)
        .join(env!("KNOCKOUT_RUNTIME_DIRECTORY")))
}

#[cfg(target_os = "linux")]
fn game_data_directory() -> Result<PathBuf> {
    Ok(linux_user_directory("XDG_DATA_HOME", ".local/share")?
        .join("ProjectKNOCKOUT")
        .join(env!("KNOCKOUT_RUNTIME_DIRECTORY")))
}

fn adjacent_runtime_directory() -> Result<PathBuf> {
    let executable = selected_game_executable()?;
    let (retail_root, _) = retail_paths(&executable)?;
    Ok(retail_root
        .parent()
        .context("selected DKO installation has no parent")?
        .join(".project-knockout-dko-runtime"))
}

#[cfg(windows)]
fn windows_volume_prefix(path: &Path) -> Option<OsString> {
    use std::path::Component;

    match path.components().next()? {
        Component::Prefix(prefix) => Some(prefix.as_os_str().to_owned()),
        _ => None,
    }
}

fn runtime_directory() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let data = game_data_directory()?;
        let executable = selected_game_executable()?;
        let (retail_root, _) = retail_paths(&executable)?;
        let data_volume = windows_volume_prefix(&data);
        let retail_volume = windows_volume_prefix(&retail_root);
        if data_volume.as_ref().is_some_and(|data_volume| {
            retail_volume.as_ref().is_some_and(|retail_volume| {
                data_volume
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&retail_volume.to_string_lossy())
            })
        }) {
            // Default Steam installs live under Program Files. Keep launcher-owned
            // files in LocalAppData when it is on the same volume, so creating and
            // replacing runtime generations never depends on Steam directory ACLs
            // while the retail payload can still be hard-linked efficiently.
            return Ok(data.join("runtime"));
        }
    }

    // A Steam library on another volume needs the adjacent layout so large retail
    // files can be hard-linked instead of duplicated across volumes.
    adjacent_runtime_directory()
}

fn runtime_generation_directory(runtime: &Path, generation: &str) -> PathBuf {
    let name = runtime
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("runtime");
    runtime.with_file_name(format!("{name}.{generation}"))
}

fn game_compat_directory() -> Result<PathBuf> {
    Ok(game_data_directory()?.join("compatdata"))
}

fn remove_inactive_generations(data: &Path) -> Result<()> {
    for name in ["runtime.staging", "runtime.previous"] {
        let directory = data.join(name);
        if directory.exists() {
            fs::remove_dir_all(&directory).with_context(|| {
                format!(
                    "remove inactive game update generation {}",
                    directory.display()
                )
            })?;
        }
    }
    Ok(())
}

fn retail_paths(executable: &Path) -> Result<(PathBuf, PathBuf)> {
    let divine_knockout = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context(
            "selected executable is outside the expected DivineKnockout/Binaries/Win64 path",
        )?;
    if divine_knockout.file_name().and_then(|name| name.to_str()) != Some("DivineKnockout") {
        bail!("selected executable is outside a Divine Knockout installation");
    }
    let root = divine_knockout
        .parent()
        .context("selected DKO installation has no root")?
        .to_owned();
    let pak = divine_knockout.join("Content/Paks/pakchunk0-WindowsNoEditor.pak");
    Ok((root, pak))
}

fn game_manifest_url(mut server: url::Url) -> Result<url::Url> {
    server.set_path(crate::game_manifest::MANIFEST_PATH);
    server.set_query(None);
    Ok(server)
}

fn fetch_manifest(
    server: &url::Url,
    ticket: Option<&str>,
) -> Result<crate::game_manifest::GameManifest> {
    let mut url = game_manifest_url(server.clone())?;
    if let Some(ticket) = ticket {
        url.set_path(&ticket_routed_path(
            ticket,
            crate::game_manifest::MANIFEST_PATH,
        ));
    }
    let response = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .get(url.clone())
        .send()
        .with_context(|| format!("download game update manifest from {url}"))?;
    if !response.status().is_success() {
        bail!(
            "game update server returned HTTP {} for its manifest",
            response.status()
        );
    }
    let manifest = response.json::<crate::game_manifest::GameManifest>()?;
    crate::game_manifest::validate_manifest(&manifest)?;
    Ok(manifest)
}

fn ticket_routed_path(ticket: &str, path: &str) -> String {
    format!("{}{ticket}{path}", env!("KNOCKOUT_LAUNCH_PREFIX"))
}

fn verify_retail_build(executable: &Path, pak: &Path) -> Result<()> {
    let (root, _) = retail_paths(executable)?;
    let version_path = root.join("DivineKnockout/Assembly/GameVersion.txt");
    let version = fs::read_to_string(&version_path)
        .with_context(|| format!("read {}", version_path.display()))?;
    if version.trim() != crate::game_manifest::TARGET_BUILD {
        bail!(
            "game launch requires DKO {}; selected installation is {}",
            crate::game_manifest::TARGET_BUILD,
            version.trim()
        );
    }
    if crate::game_manifest::sha256_file(executable)? != crate::game_manifest::TARGET_EXE_SHA256
        || crate::game_manifest::sha256_file(pak)? != crate::game_manifest::TARGET_PAK_SHA256
    {
        bail!("selected DKO files do not match the supported stock build");
    }
    Ok(())
}

fn copy_or_link_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(source).follow_links(true) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            if let Err(link_error) = fs::hard_link(entry.path(), &target) {
                let size = entry.metadata()?.len();
                if size > 64 * 1024 * 1024 {
                    bail!(
                        "cannot create a space-efficient link for {} ({} bytes): {}; refusing to copy a large retail file",
                        entry.path().display(),
                        size,
                        link_error
                    );
                }
                fs::copy(entry.path(), &target)?;
            }
        }
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct InstalledRuntime {
    manifest: crate::game_manifest::GameManifest,
    layout: String,
}

fn installed_runtime(runtime: &Path) -> Option<InstalledRuntime> {
    fs::read(runtime.join(".project-knockout-runtime.json"))
        .ok()
        .and_then(|body| serde_json::from_slice(&body).ok())
}

fn installed_runtime_is_current(
    runtime: &Path,
    expected: &crate::game_manifest::GameManifest,
) -> bool {
    let Some(installed) = installed_runtime(runtime) else {
        return false;
    };
    if crate::game_manifest::validate_manifest(expected).is_err() {
        return false;
    }
    installed.manifest == *expected
        && installed.layout == "manifest-runtime-v1"
        && expected.managed_files.iter().all(|file| {
            let path = runtime.join(&file.path);
            fs::metadata(&path)
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == file.size)
                && crate::game_manifest::sha256_file(&path)
                    .is_ok_and(|hash| hash.eq_ignore_ascii_case(&file.sha256))
        })
}

fn download_managed_file(
    server: &url::Url,
    expected: &crate::game_manifest::ManagedFile,
    destination: &Path,
    ticket: Option<&str>,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let download_path = match ticket {
        Some(ticket) => ticket_routed_path(ticket, &expected.download_path),
        None => expected.download_path.clone(),
    };
    let body = download_verified_with_progress(
        server,
        &download_path,
        expected.size,
        &expected.sha256,
        &mut progress,
    )
    .with_context(|| format!("download and verify game update file {}", expected.path))?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let parent = destination
        .parent()
        .context("game update destination has no parent")?;
    let staged = tempfile::NamedTempFile::new_in(parent)?;
    fs::write(staged.path(), body)?;
    staged.as_file().sync_all()?;
    staged
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "install verified game update file {}",
                destination.display()
            )
        })?;
    Ok(())
}

fn ensure_game_installed(
    splash: Option<&Splash>,
    ticket: Option<&str>,
) -> Result<crate::game_manifest::GameManifest> {
    let (server, _) = external_launcher_settings()?;
    let executable = selected_game_executable()?;
    let (retail_root, pak) = retail_paths(&executable)?;
    if let Some(splash) = splash {
        splash.progress("Checking the game update manifest", 0.24);
    }
    let manifest = fetch_manifest(&server, ticket)?;
    if let Some(splash) = splash {
        splash.progress("Verifying DivineKnockout.exe and the stock game PAK", 0.28);
    }
    verify_retail_build(&executable, &pak)?;
    // Require the release artifact before touching any installed runtime.
    crate::game_manifest::validate_manifest(&manifest)?;
    let data = game_data_directory()?;
    fs::create_dir_all(&data)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(data.join("install.lock"))?;
    lock.try_lock_exclusive()
        .context("another game update install or removal is already running")?;
    let runtime = runtime_directory()?;
    remove_inactive_generations(&data)?;
    if let Some(splash) = splash {
        splash.progress("Checking installed game update files", 0.36);
    }
    if installed_runtime_is_current(&runtime, &manifest) {
        if let Some(splash) = splash {
            splash.progress("Game update files are current", 0.9);
        }
        return Ok(manifest);
    }
    if let Some(splash) = splash {
        splash.progress("Preparing the isolated game update", 0.42);
    }
    let staging = runtime_generation_directory(&runtime, "staging");
    let previous = runtime_generation_directory(&runtime, "previous");
    for inactive in [&staging, &previous] {
        if inactive.exists() {
            fs::remove_dir_all(inactive)?;
        }
    }
    fs::create_dir_all(&staging)?;
    copy_or_link_tree(&retail_root, &staging)?;
    let platform_managed_files = &manifest.managed_files;
    let total_download_size = platform_managed_files
        .iter()
        .map(|file| file.size)
        .sum::<u64>();
    let mut completed_download_size = 0u64;
    for managed in platform_managed_files {
        let display_name = Path::new(&managed.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&managed.path)
            .to_owned();
        download_managed_file(
            &server,
            managed,
            &staging.join(&managed.path),
            ticket,
            |file_downloaded, _| {
                let downloaded = completed_download_size.saturating_add(file_downloaded);
                let fraction = if total_download_size == 0 {
                    1.0
                } else {
                    downloaded as f32 / total_download_size as f32
                };
                if let Some(splash) = splash {
                    splash.progress(
                        format!(
                            "Downloading game update: {display_name} — {} / {}",
                            format_download_size(downloaded),
                            format_download_size(total_download_size)
                        ),
                        0.5 + 0.34 * fraction,
                    );
                }
            },
        )?;
        completed_download_size = completed_download_size.saturating_add(managed.size);
    }
    if let Some(splash) = splash {
        splash.progress("Verifying downloaded game update files", 0.85);
    }
    fs::write(
        staging.join(".project-knockout-runtime.json"),
        serde_json::to_vec_pretty(&InstalledRuntime {
            manifest: manifest.clone(),
            layout: "manifest-runtime-v1".to_owned(),
        })?,
    )?;
    if runtime.exists() {
        fs::rename(&runtime, &previous)?;
    }
    if let Some(splash) = splash {
        splash.progress("Installing the verified game update", 0.88);
    }
    if let Err(error) = fs::rename(&staging, &runtime) {
        if previous.exists() && !runtime.exists() {
            let _ = fs::rename(&previous, &runtime);
        }
        return Err(error).context("activate the staged game update runtime");
    }
    if previous.exists() {
        fs::remove_dir_all(previous)?;
    }
    if let Some(splash) = splash {
        splash.progress("Game update installed", 0.9);
    }
    Ok(manifest)
}

#[cfg(windows)]
fn setup_from_download() -> Result<()> {
    let current = std::env::current_exe().context("resolve downloaded DKO setup")?;
    let server_url = match crate::windows_setup::server_url_from_setup_executable(&current) {
        Ok(server_url) => server_url,
        Err(error) => {
            if external_launcher_settings().is_ok() {
                windows_message_box(
                    "DKO Launcher",
                    "The Project KNOCKOUT launcher is already installed.\n\nReturn to the portal and choose Launch Divine Knockout.",
                    false,
                );
                return Ok(());
            }
            return Err(error);
        }
    };
    let pinned = validate_pinned_server_url(&server_url)?;
    install_protocol(pinned.as_str(), &default_steam_root())?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn setup_from_download() -> Result<()> {
    let current = std::env::current_exe().context("resolve downloaded Project Knockout setup")?;
    let server_url = crate::windows_setup::server_url_from_setup_executable(&current)
        .context("This installer is not bound to a Project Knockout server. Download it again from the website.")?;
    let pinned = validate_pinned_server_url(&server_url)?;
    install_protocol(pinned.as_str(), &default_steam_root())
}

#[cfg(target_os = "linux")]
fn linux_user_directory(xdg_variable: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(value) = std::env::var_os(xdg_variable) {
        return Ok(PathBuf::from(value));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(fallback))
        .with_context(|| format!("{xdg_variable} and HOME are unavailable"))
}

#[cfg(target_os = "linux")]
fn linux_launcher_settings_path() -> Result<PathBuf> {
    Ok(linux_user_directory("XDG_CONFIG_HOME", ".config")?.join("ProjectKNOCKOUT/launcher.json"))
}

#[cfg(target_os = "linux")]
fn legacy_linux_launcher_settings_path() -> Result<PathBuf> {
    Ok(linux_user_directory("XDG_CONFIG_HOME", ".config")?.join("dko-preservation/launcher.json"))
}

#[cfg(target_os = "linux")]
fn linux_installed_executable() -> Result<PathBuf> {
    Ok(linux_user_directory("XDG_DATA_HOME", ".local/share")?
        .join("ProjectKNOCKOUT/Project-KNOCKOUT"))
}

#[cfg(target_os = "linux")]
fn linux_desktop_entry() -> Result<PathBuf> {
    Ok(linux_user_directory("XDG_DATA_HOME", ".local/share")?
        .join("applications/Project-KNOCKOUT.desktop"))
}

#[cfg(target_os = "linux")]
fn external_launcher_settings() -> Result<(url::Url, PathBuf)> {
    let settings = linux_launcher_settings()?;
    Ok((
        validate_pinned_server_url(&settings.server_url)?,
        settings.steam_root,
    ))
}

#[cfg(target_os = "linux")]
fn linux_launcher_settings() -> Result<LinuxLauncherSettings> {
    let path = linux_launcher_settings_path()?;
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::PermissionsExt;

            let legacy = legacy_linux_launcher_settings_path()?;
            let body = std::fs::read(&legacy).with_context(|| {
                format!("read launcher settings {}; run setup again", path.display())
            })?;
            // Validate before migrating so malformed legacy state is never
            // promoted into the canonical launcher directory.
            let _: LinuxLauncherSettings = serde_json::from_slice(&body).context(
                "decode legacy Linux launcher settings; run the latest setup and select DivineKnockout.exe",
            )?;
            std::fs::create_dir_all(
                path.parent()
                    .context("Linux launcher settings path has no parent")?,
            )?;
            let staged = path.with_extension("new");
            std::fs::write(&staged, &body)?;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&staged, &path)?;
            body
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read launcher settings {}; run setup again", path.display())
            });
        }
    };
    serde_json::from_slice(&body).context(
        "decode Linux launcher settings; run the latest setup and select DivineKnockout.exe",
    )
}

#[cfg(target_os = "linux")]
fn validate_linux_game_executable(path: PathBuf) -> Result<PathBuf> {
    let nested = path
        .parent()
        .map(|directory| directory.join("DivineKnockout/Binaries/Win64/DivineKnockout.exe"))
        .filter(|candidate| candidate.is_file());
    let path = nested.unwrap_or(path);
    if !path.is_file()
        || !path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("DivineKnockout.exe"))
    {
        bail!(
            "select the retail executable named DivineKnockout.exe (received {})",
            path.display()
        );
    }
    Ok(path)
}

#[cfg(target_os = "linux")]
fn select_linux_game_executable(steam_root: &Path) -> Result<PathBuf> {
    let suggested = steam_root
        .join("steamapps/common/Divine Knockout/DivineKnockout/Binaries/Win64/DivineKnockout.exe");
    let graphical =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();

    if graphical {
        let mut zenity = std::process::Command::new("zenity");
        zenity
            .arg("--file-selection")
            .arg("--title=Select DivineKnockout.exe")
            .arg("--file-filter=DivineKnockout.exe | DivineKnockout.exe");
        if suggested.is_file() {
            zenity.arg(format!("--filename={}", suggested.display()));
        }
        match zenity.output() {
            Ok(output) if output.status.success() => {
                return validate_linux_game_executable(PathBuf::from(
                    String::from_utf8(output.stdout)
                        .context("Linux file picker returned an invalid path")?
                        .trim(),
                ));
            }
            Ok(_) => bail!("DivineKnockout.exe selection was canceled"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("open the Linux DivineKnockout.exe picker"),
        }

        let start = suggested.parent().unwrap_or(steam_root);
        match std::process::Command::new("kdialog")
            .arg("--getopenfilename")
            .arg(start)
            .arg("DivineKnockout.exe|DivineKnockout.exe")
            .arg("--title")
            .arg("Select DivineKnockout.exe")
            .output()
        {
            Ok(output) if output.status.success() => {
                return validate_linux_game_executable(PathBuf::from(
                    String::from_utf8(output.stdout)
                        .context("Linux file picker returned an invalid path")?
                        .trim(),
                ));
            }
            Ok(_) => bail!("DivineKnockout.exe selection was canceled"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("open the Linux DivineKnockout.exe picker"),
        }
    }

    use std::io::Write as _;
    let default = suggested.is_file().then_some(suggested);
    eprint!(
        "Path to DivineKnockout.exe{}: ",
        default
            .as_ref()
            .map(|path| format!(" [{}]", path.display()))
            .unwrap_or_default()
    );
    std::io::stderr().flush()?;
    let mut selected = String::new();
    std::io::stdin()
        .read_line(&mut selected)
        .context("read DivineKnockout.exe path")?;
    let selected = selected.trim();
    let path = if selected.is_empty() {
        default.context("DivineKnockout.exe path is required")?
    } else {
        PathBuf::from(selected)
    };
    validate_linux_game_executable(path)
}

#[cfg(target_os = "linux")]
fn desktop_exec_argument(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .context("Linux launcher installation path is not valid UTF-8")?;
    let mut escaped = String::with_capacity(path.len() + 2);
    escaped.push('"');
    for value in path.chars() {
        match value {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '`' => escaped.push_str("\\`"),
            '$' => escaped.push_str("\\$"),
            _ => escaped.push(value),
        }
    }
    escaped.push('"');
    Ok(escaped)
}

#[cfg(target_os = "linux")]
fn project_knockout_desktop_entry(installed_executable: &Path) -> Result<String> {
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=Project KNOCKOUT Launcher\nComment=Launch Divine Knockout from Project KNOCKOUT\nExec={} __protocol-launch %u\nTerminal=false\nNoDisplay=true\nMimeType=x-scheme-handler/project-knockout;\nCategories=Game;\n",
        desktop_exec_argument(installed_executable)?
    ))
}

#[cfg(target_os = "linux")]
fn remove_project_knockout_mime_type(body: &str) -> Option<String> {
    const MIME: &str = "x-scheme-handler/project-knockout";

    let mut changed = false;
    let mut output = Vec::new();
    for line in body.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(value) = line.strip_prefix("MimeType=") else {
            output.push(line.to_owned());
            continue;
        };
        let mut removed = false;
        let retained = value
            .split(';')
            .filter(|value| {
                let keep = !value.eq_ignore_ascii_case(MIME);
                removed |= !keep;
                keep && !value.is_empty()
            })
            .collect::<Vec<_>>();
        if !removed {
            output.push(line.to_owned());
            continue;
        }
        changed = true;
        if !retained.is_empty() {
            output.push(format!("MimeType={};", retained.join(";")));
        }
    }
    changed.then(|| {
        let mut body = output.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        body
    })
}

#[cfg(target_os = "linux")]
fn unregister_competing_linux_protocol_handlers(canonical: &Path) -> Result<usize> {
    let applications = canonical
        .parent()
        .context("Linux desktop entry has no applications directory")?;
    if !applications.is_dir() {
        return Ok(0);
    }
    let mut changed = 0;
    for entry in walkdir::WalkDir::new(applications)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if path == canonical
            || !entry.file_type().is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("desktop")
        {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(cleaned) = remove_project_knockout_mime_type(&body) else {
            continue;
        };
        let permissions = std::fs::metadata(path)?.permissions();
        std::fs::write(path, cleaned)
            .with_context(|| format!("unregister competing protocol handler {}", path.display()))?;
        std::fs::set_permissions(path, permissions)?;
        changed += 1;
    }
    Ok(changed)
}

#[cfg(target_os = "linux")]
fn install_protocol(server_url: &str, steam_root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server_url = validate_pinned_server_url(server_url)?;
    let setup = SetupFlow::start();
    setup.wait_for_open()?;
    setup.progress("Choose DivineKnockout.exe in the file window");
    let game_executable = select_linux_game_executable(steam_root)?;
    setup.progress("Verifying original game files");
    let (_, stock_pak) = retail_paths(&game_executable)?;
    verify_retail_build(&game_executable, &stock_pak)?;
    setup.progress("Installing Project Knockout launcher");
    let installed_executable = linux_installed_executable()?;
    std::fs::create_dir_all(
        installed_executable
            .parent()
            .context("Linux launcher installation path has no parent")?,
    )?;
    let current = std::env::current_exe().context("resolve downloaded launcher executable")?;
    replace_installed_launcher(&current, &installed_executable)?;
    std::fs::set_permissions(
        &installed_executable,
        std::fs::Permissions::from_mode(0o700),
    )?;

    let settings_path = linux_launcher_settings_path()?;
    std::fs::create_dir_all(
        settings_path
            .parent()
            .context("Linux launcher settings path has no parent")?,
    )?;
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&LinuxLauncherSettings {
            server_url: server_url.as_str().to_owned(),
            steam_root: steam_root.to_path_buf(),
            game_executable,
        })?,
    )?;
    std::fs::set_permissions(&settings_path, std::fs::Permissions::from_mode(0o600))?;

    let desktop_path = linux_desktop_entry()?;
    std::fs::create_dir_all(
        desktop_path
            .parent()
            .context("Linux desktop entry path has no parent")?,
    )?;
    unregister_competing_linux_protocol_handlers(&desktop_path)?;
    let desktop = project_knockout_desktop_entry(&installed_executable)?;
    std::fs::write(&desktop_path, desktop)?;
    std::fs::set_permissions(&desktop_path, std::fs::Permissions::from_mode(0o644))?;
    let desktop_name = desktop_path
        .file_name()
        .and_then(|value| value.to_str())
        .context("Linux desktop entry filename is invalid")?;
    let status = std::process::Command::new("xdg-mime")
        .args(["default", desktop_name, "x-scheme-handler/project-knockout"])
        .status()
        .context("run xdg-mime to register project-knockout protocol")?;
    if !status.success() {
        bail!("xdg-mime could not register the project-knockout protocol handler");
    }
    if let Some(applications) = desktop_path.parent() {
        let status = std::process::Command::new("update-desktop-database")
            .arg(applications)
            .status();
        if let Ok(status) = status {
            if !status.success() {
                bail!("update-desktop-database could not register the Project KNOCKOUT handler");
            }
        }
    }
    setup.finish()
}

fn launch_from_protocol(uri: &str) -> Result<()> {
    if is_repair_protocol_uri(uri)? {
        return repair_launcher_configuration();
    }
    let launch = parse_protocol_uri(uri)?;
    let splash = Splash::start();
    splash.progress("Starting Project KNOCKOUT launcher", 0.02);
    let (mut launch_url, steam_root) = external_launcher_settings()?;
    append_launcher_log(format!(
        "protocol launch accepted custom_args={}",
        launch.command_args.len()
    ));
    let result = (|| -> Result<()> {
        // Updating must be the first authenticated operation. Older launchers
        // can otherwise fail while reading a moved game path or preparing the
        // former Steam-adjacent runtime and never reach their updater.
        splash.progress("Authenticating this launch request", 0.04);
        acknowledge_protocol_launch(&launch_url, &launch.ticket)?;
        splash.progress("Launch request authenticated", 0.06);
        let _ =
            report_protocol_launch_status(&launch_url, &launch.ticket, "launcher_updating", None);
        if ensure_launcher_current(&launch_url, uri, &splash)? {
            append_launcher_log("launcher update or canonical handoff started");
            splash.close();
            return Ok(());
        }

        #[cfg(windows)]
        let selected = match selected_game_executable() {
            Ok(selected) => selected,
            Err(_) => {
                splash.progress("Finding Divine Knockout through Steam", 0.03);
                splash.show_now_and_wait();
                crate::process::discover_or_select_and_remember_game_executable(&steam_root)?
            }
        };
        #[cfg(target_os = "linux")]
        let selected = selected_game_executable()?;
        let (_, selected_pak) = retail_paths(&selected)?;
        append_launcher_log(format!("selected executable {}", selected.display()));
        verify_retail_build(&selected, &selected_pak)?;
        append_launcher_log("retail build verification passed");
        let _ =
            report_protocol_launch_status(&launch_url, &launch.ticket, "preflight_passed", None);
        splash.progress("Verified the selected Divine Knockout installation", 0.22);
        launch_url.set_path(&ticket_routed_path(&launch.ticket, ""));
        append_launcher_log("verifying the server-managed game runtime");
        let manifest = ensure_game_installed(Some(&splash), Some(&launch.ticket))?;
        let executable =
            runtime_directory()?.join(&crate::game_manifest::client_executable(&manifest)?.path);
        let paths = crate::process::Paths::new_explicit(
            steam_root.clone(),
            None,
            Some(&game_compat_directory()?),
            &executable,
            LAUNCH_HOMEDIR,
        )?;
        if !paths.exe.is_file() {
            bail!("DKO executable missing: {}", paths.exe.display());
        }
        splash.progress("Starting Divine Knockout", 0.94);
        let _ =
            report_protocol_launch_status(&launch_url, &launch.ticket, "process_starting", None);
        let command_args = launch.command_args.clone();
        let _child = crate::process::launch(
            &paths,
            launch_url.as_str(),
            LAUNCH_HOMEDIR,
            &launch.username,
            &command_args,
        )?;
        append_launcher_log("DivineKnockout process spawn succeeded");
        let _ = report_protocol_launch_status(&launch_url, &launch.ticket, "process_started", None);
        splash.progress("Divine Knockout started", 1.0);
        std::thread::sleep(std::time::Duration::from_millis(250));
        splash.close();
        Ok(())
    })();
    if let Err(error) = &result {
        let failure_code = launcher_failure_code(error);
        append_launcher_log(format!("launch failed code={failure_code} error={error:#}"));
        let _ = report_protocol_launch_status(
            &launch_url,
            &launch.ticket,
            "failed",
            Some(failure_code),
        );
    }
    result
}

fn doctor() -> Result<()> {
    let (server_url, steam_root) = external_launcher_settings()?;
    #[cfg(windows)]
    let _ = &steam_root;
    #[cfg(windows)]
    let paths = crate::process::Paths::new_selected(LAUNCH_HOMEDIR)?;
    #[cfg(target_os = "linux")]
    let paths = {
        let settings = linux_launcher_settings()?;
        crate::process::Paths::new_selected(
            steam_root,
            None,
            &settings.game_executable,
            LAUNCH_HOMEDIR,
        )?
    };
    if !paths.exe.is_file() {
        bail!("Divine Knockout was not found at {}", paths.exe.display());
    }
    let (_, pak) = retail_paths(&paths.exe)?;
    verify_retail_build(&paths.exe, &pak)?;
    fetch_manifest(&server_url, None)?;
    println!("DKO launcher readiness checks passed.");
    #[cfg(windows)]
    windows_message_box(
        "DKO Launcher Ready",
        "All launcher checks passed. Return to the portal and choose Launch Divine Knockout.",
        false,
    );
    Ok(())
}

#[cfg(windows)]
fn select_game() -> Result<()> {
    let setup = SetupFlow::start();
    setup.wait_for_open()?;
    setup.progress("Select the Divine Knockout executable");
    let selected = setup.select_windows_game()?;
    setup.progress("Verifying original game files");
    let (_, pak) = retail_paths(&selected)?;
    verify_retail_build(&selected, &pak)?;
    crate::process::remember_game_executable(&selected)?;
    setup.finish()?;
    windows_message_box(
        "Project KNOCKOUT",
        "The selected Divine Knockout installation is ready. Return to the portal and choose Launch game.",
        false,
    );
    Ok(())
}

fn dispatch() -> Result<()> {
    #[cfg(any(windows, target_os = "linux"))]
    if std::env::args_os().nth(1).is_none() {
        return setup_from_download();
    }
    match Cli::parse().command {
        Command::InstallProtocol {
            server_url,
            steam_root,
        } => install_protocol(&server_url, &steam_root),
        Command::InternalProtocolLaunch { uri } => launch_from_protocol(&uri),
        #[cfg(windows)]
        Command::SelectGame => select_game(),
        Command::Doctor => doctor(),
        Command::Update => {
            let manifest = ensure_game_installed(None, None)?;
            println!(
                "Game update {} is verified and installed.",
                manifest.version
            );
            Ok(())
        }
    }
}

fn player_facing_error(error: &anyhow::Error) -> String {
    let details = format!("{error:#}").to_ascii_lowercase();
    if details.contains("proton") || details.contains("compatibility tool") {
        return format!("Proton could not be found or started.\n\nIn Steam, open Library > Tools and install or verify the Proton version you want to use. Then open Divine Knockout > Properties > Compatibility and select that installed version. Let Steam finish downloading it, then try launching again.\n\nDetails: {error:#}");
    }
    if details.contains("executable missing") || details.contains("divine knockout was not found") {
        return "Divine Knockout could not be found. Use Repair on the Project KNOCKOUT website to select DivineKnockout.exe.".to_owned();
    }
    if details.contains("supported stock")
        || details.contains("game launch requires dko")
        || details.contains("supported dko build")
    {
        return "The selected DivineKnockout.exe does not match the supported Project KNOCKOUT game build. Restore the original supported DKO files, then use Repair on the Project KNOCKOUT website to select that executable.".to_owned();
    }
    if details.contains("not configured") || details.contains("setup") {
        return "Use Repair on the Project KNOCKOUT website. If the launcher does not open, download and run the latest setup there.".to_owned();
    }
    "Divine Knockout could not be launched. Download and run the latest launcher setup from the Project KNOCKOUT portal if the problem continues.".to_owned()
}

#[cfg(windows)]
fn player_wants_repair(message: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONERROR, MB_SETFOREGROUND, MB_YESNO,
    };
    let title = "Project KNOCKOUT Launcher"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let prompt = format!(
        "{message}\n\nWould you like to repair the launcher and choose DivineKnockout.exe now?\n\nDebug log: {}",
        launcher_log_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "unavailable".to_owned())
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect::<Vec<_>>();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            prompt.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_SETFOREGROUND | MB_ICONERROR,
        ) == IDYES
    }
}

#[cfg(target_os = "linux")]
fn linux_error_dialog(message: &str, offer_repair: bool) -> bool {
    let log = launcher_log_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unavailable".to_owned());
    let prompt = format!(
        "{message}\n\nDebug log: {log}{}",
        if offer_repair {
            "\n\nRepair the launcher and choose DivineKnockout.exe now?"
        } else {
            ""
        }
    );
    eprintln!("{prompt}");
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return false;
    }
    for program in ["zenity", "kdialog"] {
        let args = linux_error_dialog_args(program, &prompt, offer_repair);
        match std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(output) if output.status.success() => return offer_repair,
            // GTK can warn even after showing a dialog. Only initialization
            // errors should override the player dismissing it.
            Ok(output)
                if output.status.code() == Some(1)
                    && !dialog_initialization_failed(&output.stderr) =>
            {
                return false
            }
            _ => continue,
        }
    }
    builtin_error_dialog(&prompt, offer_repair)
}

#[cfg(target_os = "linux")]
fn dialog_initialization_failed(stderr: &[u8]) -> bool {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    [
        "cannot open display",
        "could not connect to display",
        "failed to open display",
        "could not load the qt platform plugin",
        "no qt platform plugin could be initialized",
        "unknown option",
        "cannot parse arguments",
    ]
    .iter()
    .any(|error| message.contains(error))
}

#[cfg(target_os = "linux")]
fn linux_error_dialog_args(program: &str, prompt: &str, offer_repair: bool) -> Vec<String> {
    let args = if program == "zenity" {
        let mut args = vec![
            if offer_repair {
                "--question"
            } else {
                "--error"
            },
            "--title=Project KNOCKOUT Launcher",
            "--no-markup",
            "--width=640",
            "--text",
            prompt,
        ];
        if offer_repair {
            args.extend(["--ok-label=Repair", "--cancel-label=Close"]);
        }
        args
    } else {
        let mut args = vec![
            "--title",
            "Project KNOCKOUT Launcher",
            if offer_repair { "--yesno" } else { "--error" },
            prompt,
        ];
        if offer_repair {
            args.extend(["--yes-label", "Repair", "--no-label", "Close"]);
        }
        args
    };
    args.into_iter().map(str::to_owned).collect()
}

// Keep an error visible even on desktops without Zenity or KDialog, using
// the same bundled window/font implementation as the launcher setup UI.
#[cfg(target_os = "linux")]
fn builtin_error_dialog(prompt: &str, offer_repair: bool) -> bool {
    use minifb::{Key, KeyRepeat, Window, WindowOptions};
    const WIDTH: usize = 760;
    const HEIGHT: usize = 520;
    let Ok(mut window) = Window::new(
        "Project KNOCKOUT — Launch failed",
        WIDTH,
        HEIGHT,
        WindowOptions::default(),
    ) else {
        return false;
    };
    window.set_target_fps(30);
    let mut lines = Vec::new();
    for paragraph in prompt.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > 88 {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            for character in word.chars() {
                if line.chars().count() == 88 {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(character);
            }
        }
        lines.push(line);
    }
    let mut offset = 0usize;
    let mut buffer = vec![0x00070b10; WIDTH * HEIGHT];
    while window.is_open() && !window.is_key_down(Key::Escape) {
        if offer_repair && window.is_key_pressed(Key::R, KeyRepeat::No) {
            return true;
        }
        if !offer_repair && window.is_key_pressed(Key::Enter, KeyRepeat::No) {
            break;
        }
        if window.is_key_pressed(Key::Down, KeyRepeat::Yes) {
            offset = (offset + 1).min(lines.len().saturating_sub(20));
        }
        if window.is_key_pressed(Key::Up, KeyRepeat::Yes) {
            offset = offset.saturating_sub(1);
        }
        if let Some((_, scroll)) = window.get_scroll_wheel() {
            if scroll < 0.0 {
                offset = (offset + 3).min(lines.len().saturating_sub(20));
            }
            if scroll > 0.0 {
                offset = offset.saturating_sub(3);
            }
        }
        buffer.fill(0x00070b10);
        draw_setup_text(
            &mut buffer,
            WIDTH,
            HEIGHT,
            24,
            16,
            "Unable to launch Divine Knockout",
            2,
            0x00ffffff,
        );
        for (index, line) in lines.iter().skip(offset).take(20).enumerate() {
            draw_setup_text(
                &mut buffer,
                WIDTH,
                HEIGHT,
                24,
                60 + index * 20,
                line,
                1,
                0x00ffffff,
            );
        }
        draw_setup_text(
            &mut buffer,
            WIDTH,
            HEIGHT,
            24,
            480,
            if offer_repair {
                "R: Repair     Esc: Close     Up/Down or scroll: More details"
            } else {
                "Enter/Esc: Close     Up/Down or scroll: More details"
            },
            1,
            0x00c4f66c,
        );
        if window.update_with_buffer(&buffer, WIDTH, HEIGHT).is_err() {
            break;
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn player_wants_repair(message: &str) -> bool {
    linux_error_dialog(
        message,
        !message.starts_with("Proton could not be found or started."),
    )
}

fn repair_launcher_configuration() -> Result<()> {
    // Recover the trusted origin locally, never from the repair URI. Do not
    // erase configuration or runtime files before the player selects and
    // validates a replacement; cancelling repair must keep the working setup.
    #[cfg(windows)]
    let configured_server = {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(r"Software\DKOPreservation")
            .ok()
            .and_then(|settings| settings.get_value::<String, _>("ServerUrl").ok())
            .and_then(|value| validate_pinned_server_url(&value).ok())
    };
    #[cfg(target_os = "linux")]
    let configured_server = external_launcher_settings().map(|value| value.0).ok();
    let server = configured_server
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| {
                    crate::windows_setup::server_url_from_setup_executable(&path).ok()
                })
                .and_then(|value| validate_pinned_server_url(&value).ok())
        })
        .context("the trusted server address could not be recovered; download and run setup again from the portal")?;
    // Both platforms always show the picker, verify retail files, then save the
    // selection and reconstruct the browser handler. Game updates are verified
    // normally on the next launch without deleting a possibly active runtime.
    install_protocol(server.as_str(), &default_steam_root())
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("__p2p-bridge") {
        return crate::p2p::bridge::run_from_stdin();
    }
    crate::p2p::bridge::enable_in_launcher();
    begin_launcher_log();
    match dispatch() {
        Ok(()) => {
            append_launcher_log("launcher operation completed successfully");
            Ok(())
        }
        Err(error) => {
            if format!("{error:#}").contains("selection was canceled") {
                append_launcher_log("player canceled game selection; saved configuration retained");
                return Ok(());
            }
            let message = player_facing_error(&error);
            append_launcher_log(format!("launcher operation failed: {error:#}"));
            if player_wants_repair(&message) {
                append_launcher_log("player accepted automatic repair");
                match repair_launcher_configuration() {
                    Ok(()) => {
                        append_launcher_log("automatic repair completed successfully");
                        #[cfg(windows)]
                        windows_message_box(
                            "Project KNOCKOUT Launcher",
                            "Repair completed. Return to the portal and choose Launch game again.",
                            false,
                        );
                        return Ok(());
                    }
                    Err(repair_error) => {
                        append_launcher_log(format!("automatic repair failed: {repair_error:#}"));
                        #[cfg(target_os = "linux")]
                        linux_error_dialog(
                            &format!(
                                "Repair could not complete.\n\n{}",
                                player_facing_error(&repair_error)
                            ),
                            false,
                        );
                        #[cfg(windows)]
                        windows_message_box(
                            "Project KNOCKOUT Launcher Repair",
                            &format!(
                                "Repair could not complete.\n\n{}",
                                player_facing_error(&repair_error)
                            ),
                            true,
                        );
                    }
                }
            } else {
                #[cfg(windows)]
                windows_message_box("Project KNOCKOUT Launcher", &message, true);
            }
            Err(anyhow::anyhow!(message))
        }
    }
}
