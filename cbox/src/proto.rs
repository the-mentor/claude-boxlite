//! Frame codec for the per-box control socket.
//!
//! Length-prefixed: u8 tag, u32 big-endian length, payload. Control frames are
//! JSON; data frames are raw bytes. Deliberately not bincode, which the
//! dependency audit flagged as unmaintained.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Refuse absurd frames rather than allocating on a corrupt length.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Sentinel `Frame::Exit` code the server sends when a session ends *without*
/// ever reaching a real guest exit status (e.g. `exec` setup failed, or the
/// connection was torn down before `exec.wait()`). Not a genuine process exit
/// code — those come from `boxlite`'s `ExecResult::code()`, reflecting an
/// actual guest exit. Distinguishing this from a bare, silent stream close is
/// the point: without an explicit `Exit` frame, the client reads a closed
/// connection as clean EOF and reports the whole session as exit code 0.
///
/// Deliberately `i32::MIN`, not `-1`: `boxlite-0.9.7`'s
/// `litebox/exec.rs::map_wait_response` computes `code = -resp.signal` when
/// the guest was killed by a signal, so a guest killed by SIGHUP (signal 1)
/// legitimately returns exactly `-1` -- and `rest/litebox.rs` separately uses
/// `-1` as its own "unknown" placeholder. Either would collide with `-1` here
/// and print the "session failed" explanation for a real result. No signal
/// number or POSIX exit code can ever reach `i32::MIN`.
pub const EXIT_CODE_SESSION_FAILED: i32 = i32::MIN;

const TAG_EXEC: u8 = 0;
const TAG_STDIN: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_STDOUT: u8 = 3;
const TAG_EXIT: u8 = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Exec(ExecRequest),
    Stdin(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Stdout(Vec<u8>),
    Exit { code: i32 },
}

#[derive(Serialize, Deserialize)]
struct ResizePayload {
    rows: u16,
    cols: u16,
}

#[derive(Serialize, Deserialize)]
struct ExitPayload {
    code: i32,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, f: &Frame) -> Result<()> {
    let (tag, payload): (u8, Vec<u8>) = match f {
        Frame::Exec(req) => (TAG_EXEC, serde_json::to_vec(req)?),
        Frame::Stdin(b) => (TAG_STDIN, b.clone()),
        Frame::Resize { rows, cols } => (
            TAG_RESIZE,
            serde_json::to_vec(&ResizePayload { rows: *rows, cols: *cols })?,
        ),
        Frame::Stdout(b) => (TAG_STDOUT, b.clone()),
        Frame::Exit { code } => (TAG_EXIT, serde_json::to_vec(&ExitPayload { code: *code })?),
    };

    if payload.len() > MAX_FRAME {
        bail!("frame of {} bytes exceeds MAX_FRAME", payload.len());
    }

    w.write_all(&[tag]).await?;
    w.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    w.write_all(&payload).await?;
    w.flush().await?;
    Ok(())
}

/// `Ok(None)` means a clean EOF at a frame boundary. A partial frame is an
/// error — silently treating truncation as EOF would hide a dropped peer.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>> {
    let mut tag = [0u8; 1];
    match r.read_exact(&mut tag).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME {
        bail!("frame length {len} exceeds MAX_FRAME");
    }

    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;

    let frame = match tag[0] {
        TAG_EXEC => Frame::Exec(serde_json::from_slice(&payload)?),
        TAG_STDIN => Frame::Stdin(payload),
        TAG_RESIZE => {
            let p: ResizePayload = serde_json::from_slice(&payload)?;
            Frame::Resize { rows: p.rows, cols: p.cols }
        }
        TAG_STDOUT => Frame::Stdout(payload),
        TAG_EXIT => {
            let p: ExitPayload = serde_json::from_slice(&payload)?;
            Frame::Exit { code: p.code }
        }
        other => bail!("unknown frame tag {other}"),
    };
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(f: Frame) -> Frame {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &f).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        read_frame(&mut cursor).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn every_frame_survives_a_roundtrip() {
        let exec = Frame::Exec(ExecRequest {
            cmd: vec!["claude".into(), "--continue".into()],
            env: vec![
                ("TERM_PROGRAM".into(), "WezTerm".into()),
                ("HOME".into(), "/home/user".into()),
                ("SPECIAL".into(), "value with \"quotes\" and \\backslash".into()),
            ],
            rows: 40,
            cols: 120,
        });
        assert_eq!(roundtrip(exec.clone()).await, exec);

        let stdin = Frame::Stdin(vec![0x01, 0x02, 0xff]);
        assert_eq!(roundtrip(stdin.clone()).await, stdin);

        let resize = Frame::Resize { rows: 24, cols: 80 };
        assert_eq!(roundtrip(resize.clone()).await, resize);

        let stdout = Frame::Stdout(b"hello".to_vec());
        assert_eq!(roundtrip(stdout.clone()).await, stdout);

        let exit = Frame::Exit { code: 130 };
        assert_eq!(roundtrip(exit.clone()).await, exit);
    }

    #[tokio::test]
    async fn binary_data_is_not_mangled() {
        // Raw bytes must not go through JSON or UTF-8 validation.
        let payload: Vec<u8> = (0u8..=255).collect();
        let f = Frame::Stdout(payload.clone());
        assert_eq!(roundtrip(f).await, Frame::Stdout(payload));
    }

    #[tokio::test]
    async fn clean_eof_returns_none_rather_than_an_error() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_frame(&mut empty).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error_not_a_silent_none() {
        // Tag + length claiming 10 bytes, but only 2 present.
        let mut buf = vec![TAG_STDOUT];
        buf.extend_from_slice(&10u32.to_be_bytes());
        buf.extend_from_slice(&[1, 2]);
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }

    #[tokio::test]
    async fn eof_after_tag_but_before_length_is_an_error() {
        // Tag byte present, but only 2 of 4 length bytes follow.
        let buf = vec![TAG_STDOUT, 0x00, 0x00];
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }

    #[tokio::test]
    async fn an_oversized_length_is_rejected_before_allocating() {
        // Use a length near u32::MAX so misplaced checks would allocate before failing.
        let mut buf = vec![TAG_STDOUT];
        buf.extend_from_slice(&(u32::MAX - 1000).to_be_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }

    #[tokio::test]
    async fn an_unknown_tag_is_rejected() {
        let mut buf = vec![99u8];
        buf.extend_from_slice(&0u32.to_be_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }

    #[tokio::test]
    async fn frames_stream_back_to_back() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Frame::Stdout(b"a".to_vec())).await.unwrap();
        write_frame(&mut buf, &Frame::Stdout(b"b".to_vec())).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).await.unwrap().unwrap(), Frame::Stdout(b"a".to_vec()));
        assert_eq!(read_frame(&mut cursor).await.unwrap().unwrap(), Frame::Stdout(b"b".to_vec()));
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }
}
