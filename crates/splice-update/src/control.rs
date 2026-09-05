use crate::{Host, UpdateState};
use anyhow::{ensure, Context, Result};
use futures::future::BoxFuture;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use splice_proto::MachineId;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket};

pub const PORT: u16 = 41718;
pub const PROTOCOL: u16 = 1;
const MAX_MESSAGE: u32 = 65536;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Action {
    Status,
    Check,
    Prepare { version: String },
    Install { version: String },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol: u16,
    action: Action,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    protocol: u16,
    machine: MachineId,
    result: Result<UpdateState, String>,
}

pub type Authorize = Arc<dyn Fn(SocketAddr) -> BoxFuture<'static, bool> + Send + Sync>;

async fn read<T: DeserializeOwned>(reader: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    let size = reader.read_u32().await?;
    ensure!(
        size > 0 && size <= MAX_MESSAGE,
        "invalid update control message size"
    );
    let mut bytes = vec![0; size as usize];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn write(writer: &mut (impl AsyncWrite + Unpin), value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_MESSAGE as usize,
        "update control response is too large"
    );
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    Ok(())
}

pub async fn serve(
    listener: TcpListener,
    machine: MachineId,
    host: Host,
    authorize: Authorize,
) -> Result<()> {
    let limit = Arc::new(tokio::sync::Semaphore::new(16));
    loop {
        let (mut stream, address) = listener.accept().await?;
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let (host, authorize, machine) = (host.clone(), authorize.clone(), machine.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let operation = async {
                ensure!(
                    authorize(address).await,
                    "update client is not an authorized Tailnet peer"
                );
                let request: Request = read(&mut stream).await?;
                let result = if request.protocol == PROTOCOL {
                    host.request(request.action).map_err(|e| format!("{e:#}"))
                } else {
                    Err(format!("Update control protocol {PROTOCOL} is required"))
                };
                write(
                    &mut stream,
                    &Response {
                        protocol: PROTOCOL,
                        machine,
                        result,
                    },
                )
                .await
            };
            match tokio::time::timeout(Duration::from_secs(5), operation).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::debug!(%address, %error, "update control request failed")
                }
                Err(_) => tracing::debug!(%address, "update control request timed out"),
            }
        });
    }
}

pub async fn request(
    local: IpAddr,
    remote: SocketAddr,
    machine: &MachineId,
    action: Action,
) -> Result<UpdateState> {
    tokio::time::timeout(Duration::from_secs(6), async {
        let socket = if remote.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        socket.bind(SocketAddr::new(local, 0))?;
        let mut stream = socket.connect(remote).await?;
        write(
            &mut stream,
            &Request {
                protocol: PROTOCOL,
                action,
            },
        )
        .await?;
        let response: Response = read(&mut stream).await?;
        ensure!(
            response.protocol == PROTOCOL && response.machine == *machine,
            "update server identity or protocol does not match"
        );
        response.result.map_err(anyhow::Error::msg)
    })
    .await
    .context("update control request timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn server(
        allowed: bool,
    ) -> (
        std::net::SocketAddr,
        Host,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let host = Host::new(directory.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_host = host.clone();
        let server = tokio::spawn(async move {
            serve(
                listener,
                MachineId("server".into()),
                server_host,
                Arc::new(move |_| Box::pin(async move { allowed })),
            )
            .await
            .unwrap();
        });
        (address, host, server, directory)
    }

    #[tokio::test]
    async fn denied_tailnet_peer_cannot_read_status_or_start_an_update() {
        let (address, host, server, _directory) = server(false).await;
        let before = host.state().borrow().clone();
        for action in [
            Action::Status,
            Action::Check,
            Action::Install {
                version: "9.0.0".into(),
            },
        ] {
            assert!(
                request(address.ip(), address, &MachineId("server".into()), action)
                    .await
                    .is_err()
            );
            assert_eq!(*host.state().borrow(), before);
        }
        server.abort();
    }

    #[tokio::test]
    async fn control_identity_is_verified_independently_of_kvm_protocol() {
        let (address, host, server, _directory) = server(true).await;
        let expected = host.state().borrow().clone();
        let response = request(
            address.ip(),
            address,
            &MachineId("server".into()),
            Action::Status,
        )
        .await
        .unwrap();
        assert_eq!(response, expected);
        assert!(request(
            address.ip(),
            address,
            &MachineId("impostor".into()),
            Action::Status
        )
        .await
        .is_err());
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        write(
            &mut stream,
            &Request {
                protocol: PROTOCOL + 1,
                action: Action::Check,
            },
        )
        .await
        .unwrap();
        let response: Response = read(&mut stream).await.unwrap();
        assert!(response.result.unwrap_err().contains("protocol"));
        assert_eq!(*host.state().borrow(), expected);
        server.abort();
    }

    #[tokio::test]
    async fn rejects_oversized_messages_before_allocating_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(8);
        writer.write_u32(MAX_MESSAGE + 1).await.unwrap();
        assert!(read::<Request>(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn truncated_request_never_becomes_an_update_command() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_u32(20).await.unwrap();
        writer.write_all(b"{\"").await.unwrap();
        drop(writer);
        assert!(read::<Request>(&mut reader).await.is_err());
    }
}
