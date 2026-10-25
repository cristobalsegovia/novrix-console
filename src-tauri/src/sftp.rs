//! Integrated SFTP v3 client operating directly over an active SSH session's
//! multiplexed connection (`request_subsystem("sftp")`). No second login or
//! extra crate dependencies required; works seamlessly through Bastion hosts.

use std::path::PathBuf;

use serde::Serialize;
use tauri::State;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::session::SessionManager;

const SSH_FXP_INIT: u8 = 1;
const SSH_FXP_VERSION: u8 = 2;
const SSH_FXP_OPEN: u8 = 3;
const SSH_FXP_CLOSE: u8 = 4;
const SSH_FXP_READ: u8 = 5;
const SSH_FXP_WRITE: u8 = 6;
const SSH_FXP_OPENDIR: u8 = 11;
const SSH_FXP_READDIR: u8 = 12;
const SSH_FXP_REMOVE: u8 = 13;
const SSH_FXP_MKDIR: u8 = 14;
const SSH_FXP_RMDIR: u8 = 15;
const SSH_FXP_REALPATH: u8 = 16;

const SSH_FXP_STATUS: u8 = 101;
const SSH_FXP_HANDLE: u8 = 102;
const SSH_FXP_DATA: u8 = 103;
const SSH_FXP_NAME: u8 = 104;

const SSH_FX_OK: u32 = 0;
const SSH_FX_EOF: u32 = 1;

const SSH_FXF_READ: u32 = 0x00000001;
const SSH_FXF_WRITE: u32 = 0x00000002;
const SSH_FXF_CREAT: u32 = 0x00000008;
const SSH_FXF_TRUNC: u32 = 0x00000010;

const ATTR_SIZE: u32 = 0x00000001;
const ATTR_UIDGID: u32 = 0x00000002;
const ATTR_PERMISSIONS: u32 = 0x00000004;
const ATTR_ACMODTIME: u32 = 0x00000008;
const ATTR_EXTENDED: u32 = 0x80000000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u32,
    pub permissions: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpDirListing {
    pub path: String,
    pub entries: Vec<SftpEntry>,
}

struct SftpStream<S> {
    stream: S,
    next_id: u32,
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn put_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    put_u32(buf, b.len() as u32);
    buf.extend_from_slice(b);
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_bytes(buf, s.as_bytes());
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.data.len() {
            return Err("unexpected EOF reading u32".into());
        }
        let mut arr = [0u8; 4];
        arr.copy_from_slice(&self.data[self.pos..self.pos + 4]);
        self.pos += 4;
        Ok(u32::from_be_bytes(arr))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        if self.pos + 8 > self.data.len() {
            return Err("unexpected EOF reading u64".into());
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&self.data[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(u64::from_be_bytes(arr))
    }

    fn read_bytes(&mut self) -> Result<Vec<u8>, String> {
        let len = self.read_u32()? as usize;
        if self.pos + len > self.data.len() {
            return Err("unexpected EOF reading bytes".into());
        }
        let out = self.data[self.pos..self.pos + len].to_vec();
        self.pos += len;
        Ok(out)
    }

    fn read_string(&mut self) -> Result<String, String> {
        let b = self.read_bytes()?;
        Ok(String::from_utf8_lossy(&b).into_owned())
    }

    fn read_attrs(&mut self) -> Result<(u64, u32, u32), String> {
        let flags = self.read_u32()?;
        let mut size = 0u64;
        let mut perms = 0u32;
        let mut mtime = 0u32;
        if flags & ATTR_SIZE != 0 {
            size = self.read_u64()?;
        }
        if flags & ATTR_UIDGID != 0 {
            let _uid = self.read_u32()?;
            let _gid = self.read_u32()?;
        }
        if flags & ATTR_PERMISSIONS != 0 {
            perms = self.read_u32()?;
        }
        if flags & ATTR_ACMODTIME != 0 {
            let _atime = self.read_u32()?;
            mtime = self.read_u32()?;
        }
        if flags & ATTR_EXTENDED != 0 {
            let count = self.read_u32()?;
            for _ in 0..count {
                let _ = self.read_bytes()?;
                let _ = self.read_bytes()?;
            }
        }
        Ok((size, perms, mtime))
    }
}

impl<S: AsyncReadExt + AsyncWriteExt + Unpin> SftpStream<S> {
    async fn connect(mut stream: S) -> Result<Self, String> {
        let mut init = Vec::with_capacity(9);
        put_u32(&mut init, 5);
        init.push(SSH_FXP_INIT);
        put_u32(&mut init, 3);
        stream
            .write_all(&init)
            .await
            .map_err(|e| format!("SFTP init write failed: {e}"))?;
        stream.flush().await.map_err(|e| e.to_string())?;

        let mut sftp = Self { stream, next_id: 1 };
        let (pkt_type, _payload) = sftp.recv_packet().await?;
        if pkt_type != SSH_FXP_VERSION {
            return Err(format!("unexpected SFTP handshake reply: {pkt_type}"));
        }
        Ok(sftp)
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    async fn send_packet(&mut self, pkt_type: u8, body: &[u8]) -> Result<(), String> {
        let len = (1 + body.len()) as u32;
        let mut frame = Vec::with_capacity(4 + len as usize);
        put_u32(&mut frame, len);
        frame.push(pkt_type);
        frame.extend_from_slice(body);
        self.stream
            .write_all(&frame)
            .await
            .map_err(|e| e.to_string())?;
        self.stream.flush().await.map_err(|e| e.to_string())
    }

    async fn recv_packet(&mut self) -> Result<(u8, Vec<u8>), String> {
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("SFTP read error: {e}"))?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 || len > 32 * 1024 * 1024 {
            return Err(format!("invalid SFTP packet length: {len}"));
        }
        let mut buf = vec![0u8; len];
        self.stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("SFTP payload read error: {e}"))?;
        Ok((buf[0], buf[1..].to_vec()))
    }

    async fn expect_status(&mut self) -> Result<u32, String> {
        let (t, payload) = self.recv_packet().await?;
        if t != SSH_FXP_STATUS {
            return Err(format!("expected SFTP status, got packet {t}"));
        }
        let mut c = Cursor::new(&payload);
        let _req_id = c.read_u32()?;
        let code = c.read_u32()?;
        if code == SSH_FX_OK || code == SSH_FX_EOF {
            return Ok(code);
        }
        let msg = c.read_string().unwrap_or_else(|_| format!("SFTP error code {code}"));
        Err(if msg.is_empty() {
            format!("SFTP error code {code}")
        } else {
            msg
        })
    }

    async fn realpath(&mut self, path: &str) -> Result<String, String> {
        let req_id = self.alloc_id();
        let mut body = Vec::new();
        put_u32(&mut body, req_id);
        put_str(&mut body, path);
        self.send_packet(SSH_FXP_REALPATH, &body).await?;

        let (t, payload) = self.recv_packet().await?;
        if t == SSH_FXP_NAME {
            let mut c = Cursor::new(&payload);
            let _id = c.read_u32()?;
            let count = c.read_u32()?;
            if count > 0 {
                return c.read_string();
            }
        } else if t == SSH_FXP_STATUS {
            let mut c = Cursor::new(&payload);
            let _id = c.read_u32()?;
            let code = c.read_u32()?;
            let msg = c.read_string().unwrap_or_default();
            return Err(format!("realpath failed ({code}): {msg}"));
        }
        Err("could not resolve remote path".into())
    }

    async fn close_handle(&mut self, handle: &[u8]) -> Result<(), String> {
        let req_id = self.alloc_id();
        let mut body = Vec::new();
        put_u32(&mut body, req_id);
        put_bytes(&mut body, handle);
        self.send_packet(SSH_FXP_CLOSE, &body).await?;
        let _ = self.expect_status().await;
        Ok(())
    }
}

async fn open_sftp_channel(
    manager: &SessionManager,
    session_id: &str,
) -> Result<SftpStream<russh::ChannelStream<russh::client::Msg>>, String> {
    let shared = manager
        .get_ssh(session_id)
        .ok_or_else(|| "This tab does not have an active SSH connection for SFTP".to_string())?;
    let channel = {
        let handle = shared.lock().await;
        handle
            .channel_open_session()
            .await
            .map_err(|e| format!("could not open SFTP channel: {e}"))?
    };
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("remote server rejected SFTP subsystem: {e}"))?;
    SftpStream::connect(channel.into_stream()).await
}

#[tauri::command]
pub async fn sftp_list_dir(
    manager: State<'_, SessionManager>,
    session_id: String,
    path: Option<String>,
) -> Result<SftpDirListing, String> {
    let mut sftp = open_sftp_channel(&manager, &session_id).await?;
    let target = path
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(".");
    let canonical = sftp.realpath(target).await.unwrap_or_else(|_| target.to_string());

    let req_id = sftp.alloc_id();
    let mut body = Vec::new();
    put_u32(&mut body, req_id);
    put_str(&mut body, &canonical);
    sftp.send_packet(SSH_FXP_OPENDIR, &body).await?;

    let (t, payload) = sftp.recv_packet().await?;
    if t != SSH_FXP_HANDLE {
        let mut c = Cursor::new(&payload);
        let _ = c.read_u32();
        let code = c.read_u32().unwrap_or(1);
        let msg = c.read_string().unwrap_or_else(|_| format!("error {code}"));
        return Err(format!("could not open directory {canonical}: {msg}"));
    }
    let handle = {
        let mut c = Cursor::new(&payload);
        let _ = c.read_u32()?;
        c.read_bytes()?
    };

    let mut entries = Vec::new();
    loop {
        let rid = sftp.alloc_id();
        let mut rbody = Vec::new();
        put_u32(&mut rbody, rid);
        put_bytes(&mut rbody, &handle);
        sftp.send_packet(SSH_FXP_READDIR, &rbody).await?;

        let (rt, rpayload) = sftp.recv_packet().await?;
        if rt == SSH_FXP_STATUS {
            break;
        }
        if rt != SSH_FXP_NAME {
            break;
        }
        let mut c = Cursor::new(&rpayload);
        let _ = c.read_u32()?;
        let count = c.read_u32()?;
        for _ in 0..count {
            let name = c.read_string()?;
            let longname = c.read_string()?;
            let (size, perms, mtime) = c.read_attrs()?;
            if name == "." || name == ".." {
                continue;
            }
            let is_dir = (perms & 0o170000) == 0o040000 || longname.starts_with('d');
            let full_path = if canonical == "/" {
                format!("/{name}")
            } else {
                format!("{}/{}", canonical.trim_end_matches('/'), name)
            };
            entries.push(SftpEntry {
                name,
                path: full_path,
                is_dir,
                size,
                modified: mtime,
                permissions: perms,
            });
        }
    }

    let _ = sftp.close_handle(&handle).await;

    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(SftpDirListing {
        path: canonical,
        entries,
    })
}

/// Download a remote file into ~/Downloads (and return the saved local path).
#[tauri::command]
pub async fn sftp_download(
    manager: State<'_, SessionManager>,
    session_id: String,
    remote_path: String,
) -> Result<String, String> {
    let mut sftp = open_sftp_channel(&manager, &session_id).await?;
    let req_id = sftp.alloc_id();
    let mut body = Vec::new();
    put_u32(&mut body, req_id);
    put_str(&mut body, &remote_path);
    put_u32(&mut body, SSH_FXF_READ);
    put_u32(&mut body, 0); // empty attrs
    sftp.send_packet(SSH_FXP_OPEN, &body).await?;

    let (t, payload) = sftp.recv_packet().await?;
    if t != SSH_FXP_HANDLE {
        return Err(format!("could not open remote file {remote_path}"));
    }
    let handle = {
        let mut c = Cursor::new(&payload);
        let _ = c.read_u32()?;
        c.read_bytes()?
    };

    let filename = remote_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("downloaded_file");
    let downloads = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let local_path = downloads.join(filename);
    let mut file =
        std::fs::File::create(&local_path).map_err(|e| format!("could not create local file: {e}"))?;

    let mut offset = 0u64;
    loop {
        let rid = sftp.alloc_id();
        let mut rbody = Vec::new();
        put_u32(&mut rbody, rid);
        put_bytes(&mut rbody, &handle);
        put_u64(&mut rbody, offset);
        put_u32(&mut rbody, 32768);
        sftp.send_packet(SSH_FXP_READ, &rbody).await?;

        let (rt, rpayload) = sftp.recv_packet().await?;
        if rt == SSH_FXP_STATUS {
            break;
        }
        if rt != SSH_FXP_DATA {
            break;
        }
        let mut c = Cursor::new(&rpayload);
        let _ = c.read_u32()?;
        let chunk = c.read_bytes()?;
        if chunk.is_empty() {
            break;
        }
        std::io::Write::write_all(&mut file, &chunk).map_err(|e| e.to_string())?;
        offset += chunk.len() as u64;
    }

    let _ = sftp.close_handle(&handle).await;
    Ok(local_path.to_string_lossy().into_owned())
}

/// Upload a file (given filename + raw bytes from frontend file picker / drag-drop)
/// into `remote_dir`.
#[tauri::command]
pub async fn sftp_upload_bytes(
    manager: State<'_, SessionManager>,
    session_id: String,
    remote_dir: String,
    file_name: String,
    data: Vec<u8>,
) -> Result<String, String> {
    let mut sftp = open_sftp_channel(&manager, &session_id).await?;
    let remote_path = if remote_dir == "/" {
        format!("/{file_name}")
    } else {
        format!("{}/{}", remote_dir.trim_end_matches('/'), file_name)
    };

    let req_id = sftp.alloc_id();
    let mut body = Vec::new();
    put_u32(&mut body, req_id);
    put_str(&mut body, &remote_path);
    put_u32(&mut body, SSH_FXF_WRITE | SSH_FXF_CREAT | SSH_FXF_TRUNC);
    put_u32(&mut body, 0); // empty attrs
    sftp.send_packet(SSH_FXP_OPEN, &body).await?;

    let (t, payload) = sftp.recv_packet().await?;
    if t != SSH_FXP_HANDLE {
        return Err(format!("permission denied or error opening {remote_path} for writing"));
    }
    let handle = {
        let mut c = Cursor::new(&payload);
        let _ = c.read_u32()?;
        c.read_bytes()?
    };

    let mut offset = 0u64;
    for chunk in data.chunks(32768) {
        let wid = sftp.alloc_id();
        let mut wbody = Vec::new();
        put_u32(&mut wbody, wid);
        put_bytes(&mut wbody, &handle);
        put_u64(&mut wbody, offset);
        put_bytes(&mut wbody, chunk);
        sftp.send_packet(SSH_FXP_WRITE, &wbody).await?;
        sftp.expect_status().await?;
        offset += chunk.len() as u64;
    }

    let _ = sftp.close_handle(&handle).await;
    Ok(remote_path)
}

#[tauri::command]
pub async fn sftp_mkdir(
    manager: State<'_, SessionManager>,
    session_id: String,
    path: String,
) -> Result<(), String> {
    let mut sftp = open_sftp_channel(&manager, &session_id).await?;
    let req_id = sftp.alloc_id();
    let mut body = Vec::new();
    put_u32(&mut body, req_id);
    put_str(&mut body, &path);
    put_u32(&mut body, 0); // default attrs
    sftp.send_packet(SSH_FXP_MKDIR, &body).await?;
    sftp.expect_status().await.map(|_| ())
}

#[tauri::command]
pub async fn sftp_delete(
    manager: State<'_, SessionManager>,
    session_id: String,
    path: String,
    is_dir: bool,
) -> Result<(), String> {
    let mut sftp = open_sftp_channel(&manager, &session_id).await?;
    let req_id = sftp.alloc_id();
    let mut body = Vec::new();
    put_u32(&mut body, req_id);
    put_str(&mut body, &path);
    let op = if is_dir { SSH_FXP_RMDIR } else { SSH_FXP_REMOVE };
    sftp.send_packet(op, &body).await?;
    sftp.expect_status().await.map(|_| ())
}
