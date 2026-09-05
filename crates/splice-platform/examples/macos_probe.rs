//! Read-only probe of the macOS platform preconditions.
//!
//! Runs fine WITHOUT any permission granted — it prints `false` and exits 0, which is what
//! makes it usable as a smoke test in CI and right after a fresh checkout.
//!
//! ```text
//! cargo run -p splice-platform --example macos_probe
//! ```

#[cfg(target_os = "macos")]
fn main() {
    use splice_platform::macos;

    let post = unsafe { macos::ffi::CGPreflightPostEventAccess() };
    let listen = unsafe { macos::ffi::CGPreflightListenEventAccess() };
    // No prompt: the probe must never pop a system dialog.
    let trusted = macos::ax_trusted(false);

    println!("permissions");
    println!("  post event access (Accessibility): {post}");
    println!("  listen event access (Input Monitoring): {listen}");
    println!("  AXIsProcessTrusted: {trusted}");
    if !(post && listen && trusted) {
        println!("  → grant: System Settings › Privacy & Security › Accessibility");
    }

    println!("displays (CG global points, y-down)");
    for d in macos::displays::snapshot() {
        println!(
            "  {:>10}  {:>6},{:<6} {:>5}x{:<5} scale {:.2}",
            d.id, d.x, d.y, d.w, d.h, d.scale
        );
    }

    println!("secure input");
    match macos::secure_input_status() {
        Some(msg) => println!("  {msg}"),
        None => println!("  off"),
    }

    println!("scroll");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macos_probe only does anything on macOS");
}
