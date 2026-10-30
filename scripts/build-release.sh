#!/bin/bash
# Build dynamically linked GNU and musl archives in the native Docker images.
set -euo pipefail
cd "$(dirname "$0")/.."
version=${SOJU_VERSION:-v0.99.0}
[[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { echo 'invalid SOJU_VERSION' >&2; exit 2; }
mkdir -p dist
for target in "${@:-gnu musl}"; do
  # With no arguments expand the two defaults without treating them as one target.
  for libc in $target; do
    [[ "$libc" == gnu || "$libc" == musl ]] || { echo 'expected gnu or musl' >&2; exit 2; }
    image=soju-build-$libc
    if [[ ${SOJU_DOCKER_REUSE:-0} != 1 ]]; then
      docker build --pull --no-cache -t "$image" -f "docker/Dockerfile.$libc" .
    fi
    mkdir -p "build/docker/$libc"
    docker run --rm --init --security-opt seccomp=unconfined --user "$(id -u):$(id -g)" \
      -v "$PWD:/src:ro" -v "$PWD/build/docker/$libc:/src/build" -v "$PWD/dist:/out" \
      -w /src -e SOJU_RELEASE_VERSION="$version" -e SOJU_RELEASE_LIBC="$libc" "$image" bash -euo pipefail -c '
      name=soju-$SOJU_RELEASE_VERSION-x86_64-linux-$SOJU_RELEASE_LIBC
      cmake --preset gcc-release -DSOJU_VERSION_STRING="$SOJU_RELEASE_VERSION"
      cmake --build --preset gcc-release -j4
      ctest --preset gcc-release
      stage=$(mktemp -d /src/build/release-XXXXXX)
      cmake --install build/gcc-release --prefix "$stage/$name"
      cp LICENSE README.md config.in "$stage/$name/"
      printf "Runtime packages: openssl curl llhttp\n" > "$stage/$name/RUNTIME.txt"
      if [[ $SOJU_RELEASE_LIBC == gnu ]]; then
        printf "Also requires glibc and libxcrypt.\n" >> "$stage/$name/RUNTIME.txt"
      else
        printf "Also requires musl; libxcrypt is linked into the executable.\n" >> "$stage/$name/RUNTIME.txt"
      fi
      for program in soju sojuctl; do
        objcopy --only-keep-debug "$stage/$name/bin/$program" "$stage/$name/bin/$program.debug"
        strip --strip-debug "$stage/$name/bin/$program"
        objcopy --add-gnu-debuglink="$stage/$name/bin/$program.debug" "$stage/$name/bin/$program"
        readelf -l "$stage/$name/bin/$program" | grep "Requesting program interpreter"
        ldd "$stage/$name/bin/$program"
      done
      "$stage/$name/bin/soju" -version | grep -Fx "soju $SOJU_RELEASE_VERSION"
      tar -C "$stage" -czf "/out/$name.tar.gz" "$name"
    '
  done
done
# Only this version's archives: dist/ may hold other builds
(cd dist && sha256sum ./soju-"$version"-*.tar.gz > SHA256SUMS)
