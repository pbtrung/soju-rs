# Build and test image for the x86_64-linux-gnu target (PLAN.md 3.1-3.2):
# the latest GCC and Clang of a freshly upgraded Arch Linux, with the
# sanitizer runtimes (compiler-rt), llvm-symbolizer and valgrind, which
# needs glibc's debug symbols (Arch strips ld.so; glibc-debug comes from the
# core-debug repository).
# Built with `docker build --pull` by tools/docker-test.sh and
# scripts/build-release.sh; builds run as the invoking user.
FROM docker.io/library/archlinux:latest

RUN pacman -Syu --noconfirm --needed \
		gcc clang compiler-rt llvm lld clang-tools-extra \
		cmake ninja make perl pkgconf patch \
		openssl libxcrypt curl llhttp valgrind shellcheck ripgrep \
		bash coreutils diffutils file findutils grep sed tar gzip xz util-linux \
	&& printf '[core-debug]\nServer = https://geo.mirror.pkgbuild.com/$repo/os/$arch\n' >>/etc/pacman.conf \
	&& pacman -Sy --noconfirm glibc-debug \
	&& pacman -Scc --noconfirm \
	&& rm -rf /var/cache/pacman/pkg/* \
	&& gcc --version && clang --version && valgrind --version

WORKDIR /src
