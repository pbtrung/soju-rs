# Builds the vendored dependencies as static libraries into
# CMAKE_INSTALL_PREFIX, with the compiler and flags of one build mode. The
# top-level project runs this once per (compiler, mode, versions) and
# shares the result between build trees (cmake/SojuDeps.cmake).
cmake_minimum_required(VERSION 3.29)
project(soju_deps C)

include(ExternalProject)
include(${CMAKE_CURRENT_LIST_DIR}/versions.cmake)

set(SOJU_MODE "debug" CACHE STRING "Build mode")
set(SOJU_DEP_CFLAGS "" CACHE STRING "C flags for the dependencies")
set(SOJU_DEP_LDFLAGS "" CACHE STRING "Linker flags for the dependencies")
set(SOJU_DOWNLOAD_DIR "${CMAKE_BINARY_DIR}/download" CACHE PATH "Tarball cache")
option(SOJU_WITH_MIMALLOC "Build mimalloc" ON)
set(SOJU_MIMALLOC_TRACK "OFF" CACHE STRING "mimalloc MI_TRACK")
option(SOJU_MIMALLOC_DEBUG "mimalloc debug checks" OFF)
option(SOJU_MIMALLOC_SECURE "mimalloc secure mode" OFF)
option(SOJU_BUNDLE_LIBXCRYPT "Build libxcrypt from source" OFF)
option(SOJU_OPENSSL_NO_ASM "Build OpenSSL without assembly (MSan)" OFF)
option(SOJU_MUSL "Target musl libc" OFF)

set(P "${CMAKE_INSTALL_PREFIX}")
set(cflags "${SOJU_DEP_CFLAGS} -fPIC")
set(common_cmake_args
	-DCMAKE_BUILD_TYPE=RelWithDebInfo
	-DCMAKE_C_COMPILER=${CMAKE_C_COMPILER}
	"-DCMAKE_C_FLAGS=${cflags}"
	"-DCMAKE_C_FLAGS_RELWITHDEBINFO="
	"-DCMAKE_EXE_LINKER_FLAGS=${SOJU_DEP_LDFLAGS}"
	-DCMAKE_INSTALL_PREFIX=${P}
	-DCMAKE_INSTALL_LIBDIR=lib
	-DCMAKE_POSITION_INDEPENDENT_CODE=ON
	-DCMAKE_PREFIX_PATH=${P}
	-DBUILD_SHARED_LIBS=OFF
	-DBUILD_TESTING=OFF
	-G Ninja
)
set(dl DOWNLOAD_DIR ${SOJU_DOWNLOAD_DIR} DOWNLOAD_EXTRACT_TIMESTAMP ON)

ExternalProject_Add(libuv
	URL ${LIBUV_URL} URL_HASH SHA256=${LIBUV_SHA256} ${dl}
	CMAKE_ARGS ${common_cmake_args}
		-DLIBUV_BUILD_SHARED=OFF -DLIBUV_BUILD_TESTS=OFF -DLIBUV_BUILD_BENCH=OFF
)

# OpenSSL's own build system. Only libcrypto and libssl are installed.
set(openssl_opts no-shared no-tests no-docs no-apps no-module no-dso)
if(SOJU_OPENSSL_NO_ASM)
	list(APPEND openssl_opts no-asm)
endif()
ExternalProject_Add(openssl
	URL ${OPENSSL_URL} URL_HASH SHA256=${OPENSSL_SHA256} ${dl}
	BUILD_IN_SOURCE ON
	CONFIGURE_COMMAND ${CMAKE_COMMAND} -E env CC=${CMAKE_C_COMPILER}
		"CFLAGS=${cflags}" "LDFLAGS=${SOJU_DEP_LDFLAGS}"
		perl ./Configure linux-x86_64 --prefix=${P} --libdir=lib
		--openssldir=/etc/ssl ${openssl_opts}
	BUILD_COMMAND make -j build_libs
	INSTALL_COMMAND make install_dev
)

ExternalProject_Add(libwebsockets
	URL ${LWS_URL} URL_HASH SHA256=${LWS_SHA256} ${dl}
	DEPENDS libuv openssl
	LIST_SEPARATOR |
	CMAKE_ARGS ${common_cmake_args}
		-DDISABLE_WERROR=ON
		-DLWS_WITH_STATIC=ON -DLWS_WITH_SHARED=OFF
		-DLWS_WITH_LIBUV=ON -DLWS_WITH_EVLIB_PLUGINS=OFF
		-DLWS_LIBUV_LIBRARIES=${P}/lib/libuv.a
		-DLWS_LIBUV_INCLUDE_DIRS=${P}/include
		-DLWS_WITH_SSL=ON
		"-DLWS_OPENSSL_LIBRARIES=${P}/lib/libssl.a|${P}/lib/libcrypto.a"
		-DLWS_OPENSSL_INCLUDE_DIRS=${P}/include
		-DLWS_WITH_HTTP2=OFF -DLWS_WITH_HTTP3=OFF
		-DLWS_IPV6=ON -DLWS_UNIX_SOCK=ON
		-DLWS_WITHOUT_TESTAPPS=ON -DLWS_WITH_MINIMAL_EXAMPLES=OFF
		-DLWS_WITH_SECURE_STREAMS=OFF -DLWS_WITH_UPNG=OFF -DLWS_WITH_JPEG=OFF
		-DLWS_WITH_LHP=OFF -DLWS_WITH_LEJP=OFF -DLWS_WITH_CONMON=OFF
		-DLWS_WITH_NETLINK=OFF -DLWS_WITH_CACHE_NSCOOKIEJAR=OFF
		-DLWS_WITH_UDP=OFF -DLWS_WITH_DIR=OFF -DLWS_WITH_LEJP_CONF=OFF
		-DLWS_WITH_SYS_SMD=OFF -DLWS_WITH_SYS_STATE=OFF
		-DLWS_WITH_LWSAC=ON -DLWS_WITH_LIBCAP=OFF
		# Not our git repository's hash in lws_get_library_version()
		-DCMAKE_DISABLE_FIND_PACKAGE_Git=ON -DLWS_REPRODUCIBLE=ON
)

if(SOJU_WITH_MIMALLOC)
	set(mi_debug OFF)
	set(mi_padding OFF)
	if(SOJU_MIMALLOC_DEBUG)
		set(mi_debug FULL)
		set(mi_padding ON)
	endif()
	set(mi_secure OFF)
	if(SOJU_MIMALLOC_SECURE)
		set(mi_secure ON)
	endif()
	ExternalProject_Add(mimalloc
		URL ${MIMALLOC_URL} URL_HASH SHA256=${MIMALLOC_SHA256} ${dl}
		CMAKE_ARGS ${common_cmake_args}
			-DMI_OVERRIDE=ON -DMI_BUILD_SHARED=OFF -DMI_BUILD_STATIC=ON
			-DMI_BUILD_OBJECT=ON -DMI_BUILD_TESTS=OFF -DMI_INSTALL_TOPLEVEL=ON
			-DMI_DEBUG=${mi_debug} -DMI_TRACK=${SOJU_MIMALLOC_TRACK}
			-DMI_SECURE=${mi_secure}
			-DMI_LIBC_MUSL=${SOJU_MUSL} -DMI_NO_USE_CXX=ON
			-DMI_PADDING=${mi_padding}
	)
endif()

ExternalProject_Add(utf8proc
	URL ${UTF8PROC_URL} URL_HASH SHA256=${UTF8PROC_SHA256} ${dl}
	CMAKE_ARGS ${common_cmake_args} -DUTF8PROC_INSTALL=ON -DUTF8PROC_ENABLE_TESTING=OFF
)

ExternalProject_Add(yyjson
	URL ${YYJSON_URL} URL_HASH SHA256=${YYJSON_SHA256} ${dl}
	CMAKE_ARGS ${common_cmake_args}
)

ExternalProject_Add(sqlite
	URL ${SQLITE_URL} URL_HASH SHA256=${SQLITE_SHA256} ${dl}
	PATCH_COMMAND ${CMAKE_COMMAND} -E copy
		${CMAKE_CURRENT_LIST_DIR}/sqlite/CMakeLists.txt <SOURCE_DIR>/CMakeLists.txt
	CMAKE_ARGS ${common_cmake_args}
)

ExternalProject_Add(cmocka
	URL ${CMOCKA_URL} URL_HASH SHA256=${CMOCKA_SHA256} ${dl}
	CMAKE_ARGS ${common_cmake_args} -DWITH_EXAMPLES=OFF -DUNIT_TESTING=OFF
)

if(SOJU_BUNDLE_LIBXCRYPT)
	ExternalProject_Add(libxcrypt
		URL ${LIBXCRYPT_URL} URL_HASH SHA256=${LIBXCRYPT_SHA256} ${dl}
		CONFIGURE_COMMAND <SOURCE_DIR>/configure --prefix=${P} --libdir=${P}/lib
			--disable-shared --enable-static --disable-werror
			--enable-obsolete-api=no --enable-hashes=strong,glibc
			CC=${CMAKE_C_COMPILER} "CFLAGS=${cflags}" "LDFLAGS=${SOJU_DEP_LDFLAGS}"
		BUILD_COMMAND make -j
		INSTALL_COMMAND make install
	)
endif()
