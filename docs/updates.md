# Update Splice across your tailnet

Splice can update itself and your other authorized Tailnet computers from the **Updates** panel.
Every computer needs a manual installation of Splice 1.1.0 or later before remote updates work.

## Check and install

1. Open **Updates** in the Splice window.
2. Select **Check for updates** for the computer you want to update.
3. Select **Download VERSION** when a newer release is available.
4. Wait until Splice reports that the release is verified and ready.
5. Select **Install and restart this computer's Splice**.
6. Wait for the computer to return with the new installed version.

Splice downloads nothing until you request it. Installing restarts Splice on the selected computer
and briefly disconnects its KVM sessions. Update each computer to the same release.
If a release changes the KVM protocol, mixed versions cannot share input or clipboard data during
that rollout. The independent update connection remains available.

## Supported installations

| Installation | Update method |
|---|---|
| Linux at `~/.local/bin/splice`, owned by your user and running as `app-splice.service` | Updates panel |
| Developer ID signed macOS app bundle, owned by your user in a writable directory | Updates panel |
| Distribution package, Flatpak, another binary path, or an unsigned development app | Installer or package manager; the panel explains the requirement |

Remote update requests use TCP port **41718** on the Tailscale interface. KVM uses TCP **41717**.
Allow both through applicable Tailnet policies and local firewall or VPN rules. Update authorization
uses Tailscale's local `whois` identity and Splice's same-user policy. Another user's Tailnet node
cannot request status or an update. The peer can select a release version, but cannot supply a URL,
file path, or shell command.

## Failed updates

A failed download or signature check leaves the current installation running and reports the error.
Splice checks the staged binary's version, commit, target, and protocol against the signed manifest.
macOS also checks the Developer ID team, bundle signature, and Gatekeeper assessment.

The app exits only after an independent helper acknowledges the installation attempt. That helper
retains the old executable, replaces the installation atomically, starts it, and waits up to 60 seconds
for startup confirmation. Confirmation requires the new version and commit to start the core network
and update listeners. It does not test physical input permissions or every desktop backend.

If the new process fails this check, the helper stops it, restores the previous executable and saved
configuration, and restarts the old version. Splice reports that restoration as an update failure.
If stopping or restoring fails, it retains the recovery files and reports that error.
An interrupted update is reported on the next startup.

Use **Check for updates** to retry a failed attempt. The update status and last error are also included
in [diagnostic reports](diagnostics.md). Update state lives under `updates` in Splice's configuration
directory. Retained `.splice-update-*` directories beside the installation contain recovery files.
