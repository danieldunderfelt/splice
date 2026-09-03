Name:           splice
Version:        1.0.0
Release:        1%{?dist}
Summary:        Software KVM for Tailscale networks

License:        MIT
URL:            https://github.com/danieldunderfelt/splice
# Both produced by packaging/rpm/build.sh: a git archive and a cargo vendor tarball.
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  systemd-rpm-macros
Requires:       libwayland-client
Requires:       libxkbcommon
Recommends:     tailscale
Recommends:     vulkan-loader
Recommends:     (xdg-desktop-portal-gnome or xdg-desktop-portal-kde)

%description
Splice shares one keyboard and mouse between the computers on your Tailscale
network. Move the pointer through a screen edge and the keyboard follows.
Clipboard contents are shared as well. On Linux it uses the Wayland Input
Capture and Remote Desktop portals, so it needs GNOME or KDE Plasma.

%prep
%autosetup -n %{name}-%{version}
tar -xf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<CFG
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
CFG

%build
cargo build --release --locked --offline -p splice-app

%install
install -Dpm0755 target/release/splice %{buildroot}%{_bindir}/splice
install -Dpm0644 packaging/linux/io.github.danieldunderfelt.Splice.desktop \
    %{buildroot}%{_datadir}/applications/io.github.danieldunderfelt.Splice.desktop
install -Dpm0644 packaging/linux/io.github.danieldunderfelt.Splice.metainfo.xml \
    %{buildroot}%{_metainfodir}/io.github.danieldunderfelt.Splice.metainfo.xml
install -Dpm0644 packaging/linux/app-splice.service \
    %{buildroot}%{_userunitdir}/app-splice.service
install -Dpm0644 packaging/linux/70-splice.rules \
    %{buildroot}%{_udevrulesdir}/70-splice.rules

%post
%udev_rules_update

%postun
%udev_rules_update

%files
%license LICENSE
%doc README.md docs/linux-setup.md
%{_bindir}/splice
%{_datadir}/applications/io.github.danieldunderfelt.Splice.desktop
%{_metainfodir}/io.github.danieldunderfelt.Splice.metainfo.xml
%{_userunitdir}/app-splice.service
%{_udevrulesdir}/70-splice.rules

%changelog
* Thu Sep 03 2026 Daniel Dunderfelt <dev@developsuperpowers.com> - 1.0.0-1
- Initial package
