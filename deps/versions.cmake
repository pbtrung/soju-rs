# Pinned dependency releases: the latest stable release of each when last
# updated (2026-10-22). Tarballs are downloaded once into
# build/download/ and checked against these SHA-256 sums; the OpenSSL,
# libxcrypt and SQLite sums were also compared with the ones published
# upstream.

set(LIBUV_VERSION 1.53.0)
set(LIBUV_URL https://dist.libuv.org/dist/v${LIBUV_VERSION}/libuv-v${LIBUV_VERSION}.tar.gz)
set(LIBUV_SHA256 cb0d6dd2128d5a95bd242c6cc982a24fe608fa93da57b6b4ec763b0018c53e64)

set(OPENSSL_VERSION 4.0.3)
set(OPENSSL_URL https://github.com/openssl/openssl/releases/download/openssl-${OPENSSL_VERSION}/openssl-${OPENSSL_VERSION}.tar.gz)
set(OPENSSL_SHA256 325b5c806167c13b40b1ffeadfe0248197c00eccc4cf123ec1e28d2d2fd216d9)

set(LWS_VERSION 5.0.0)
set(LWS_URL https://github.com/warmcat/libwebsockets/archive/refs/tags/v${LWS_VERSION}.tar.gz)
set(LWS_SHA256 f853c6582101cfcee3a5a9e28ae92ab19d9735c5f31f0bb2e9794b5106123962)

set(MIMALLOC_VERSION 3.5.4)
set(MIMALLOC_URL https://github.com/microsoft/mimalloc/archive/refs/tags/v${MIMALLOC_VERSION}.tar.gz)
set(MIMALLOC_SHA256 36e9b5bf1bb703567a0347491721ce2098000db8284dc55cd53d211452723cdb)

set(UTF8PROC_VERSION 2.12.0)
set(UTF8PROC_URL https://github.com/JuliaStrings/utf8proc/releases/download/v${UTF8PROC_VERSION}/utf8proc-${UTF8PROC_VERSION}.tar.gz)
set(UTF8PROC_SHA256 a393fbef160835fb315bc3e91ba8d86f7a73a7cec9e6198b6c60b848b498bfeb)

set(YYJSON_VERSION 0.13.0)
set(YYJSON_URL https://github.com/ibireme/yyjson/archive/refs/tags/${YYJSON_VERSION}.tar.gz)
set(YYJSON_SHA256 34e0f62a2bc11ab20d601e8ca1cc2b2079503aa45119a19133d89d19b94a0fae)

set(SQLITE_VERSION 3.54.1)
set(SQLITE_URL https://www.sqlite.org/2026/sqlite-amalgamation-3540100.zip)
set(SQLITE_SHA256 0a9c8e34352dad46006aa82836d5383bff9dc6bc9ce5a60553c05aa30e6ad1f9)

set(CMOCKA_VERSION 2.0.2)
set(CMOCKA_URL https://cmocka.org/files/2.0/cmocka-${CMOCKA_VERSION}.tar.xz)
set(CMOCKA_SHA256 39f92f366bdf3f1a02af4da75b4a5c52df6c9f7e736c7d65de13283f9f0ef416)

# Only built from source for musl (Alpine doesn't package it) and MSan
# (the system library isn't instrumented). Same version as Arch's package.
set(LIBXCRYPT_VERSION 4.5.2)
set(LIBXCRYPT_URL https://github.com/besser82/libxcrypt/releases/download/v${LIBXCRYPT_VERSION}/libxcrypt-${LIBXCRYPT_VERSION}.tar.xz)
set(LIBXCRYPT_SHA256 71513a31c01a428bccd5367a32fd95f115d6dac50fb5b60c779d5c7942aec071)
