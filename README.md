# Switchboard Term

**Network Operations Console.** A session-oriented terminal for macOS, inspired by MobaXterm. Patch into your
whole fleet — SSH, serial consoles, SFTP, RDP, VNC — from one lightweight
native app.

Built with [Tauri 2](https://tauri.app) (Rust backend) and
[xterm.js](https://xtermjs.org). Around 20 MB instead of an Electron-sized
footprint, with credentials headed for the macOS Keychain rather than a
config file.

> **Status: v0.2.** Local shells, SSH with saved sessions, keychain-backed
> credentials, host-key verification, per-session logging, and multi-exec.

## Features

- **Nested groups with drag & drop** (v0.4): groups can hold subgroups to any
  depth — "Agrosuper › Rosario › Core". Drag a group onto another to nest it,
  or onto the root drop-zone to promote it to top level; drag sessions between
  groups the same way. Moving a group into its own subtree is rejected, and
  deleting a parent cascades to everything inside.
- **Command snippets** (v0.4): an editable bar along the bottom with your
  frequent commands (seeded with Cisco show-commands). Click sends to the
  active session, or to every broadcast-selected tab when multi-exec is on.
  The Run/Insert toggle decides whether a click executes (with Enter) or just
  types the command for review. Every snippet's label and command are
  editable, and the set is stored in SQLite.

- **Session sidebar**: groups (one per client, site, or lab) with saved
  sessions in SQLite. Double-click to connect; filter by name, host, or user.
- **Credentials in the macOS Keychain**: passwords and key passphrases never
  touch the database or any config file.
- **Host-key verification (TOFU)**: first contact records the key in
  `~/.ssh/known_hosts`; a changed key refuses the connection and raises a
  red alert (possible MITM).
- **Automatic session logging**: every SSH session streams to
  `~/Library/Application Support/switchboard-term/logs/<date>/<name>_<time>.log`.
  One click in the sidebar opens the folder in Finder.
- **Multi-exec broadcast** (⌘B): mark tabs with the ⚡ toggle and send one
  command to all of them at once — firmware audits across a whole site in
  one keystroke.
- **Keyboard shortcuts**: ⌘T local shell, ⌘N new SSH, ⌘W close tab,
  ⌘1–9 jump to tab, ⌃Tab cycle, ⌘B broadcast bar.
- **RDP client** (v0.3): connect to Windows hosts via FreeRDP. Saved RDP
  sessions live in the same sidebar as SSH (with a monitor icon), credentials
  in the Keychain, domain support for AD, dynamic or fixed resolution, and
  shared clipboard. Certificates are pinned on first use (`/cert:tofu`), and
  the password is passed over stdin — never on the command line. Requires
  FreeRDP: `brew install freerdp` (the app detects it and prompts if missing).
- Tabbed terminal with a live "jack lamp" per session (amber = connecting,
  green = live, off = dead), local shells via `portable-pty`, SSH via
  `russh` with password (with keyboard-interactive fallback for appliances like F5) or private-key auth, xterm.js with 10k scrollback. See **Clipboard & URLs** below for copy/paste and link handling.

## Clipboard & URLs

The terminal reports an iTerm2-class identity to programs it runs
(`TERM_PROGRAM=iTerm.app`, `LC_TERMINAL=iTerm2`, `COLORTERM=truecolor`), so
CLIs that gate features on the terminal — Claude Code, `gh`, `gitui` — emit
**OSC 8 hyperlinks**: the full URL travels inside the escape sequence and stays
intact no matter how the visible text is wrapped across lines. Switchboard then
lets you follow or copy that full URL:

| Shortcut | Action |
| -------- | ------ |
| `⌘C` | Copy the selection. With no selection but the mouse over a link, copy that link's **full URL**. |
| `⌘⇧C` | Copy the full URL of the link under the mouse (hover it first). |
| `⌘⇧O` | Open the URL under the mouse (or in the selection / cursor line) in the system browser. |
| `⌘V` | Paste. Right-click pastes (or copies a selection); middle-click pastes. |
| `⌘/⌃-click` | Open a link in the browser (when the running app hasn't grabbed the mouse). |

Hold **⌥ (Option) and drag** to select text even inside full-screen apps like
Claude Code or vim, which grab the mouse for their own use. For plain-text URLs
(not OSC 8), long lines wrapped by the terminal are rejoined on copy via
xterm's `isWrapped` handling.

> **SSH note:** the iTerm2 identity is sent to the remote host as environment
> variables. `LC_*` is forwarded by default sshd (`AcceptEnv LANG LC_*`), but
> `TERM_PROGRAM` only arrives if the server's `/etc/ssh/sshd_config` also lists
> it: `AcceptEnv TERM_PROGRAM TERM_PROGRAM_VERSION LC_TERMINAL LC_TERMINAL_VERSION`
> (then restart sshd). Verify inside the session with `echo $TERM_PROGRAM`.

## Roadmap

| Version | Focus |
| ------- | ----- |
| v0.5 | SFTP side panel synced to the active session, serial consoles, telnet, jump hosts |
| v0.4 | Multi-exec: broadcast commands to N selected sessions |
| v0.5 | VNC via embedded noVNC with a Rust TCP↔WebSocket bridge |
| v0.6 | RDP via [IronRDP](https://github.com/Devolutions/IronRDP) |
| v1.0 | Graphical SSH tunnels, snippets, MobaXterm/SecureCRT import, signed + notarized DMG, Homebrew cask, CI releases |

## Building from source

Requirements (macOS):

1. Xcode Command Line Tools: `xcode-select --install`
2. Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
3. Node.js ≥ 20 and pnpm: `corepack enable pnpm`

Then:

```sh
pnpm install
pnpm tauri icon src-tauri/app-icon.png   # generates all icon sizes (first time only)
pnpm tauri dev                           # run in development
pnpm tauri build                         # produce Switchboard Term.app + .dmg
```

The first build compiles the full Rust dependency tree and takes a few
minutes; subsequent builds are incremental and fast.

## Known limitations in v0.2

- First-contact host keys are trusted automatically (TOFU) and announced with
  a notice; an interactive accept/reject prompt is planned.
- If a saved session has no password in the keychain (e.g. restored on a new
  Mac), the tab shows the error — edit the session and re-enter it.
- Jump hosts and per-session log toggles land in v0.3.

## Architecture

```
frontend (WebView)          backend (Rust)
┌─────────────────┐  IPC   ┌──────────────────────────┐
│ React + xterm.js│ ─────▶ │ commands: create / write │
│ one Terminal    │ ◀───── │ resize / close           │
│ per tab         │ channel│                          │
└─────────────────┘ (bytes)│ SessionManager           │
                           │  ├─ LocalPty (threads)   │
                           │  └─ Ssh (tokio task)     │
                           └──────────────────────────┘
```

Every session streams raw bytes to its tab through a dedicated Tauri channel;
keystrokes travel back through commands. Session death is announced app-wide
with a `session-exit` event.

## License

Apache-2.0 — see [LICENSE](LICENSE).
