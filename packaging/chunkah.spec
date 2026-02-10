%global crate chunkah

Name:           chunkah
Version:        0.1.1
Release:        %autorelease
Summary:        OCI building tool for content-based container image layers

# chunkah itself is MIT OR Apache-2.0
# LICENSE.dependencies contains full breakdown of vendored crates
License:        MIT OR Apache-2.0
URL:            https://github.com/jlebon/chunkah
Source0:        %{url}/releases/download/v%{version}/%{crate}-%{version}.tar.gz
Source1:        %{url}/releases/download/v%{version}/%{crate}-%{version}-vendor.tar.gz

BuildRequires:  cargo-rpm-macros >= 26
BuildRequires:  openssl-devel
BuildRequires:  zlib-devel

%description
chunkah is an OCI building tool that takes a flat rootfs and outputs a
layered OCI image with content-based layers. It optimizes container image
layer reuse by grouping files based on their content (e.g., by RPM package)
rather than by Dockerfile instruction order.

It is a generalized successor to rpm-ostree's build-chunked-oci command.

%prep
%autosetup -n %{crate}-%{version} -p1
tar xf %{SOURCE1}
%cargo_prep -v vendor

%build
%cargo_build
%cargo_vendor_manifest
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies

%install
%cargo_install

%check
%cargo_test

%files
%license LICENSE-MIT LICENSE-APACHE
%license LICENSE.dependencies
%license cargo-vendor.txt
%doc README.md
%{_bindir}/chunkah

%changelog
%autochangelog
