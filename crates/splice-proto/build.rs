use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SPLICE_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SPLICE_BUILD_DIRTY");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../../.git/refs");
    let commit = match std::env::var("SPLICE_BUILD_COMMIT") {
        Ok(value) => value,
        Err(_) => {
            let output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("git is required; set SPLICE_BUILD_COMMIT when building a source archive");
            assert!(
                output.status.success(),
                "set SPLICE_BUILD_COMMIT when building outside a Git checkout"
            );
            String::from_utf8(output.stdout)
                .expect("Git commit must be UTF-8")
                .trim()
                .to_string()
        }
    };
    assert!(
        commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "SPLICE_BUILD_COMMIT must be a full Git commit"
    );
    let dirty = match std::env::var("SPLICE_BUILD_DIRTY") {
        Ok(value) => value
            .parse::<bool>()
            .expect("SPLICE_BUILD_DIRTY must be true or false"),
        Err(_) => {
            let output = Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .expect("git is required; set SPLICE_BUILD_DIRTY for a source archive");
            assert!(
                output.status.success(),
                "set SPLICE_BUILD_DIRTY when building outside a Git checkout"
            );
            !output.stdout.is_empty()
        }
    };
    if let Ok(output) = Command::new("git").args(["ls-files", "../.."]).output() {
        if output.status.success() {
            for path in String::from_utf8(output.stdout)
                .expect("Git paths must be UTF-8")
                .lines()
            {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    println!("cargo:rustc-env=SPLICE_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=SPLICE_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=SPLICE_BUILD_TARGET={}",
        std::env::var("TARGET").expect("Cargo sets TARGET")
    );
}
