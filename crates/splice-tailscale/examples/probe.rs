use splice_tailscale::Client;
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::discover().await?;
    let status = client.status().await?;
    println!(
        "self: {} ({})",
        status.self_node.hostname, status.self_node.stable_id
    );
    println!("peers: {}", status.peers.len());

    let ip = status
        .self_node
        .ips
        .iter()
        .copied()
        .find(std::net::IpAddr::is_ipv4)
        .or_else(|| status.self_node.ips.first().copied())
        .ok_or_else(|| anyhow::anyhow!("tailscaled status returned no self Tailscale IP"))?;
    let addr = SocketAddr::new(ip, 12345);
    let whois = client.whois(addr).await?;
    println!(
        "whois {addr}: stable_id={}, user={} ({})",
        whois.node_stable_id, whois.user.login_name, whois.user.id
    );

    Ok(())
}
