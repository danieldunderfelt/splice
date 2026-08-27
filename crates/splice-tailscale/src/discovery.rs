use super::{Endpoint, Result, TsError};
use std::path::{Path, PathBuf};

const SAME_USER_PROOF_PREFIX: &str = "sameuserproof-";

pub async fn discover_endpoint() -> Result<Endpoint> {
    #[cfg(target_os = "linux")]
    {
        return discover_linux().await;
    }

    #[cfg(target_os = "macos")]
    {
        return discover_macos().await;
    }

    #[allow(unreachable_code)]
    Err(TsError::Unreachable(
        "LocalAPI endpoint discovery is supported only on Linux and macOS".into(),
    ))
}

#[cfg(target_os = "linux")]
async fn discover_linux() -> Result<Endpoint> {
    const SOCKETS: [&str; 2] = [
        "/var/run/tailscale/tailscaled.sock",
        "/run/tailscale/tailscaled.sock",
    ];

    for socket in SOCKETS {
        if tokio::fs::metadata(socket).await.is_ok() {
            return Ok(Endpoint::Unix(PathBuf::from(socket)));
        }
    }

    Err(TsError::Unreachable(format!(
        "Tailscale not running? No LocalAPI socket found at {} or {}",
        SOCKETS[0], SOCKETS[1]
    )))
}

#[cfg(target_os = "macos")]
async fn discover_macos() -> Result<Endpoint> {
    if let Some(endpoint) = discover_group_container().await {
        return Ok(endpoint);
    }

    if let Some(endpoint) = discover_standalone().await {
        return Ok(endpoint);
    }

    let socket = PathBuf::from("/var/run/tailscaled.sock");
    if tokio::fs::metadata(&socket).await.is_ok() {
        return Ok(Endpoint::Unix(socket));
    }

    Err(TsError::Unreachable(
        "Tailscale not running? No macOS LocalAPI credentials or /var/run/tailscaled.sock found"
            .into(),
    ))
}

#[cfg(target_os = "macos")]
async fn discover_group_container() -> Option<Endpoint> {
    let home = std::env::var_os("HOME")?;
    let containers = PathBuf::from(home).join("Library").join("Group Containers");
    let mut container_entries = tokio::fs::read_dir(containers).await.ok()?;
    let mut proof_paths = Vec::new();

    while let Ok(Some(entry)) = container_entries.next_entry().await {
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".io.tailscale.ipn.macos") {
            continue;
        }

        let mut entries = match tokio::fs::read_dir(entry.path()).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        while let Ok(Some(proof)) = entries.next_entry().await {
            if proof
                .file_name()
                .to_string_lossy()
                .starts_with(SAME_USER_PROOF_PREFIX)
            {
                proof_paths.push(proof.path());
            }
        }
    }

    proof_paths.sort();
    proof_paths
        .iter()
        .find_map(|path| parse_sameuserproof_path(path))
}

#[cfg(target_os = "macos")]
async fn discover_standalone() -> Option<Endpoint> {
    let root = Path::new("/Library/Tailscale");
    let port_path = root.join("ipnport");
    let port = read_port(&port_path).await?;
    let token_path = root.join(format!("{SAME_USER_PROOF_PREFIX}{port}"));
    let token = tokio::fs::read_to_string(token_path).await.ok()?;
    let token = token.trim().to_owned();
    (!token.is_empty()).then_some(Endpoint::Loopback { port, token })
}

#[cfg(target_os = "macos")]
async fn read_port(path: &Path) -> Option<u16> {
    if let Ok(target) = tokio::fs::read_link(path).await {
        if let Some(port) = target.to_str().and_then(parse_port_text) {
            return Some(port);
        }
        if let Some(port) = target
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_port_text)
        {
            return Some(port);
        }
    }

    let contents = tokio::fs::read_to_string(path).await.ok()?;
    parse_port_text(&contents)
}

fn parse_sameuserproof_path(path: &Path) -> Option<Endpoint> {
    let filename = path.file_name()?.to_str()?;
    let value = filename.strip_prefix(SAME_USER_PROOF_PREFIX)?;
    let (port, token) = value.split_once('-')?;
    let port = parse_port_text(port)?;
    if token.is_empty() {
        return None;
    }

    Some(Endpoint::Loopback {
        port,
        token: token.to_owned(),
    })
}

fn parse_port_text(value: &str) -> Option<u16> {
    let value = value.trim();
    let port = value.parse::<u16>().ok()?;
    (port != 0).then_some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_group_container_proof_filename() {
        let endpoint = parse_sameuserproof_path(Path::new(
            "/Users/test/Library/Group Containers/group.io.tailscale.ipn.macos/\
             sameuserproof-41112-a-token-with-dashes",
        ))
        .expect("valid proof filename");

        match endpoint {
            Endpoint::Loopback { port, token } => {
                assert_eq!(port, 41112);
                assert_eq!(token, "a-token-with-dashes");
            }
            Endpoint::Unix(_) => panic!("expected loopback endpoint"),
        }
    }

    #[test]
    fn rejects_malformed_proof_filenames() {
        for filename in [
            "sameuserproof-0-token",
            "sameuserproof-70000-token",
            "sameuserproof-41112-",
            "sameuserproof-notaport-token",
            "other-41112-token",
        ] {
            assert!(parse_sameuserproof_path(Path::new(filename)).is_none());
        }
    }

    #[test]
    fn parses_port_file_and_link_text() {
        assert_eq!(parse_port_text("41112\n"), Some(41112));
        assert_eq!(parse_port_text("/Library/Tailscale/41112"), None);
        assert_eq!(parse_port_text("0"), None);
        assert_eq!(parse_port_text("65536"), None);
    }
}
