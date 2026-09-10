# Packaging the Linux file shelf

Splice ships one executable. The GTK4 shelf runs in a separate `splice file-shelf` process, started only when the user requests the file shelf. The service starts `/proc/self/exe` so an existing service launches the same executable image even during an atomic on-disk replacement.

`splice files` is the public command that asks the running service to open the shelf. The internal `file-shelf` mode connects to that service's checked local socket. The `splice-files` crate also contains a standalone development executable and an opt-in pilot; neither is installed or required by production packages.

The ordinary installer, signed release archive and updater continue to install and atomically replace one `splice` executable. Their existing archive and rollback format is unchanged. No two-executable transaction or manual migration is needed for the shelf.

Linux builds now need GTK4 version 4.10 or newer and FUSE3 development libraries. Host packages add the corresponding runtime dependencies. Build and install normally:

```sh
cargo build -p splice-app --release --locked
packaging/linux/install.sh
```

Debian packages use cargo-deb's `$auto` dependency detection and add `fuse3` for the executed mount utility. This selects the library package matching the built executable rather than hardcoding a FUSE soname. See [cargo-deb dependency configuration](https://github.com/kornelski/cargo-deb#packagemetadatadeb-options) and [Debian's current FUSE library package](https://packages.debian.org/trixie/libfuse3-4).

The Flatpak manifest now uses GNOME 50 to supply GTK4. Its [runtime dependencies](https://github.com/GNOME/gnome-build-meta/blob/gnome-50/elements/sdk-platform.bst) and [Freedesktop SDK base](https://github.com/GNOME/gnome-build-meta/blob/gnome-50/elements/freedesktop-sdk.bst) retain compatibility with the Rust SDK extension. Deferred FUSE drags in the sandbox are not supported by the current package configuration and need separate qualification.

Mac bundles keep their existing build and install path. Native drag acceptance, real package installation and real updater operations are not exercised in this session.
