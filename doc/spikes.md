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
- `openssl-callback-types.patch`: sparse-array helpers cast callbacks
  taking typed pointers to function pointers taking `void *`. Clang UBSan
  rejects calls through those incompatible function types. Typed callback
  contexts and correctly declared trampolines remove the casts. The same
  patch adds a correctly typed adapter for the X509 public-key BIO encoder,
  which otherwise calls a typed encoder through a `const void *` callback.
  Key-management enumeration also uses a typed callback context.

## Validation at this checkpoint

GCC debug, GCC ASan/UBSan and Clang debug passed all 26 tests before the
last key-management callback patch. Formatting, clang-tidy, shell syntax
checks and ShellCheck passed. The final four-configuration rerun remains
outstanding. A targeted Clang sanitizer run with that last patch still
fails in OpenSSL's decoder stack deep-copy callback (`ossl_decoder_instance_dup`
called through an incompatible function-pointer type). This finding is
unresolved; sanitizer checks have not been disabled.

## Reproduction

Run `tools/ci.sh -j 4 gcc-debug gcc-asan clang-debug clang-asan` for the
commit checks. Run `tools/docker-test.sh gnu` and
`tools/docker-test.sh musl` for the native target matrices. Each default
Docker run pulls and rebuilds a fresh image; `SOJU_DOCKER_REUSE=1` reuses
an existing image for local iteration. Build outputs and test logs belong
under `build/` and are not committed.
