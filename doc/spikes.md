# Bootstrap checkpoint

Phase 0 is in progress. Phase 1 has not been completed. The full target,
compiler and sanitizer matrix, Valgrind runs, differential oracle and
extended fuzzing gates in PLAN.md remain outstanding.

The dependency versions inherited from the bootstrap are listed in
`deps/VERSIONS`, with URLs and SHA-256 hashes in `deps/versions.cmake`.
In particular, the existing pin is OpenSSL 4.0.3, although the original
plan describes OpenSSL 3.

## Executable experiments

- `hello` includes the public dependency headers as C23 and exercises the
  linked libraries, including cmocka and yyjson.
- `spike_lws` consumes a known PROXY header from a socket pair, adopts the
  remaining HTTP stream and checks that client metadata reaches dispatch.
  This tests socket handoff, not a PROXY parser. Avoid reading beyond the
  PROXY header: the descriptor adoption API has no pre-read buffer field.
- Foreign libuv loop shutdown requires `lws_context_destroy`, draining
  the loop, and a final destroy call if `pcontext` is still non-null.
  `spikes/support.h` asserts that finalization clears the context pointer
  and that `uv_loop_close` succeeds.
- `spike_tls` obtains the vhost SSL_CTX through the server verification
  setup callback. The callback installs a generated certificate and key;
  the same context then accepts a replacement certificate. A memory-BIO
  handshake without SNI checks the replacement serial, HTTP/1.1 ALPN and
  acceptance of a self-signed client certificate. An adopted TLS server
  must use `CONTEXT_PORT_NO_LISTEN_SERVER`, not `CONTEXT_PORT_NO_LISTEN`.
- `spike_http_client` connects to a numeric loopback address while sending
  an unrelated Host name, and configures a request timeout. It disables
  automatic redirects. Production code must resolve and validate addresses
  before connecting and repeat that check for redirects. TLS SNI behavior
  was inspected in lws's `openssl-client.c`; the current client experiment
  uses plain HTTP and does not test DNS filtering or timeout expiration.
- `spike_bcrypt` verifies a published bcrypt vector with `$2a$`, `$2b$`
  and `$2y$` prefixes, then generates and verifies a new `$2b$` hash. It
  does not yet run cross-language verification with Go and Rust.

## Dependency fixes

The spikes exposed defects in the pinned dependencies. Source patches
are applied in every build mode, and their contents participate in the
dependency cache key. Sanitizer checks remain enabled.

- `lws-address-size.patch`: the Unix interface lookup cast a caller's
  `sockaddr_in` to the larger IPv4/IPv6 union. Parsing a numeric IPv4
  interface overwrote the stack. Parse into a full union, check the address
  family and destination size, then copy only the matching sockaddr.
- OpenSSL calls its stack, hash and sparse-array callbacks through
  function pointers cast to generic types, by design and throughout the
  library. Clang's UBSan `function` check rejects every such call (first
  seen as `ossl_decoder_instance_dup` called through
  `void *(*)(const void *)` in `OPENSSL_sk_deep_copy`). Patching each call
  site was not bounded, so the dependencies are built with
  `-fno-sanitize=function` in the Clang `asan` and `fuzz` modes
  (`cmake/SojuMode.cmake`). Our own code keeps the check; only calls made
  from inside dependency code are unchecked. The earlier
  `openssl-callback-types.patch` is removed. The dependency cache key now
  hashes every file in `deps/patches/`.

## Validation at this checkpoint

`tools/ci.sh gcc-debug gcc-asan clang-debug clang-asan`: all 26 tests pass
in each preset (2026-10-23).

## Reproduction

Run `tools/ci.sh -j 4 gcc-debug gcc-asan clang-debug clang-asan` for the
commit checks. Run `tools/docker-test.sh gnu` and
`tools/docker-test.sh musl` for the native target matrices. Each default
Docker run pulls and rebuilds a fresh image; `SOJU_DOCKER_REUSE=1` reuses
an existing image for local iteration. Build outputs and test logs belong
under `build/` and are not committed.
