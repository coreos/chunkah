%global crate chunkah

Name:           chunkah
Version:        0.3.1
Release:        %autorelease
Summary:        OCI building tool for content-based container image layers

# (MIT OR Apache-2.0) AND Unicode-3.0
# 0BSD OR MIT OR Apache-2.0
# Apache-2.0
# Apache-2.0 OR MIT
# Apache-2.0 WITH LLVM-exception
# Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT
# BSD-2-Clause OR Apache-2.0 OR MIT
# LGPL-2.1-or-later
# MIT
# MIT OR Apache-2.0
# MIT OR Apache-2.0 OR LGPL-2.1-or-later
# MIT OR Zlib OR Apache-2.0
# Unlicense OR MIT
# Zlib
# Zlib OR Apache-2.0 OR MIT
License:        Apache-2.0 AND Apache-2.0 WITH LLVM-exception AND LGPL-2.1-or-later AND MIT AND Zlib AND ((MIT OR Apache-2.0) AND Unicode-3.0) AND (0BSD OR MIT OR Apache-2.0) AND (Apache-2.0 OR MIT) AND (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT) AND (BSD-2-Clause OR Apache-2.0 OR MIT) AND (MIT OR Apache-2.0 OR LGPL-2.1-or-later) AND (MIT OR Apache-2.0) AND (MIT OR Zlib OR Apache-2.0) AND (Unlicense OR MIT) AND (Zlib OR Apache-2.0 OR MIT)
URL:            https://github.com/coreos/chunkah
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
