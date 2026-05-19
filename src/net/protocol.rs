use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Message {
    Hello {
        username: String,
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_id: Option<String>,
    },
    Text {
        body: String,
    },
    Bye,
}

impl Message {
    pub fn hello(username: impl Into<String>) -> Self {
        Self::Hello {
            username: username.into(),
            version: PROTOCOL_VERSION.to_string(),
            peer_id: None,
        }
    }

    pub fn hello_with_id(username: impl Into<String>, peer_id: impl Into<String>) -> Self {
        Self::Hello {
            username: username.into(),
            version: PROTOCOL_VERSION.to_string(),
            peer_id: Some(peer_id.into()),
        }
    }

    pub fn text(body: impl Into<String>) -> Self {
        Self::Text { body: body.into() }
    }
}

pub async fn write_frame(
    w: &mut OwnedWriteHalf,
    msg: &Message,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

pub async fn read_frame(
    r: &mut BufReader<OwnedReadHalf>,
) -> std::io::Result<Option<Message>> {
    let mut buf = String::new();
    let n = r.read_line(&mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    let msg = serde_json::from_str::<Message>(buf.trim_end())
        .map_err(std::io::Error::other)?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip() {
        let m = Message::hello("alice");
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"kind\":\"hello\""));
        assert!(s.contains("\"username\":\"alice\""));
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn text_roundtrip() {
        let m = Message::text("hi there");
        let back: Message = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn bye_roundtrip() {
        let m = Message::Bye;
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, "{\"kind\":\"bye\"}");
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
    }
}
