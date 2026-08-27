use super::{Endpoint, Node, Result, Status, TsError, WhoIs, WhoIsUser};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use std::{collections::BTreeMap, io, net::SocketAddr};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    net::{TcpStream, UnixStream},
};

const HOST: &str = "local-tailscaled.sock";
const MAX_HEADER_BYTES: usize = 64 * 1024;

pub async fn get_status(endpoint: &Endpoint) -> Result<Status> {
    let response = get(endpoint, "/localapi/v0/status").await?;
    require_ok(response.status)?;
    parse_status(&response.body)
}

pub async fn get_whois(endpoint: &Endpoint, addr: SocketAddr) -> Result<WhoIs> {
    let addr = addr.to_string();
    let raw_path = format!("/localapi/v0/whois?addr={addr}");
    let response = get(endpoint, &raw_path).await?;
    if response.status == 200 {
        return parse_whois(&response.body);
    }

    let encoded_path = format!(
        "/localapi/v0/whois?addr={}",
        percent_encode_query_value(&addr)
    );
    let response = get(endpoint, &encoded_path).await?;
    require_ok(response.status)?;
    parse_whois(&response.body)
}

async fn get(endpoint: &Endpoint, path: &str) -> Result<Response> {
    match endpoint {
        Endpoint::Unix(socket_path) => {
            let stream = UnixStream::connect(socket_path).await?;
            exchange(stream, path, None).await
        }
        Endpoint::Loopback { port, token } => {
            let stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *port)).await?;
            exchange(stream, path, Some(token)).await
        }
    }
}

async fn exchange<S>(mut stream: S, path: &str, token: Option<&str>) -> Result<Response>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if path.contains(['\r', '\n']) || token.is_some_and(|token| token.contains(['\r', '\n'])) {
        return Err(invalid_data("invalid character in LocalAPI request").into());
    }

    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n");
    if let Some(token) = token {
        let credentials = STANDARD.encode(format!(":{token}"));
        request.push_str(&format!("Authorization: Basic {credentials}\r\n"));
    }
    request.push_str("\r\n");

    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    read_response(BufReader::new(stream)).await
}

async fn read_response<R>(mut reader: R) -> Result<Response>
where
    R: AsyncBufRead + Unpin,
{
    let status_line = read_line(&mut reader).await?;
    let status = parse_status_line(&status_line)?;
    let mut content_length = None;
    let mut chunked = false;
    let mut header_bytes = status_line.len();

    loop {
        let line = read_line(&mut reader).await?;
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(invalid_data("LocalAPI response headers are too large").into());
        }
        if line.is_empty() {
            break;
        }

        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid_data("malformed LocalAPI response header"))?;
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| invalid_data("invalid LocalAPI Content-Length"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"));
        }
    }

    let body = if chunked {
        read_chunked_body(&mut reader).await?
    } else if let Some(length) = content_length {
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await?;
        body
    } else {
        let mut body = Vec::new();
        reader.read_to_end(&mut body).await?;
        body
    };

    Ok(Response { status, body })
}

async fn read_chunked_body<R>(reader: &mut R) -> Result<Vec<u8>>
where
    R: AsyncBufRead + Unpin,
{
    let mut body = Vec::new();
    loop {
        let line = read_line(reader).await?;
        let size = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| invalid_data("invalid LocalAPI chunk size"))?;
        if size == 0 {
            loop {
                if read_line(reader).await?.is_empty() {
                    return Ok(body);
                }
            }
        }

        let old_len = body.len();
        let new_len = old_len
            .checked_add(size)
            .ok_or_else(|| invalid_data("LocalAPI response body is too large"))?;
        body.resize(new_len, 0);
        reader.read_exact(&mut body[old_len..]).await?;

        let mut terminator = [0; 2];
        reader.read_exact(&mut terminator).await?;
        if terminator != *b"\r\n" {
            return Err(invalid_data("malformed LocalAPI chunk terminator").into());
        }
    }
}

async fn read_line<R>(reader: &mut R) -> Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    let count = reader.read_until(b'\n', &mut bytes).await?;
    if count == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated HTTP response").into());
    }
    if bytes.len() > MAX_HEADER_BYTES {
        return Err(invalid_data("LocalAPI response line is too large").into());
    }
    if bytes.ends_with(b"\n") {
        bytes.pop();
    }
    if bytes.ends_with(b"\r") {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map_err(|_| invalid_data("LocalAPI response header is not valid UTF-8").into())
}

fn parse_status_line(line: &str) -> Result<u16> {
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    let status = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(invalid_data("invalid LocalAPI HTTP status line").into());
    }
    status
        .parse()
        .map_err(|_| invalid_data("invalid LocalAPI HTTP status code").into())
}

fn require_ok(status: u16) -> Result<()> {
    if status == 200 {
        Ok(())
    } else {
        Err(TsError::Http(status))
    }
}

fn parse_status(body: &[u8]) -> Result<Status> {
    let response: StatusResponse = serde_json::from_slice(body)?;
    Ok(Status {
        self_node: response.self_node,
        peers: response.peers.into_values().collect(),
    })
}

fn parse_whois(body: &[u8]) -> Result<WhoIs> {
    let response: WhoIsResponse = serde_json::from_slice(body)?;
    Ok(WhoIs {
        node_stable_id: response.node.stable_id,
        user: response.user_profile,
    })
}

fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct StatusResponse {
    #[serde(rename = "Self")]
    self_node: Node,
    #[serde(rename = "Peer")]
    peers: BTreeMap<String, Node>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WhoIsResponse {
    #[serde(rename = "Node")]
    node: WhoIsNode,
    #[serde(rename = "UserProfile")]
    user_profile: WhoIsUser,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WhoIsNode {
    #[serde(rename = "StableID")]
    stable_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt};

    const STATUS_JSON: &str = r#"
    {
      "Version": "1.102.0-tabcdef-g123456789",
      "TUN": true,
      "Self": {
        "ID": "nH2YfLhmWk11CNTRL",
        "PublicKey": "nodekey:0123456789abcdef",
        "HostName": "studio-mac",
        "DNSName": "studio-mac.example.ts.net.",
        "OS": "macOS",
        "UserID": 123456789,
        "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1234:5678"],
        "Online": true
      },
      "Peer": {
        "nodekey:fedcba9876543210": {
          "ID": "nX9KpqLmRs11CNTRL",
          "HostName": "fedora-box",
          "DNSName": "fedora-box.example.ts.net.",
          "OS": "linux",
          "UserID": 123456789,
          "TailscaleIPs": ["100.77.88.99"],
          "Online": true,
          "Active": true,
          "CurAddr": "192.168.1.25:41641",
          "Relay": "hel"
        },
        "nodekey:aaaaaaaaaaaaaaaa": {
          "ID": "nZ3Offline11CNTRL",
          "HostName": "old-laptop",
          "UserID": 123456789,
          "TailscaleIPs": ["100.64.0.8"],
          "Online": false,
          "CurAddr": "",
          "Relay": "fra"
        }
      },
      "CurrentTailnet": {"Name": "example.ts.net", "MagicDNSSuffix": "example.ts.net"}
    }
    "#;

    const WHOIS_JSON: &str = r#"
    {
      "Node": {
        "ID": 987654321,
        "StableID": "nX9KpqLmRs11CNTRL",
        "Name": "fedora-box.example.ts.net.",
        "User": 123456789,
        "Addresses": ["100.77.88.99/32"]
      },
      "UserProfile": {
        "ID": 123456789,
        "LoginName": "person@example.com",
        "DisplayName": "Example Person",
        "ProfilePicURL": "https://example.com/avatar.png"
      }
    }
    "#;

    #[test]
    fn parses_realistic_status_and_uses_string_ids() {
        let status = parse_status(STATUS_JSON.as_bytes()).expect("valid status response");

        assert_eq!(status.self_node.stable_id, "nH2YfLhmWk11CNTRL");
        assert_eq!(status.self_node.hostname, "studio-mac");
        assert_eq!(
            status.self_node.ips[0],
            "100.101.102.103".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(status.peers.len(), 2);
        let direct = status
            .peers
            .iter()
            .find(|peer| peer.stable_id == "nX9KpqLmRs11CNTRL")
            .expect("direct peer");
        assert_eq!(direct.cur_addr, "192.168.1.25:41641");
        assert_eq!(direct.relay, "hel");
    }

    #[test]
    fn parses_whois_stable_id_instead_of_numeric_node_id() {
        let whois = parse_whois(WHOIS_JSON.as_bytes()).expect("valid WhoIs response");

        assert_eq!(whois.node_stable_id, "nX9KpqLmRs11CNTRL");
        assert_eq!(whois.user.id, 123456789);
        assert_eq!(whois.user.login_name, "person@example.com");
    }

    #[test]
    fn percent_encodes_ipv4_and_ipv6_socket_addresses() {
        assert_eq!(
            percent_encode_query_value("100.77.88.99:41717"),
            "100.77.88.99%3A41717"
        );
        assert_eq!(
            percent_encode_query_value("[fd7a:115c:a1e0::1]:41717"),
            "%5Bfd7a%3A115c%3Aa1e0%3A%3A1%5D%3A41717"
        );
    }

    #[tokio::test]
    async fn reads_content_length_chunked_and_eof_bodies() {
        let content_length =
            read_test_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello trailing").await;
        assert_eq!(content_length.body, b"hello");

        let chunked = read_test_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
              4\r\nWiki\r\n5;extension=yes\r\npedia\r\n0\r\nTrailer: value\r\n\r\n",
        )
        .await;
        assert_eq!(chunked.body, b"Wikipedia");

        let eof = read_test_response(b"HTTP/1.0 200 OK\r\n\r\nto eof").await;
        assert_eq!(eof.body, b"to eof");
    }

    async fn read_test_response(bytes: &'static [u8]) -> Response {
        let (reader, mut writer) = duplex(1024);
        let write = tokio::spawn(async move {
            writer.write_all(bytes).await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let response = read_response(BufReader::new(reader)).await.unwrap();
        write.await.unwrap();
        response
    }
}
