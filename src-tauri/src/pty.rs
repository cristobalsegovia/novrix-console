//! Local terminal sessions backed by a native PTY (portable-pty).
//! The PTY reader is blocking, so we drive it from plain std threads and
//! stream every chunk to the frontend through a Tauri channel.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

use crate::session::{SessionExit, SessionHandle, SessionManager};

pub fn spawn_local(
    app: AppHandle,
    manager: &SessionManager,
    cols: u16,
    rows: u16,
    on_data: Channel<Vec<u8>>,
) -> Result<String, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.arg("-l"); // login shell so the user's profile loads
    cmd.env("TERM", "xterm-256color");
    // Advertise an iTerm2-class identity so CLIs that gate features on the
    // terminal (Claude Code, gh, etc.) emit OSC 8 hyperlinks and truecolor.
    // Claude Code only emits OSC 8 URLs when TERM_PROGRAM is a known terminal
    // (iTerm.app/ghostty/kitty/…); without this it prints raw URLs that break
    // across wrapped lines. LC_TERMINAL mirrors iTerm and survives SSH.
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "iTerm.app");
    cmd.env("TERM_PROGRAM_VERSION", "3.5.0");
    cmd.env("LC_TERMINAL", "iTerm2");
    cmd.env("LC_TERMINAL_VERSION", "3.5.0");
    if let Some(home) = dirs::home_dir() {
        cmd.cwd(home);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| e.to_string())?;
    // The slave end must be dropped in the parent, otherwise reads never EOF.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    let id = Uuid::new_v4().to_string();
    manager.insert(
        id.clone(),
        SessionHandle::LocalPty {
            writer: std::sync::Mutex::new(writer),
            master: std::sync::Mutex::new(pair.master),
        },
    );

    // Reader thread: PTY -> frontend.
    {
        let id = id.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if on_data.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = app.emit(
                "session-exit",
                SessionExit {
                    id,
                    reason: "shell exited".into(),
                },
            );
        });
    }

    // Reaper thread: avoid zombies and drop the registry entry on exit.
    {
        let id = id.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            if let Some(state) = app.try_state::<SessionManager>() {
                state.remove(&id);
            }
        });
    }

    Ok(id)
}
