# Tailscale integration facts (live-verified on this tailnet, 2026-08)

Authoritative reference for `crates/splice-tailscale`. All verified against Tailscale 1.102 on
macOS (Mac App Store variant) and Linux (tailscaled).

## LocalAPI access

- **Linux**: unix socket `/var/run/tailscale/tailscaled.sock` (mode 0666 — read endpoints need
  no privileges, no auth header). HTTP/1.1, Host header `local-tailscaled.sock`, paths under
  `/localapi/v0/...`.
- **macOS (App Store / standalone GUI variants)**: NO unix socket. Loopback TCP + HTTP Basic
  auth; discover port+token by globbing
  `~/Library/Group Containers/*.io.tailscale.ipn.macos/sameuserproof-<port>-<token>`
  (filename = `sameuserproof-{port}-{token}`). Request: `http://127.0.0.1:{port}/localapi/v0/...`
  with Basic auth, EMPTY username, token as PASSWORD. Standalone variant fallback:
  `/Library/Tailscale/ipnport` symlink → port, `/Library/Tailscale/sameuserproof-{port}` file
  contains token. Try group-container glob first, then /Library/Tailscale, then unix socket
  (open-source tailscaled on macOS).
- `tailscale debug local-creds` prints a working curl line (debugging aid).

## Endpoints used

- `GET /localapi/v0/status` → JSON. Fields we consume:
  - `Self`: `{ID (StableID), HostName, DNSName, OS, UserID, TailscaleIPs, Online}`
  - `Peer`: map keyed by node public key → same shape plus `CurAddr`, `Relay`, `LastSeen`,
    `Online`, `Active`.
  - `CurrentTailnet`, `MagicDNSSuffix`.
  Poll every 15 s + on demand. Treat `Online` as a hint (control-plane claim); confirm by
  connecting. `Relay` is ALWAYS populated (home DERP region) even for direct peers — the
  direct-vs-DERP check is `CurAddr != ""` (direct) vs empty (DERP/idle).
- `GET /localapi/v0/whois?addr=IP:PORT` → `{Node: {ID, StableID, Name, User, ...},
  UserProfile: {ID, LoginName, DisplayName}}`. **Pass ip:port, not bare IP.**
  Returns 404/error for non-tailnet sources (fails closed — good).
  **CRITICAL: WhoIs resolves SELF too** — any local process dialing our own 100.x returns a
  valid answer with our own user. Auth check = WhoIs succeeds AND `Node.StableID != self.ID`
  AND `UserProfile.ID == self UserID`.

## Rules

- `MachineId` = `StableID` (e.g. "nxVs7xinLs11CNTRL") — stable across IP changes.
- Use `TailscaleIPs[0]` (100.x IPv4) for dialing. **Never MagicDNS** (FQDN resolution broken
  on macOS MAS variant through the OS resolver).
- Bind the Splice listener to the Tailscale IP ONLY (never 0.0.0.0 — ACLs don't apply to
  localhost/LAN sources).
- Splice port: TCP 41717.
- TCP_NODELAY on every connection (Nagle + delayed ACK = tens of ms on small writes).
- First packets after idle may route via DERP while the direct path re-establishes — the
  1 s active heartbeat doubles as path keep-warm.
- Latency reality check (measured): 500 Hz small-payload round trips over tailnet on LAN =
  ~5 ms p50 (≈ raw LAN + 0.1–0.3 ms). WiFi tail spikes (100–380 ms) dominate — not Tailscale.
- Surface `CurAddr == ""` (DERP) in the UI as a latency warning.
- No embedded tsnet — always the host's tailscaled. If LocalAPI is unreachable: clear UI
  error ("Tailscale not running?"), retry with backoff.
- MTU on tailscale interfaces is 1280 — irrelevant for our TCP stream; do not use QUIC.

## Implementation notes

- Hand-roll the HTTP client: one-shot HTTP/1.1 GET over `tokio::net::UnixStream` /
  `TcpStream`, `Authorization: Basic base64(":"+token)` on macOS, parse with serde_json.
  No hyper dependency needed. Handle chunked responses defensively (LocalAPI generally
  returns Content-Length; support both).
- Status JSON is "subject to change" — deserialize with `#[serde(default)]` everywhere,
  unknown fields ignored, and treat missing fields as absent-not-error.
