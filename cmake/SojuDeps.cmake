# Builds the vendored dependencies (deps/) for this compiler and mode, once,
# into a prefix shared by all build trees, then defines imported targets:
#   soju::uv soju::ssl soju::crypto soju::lws soju::utf8proc soju::yyjson
#   soju::sqlite3 soju::cmocka soju::xcrypt soju::mimalloc_obj (if used)

set(SOJU_DEPS_ROOT "${CMAKE_SOURCE_DIR}/build/deps" CACHE PATH
	"Where dependency builds are kept")
set(SOJU_DOWNLOAD_DIR "${CMAKE_SOURCE_DIR}/build/download" CACHE PATH
	"Dependency tarball cache")

# The key changes when the compiler, mode or any dependency recipe changes
file(SHA256 "${CMAKE_SOURCE_DIR}/deps/versions.cmake" h1)
file(SHA256 "${CMAKE_SOURCE_DIR}/deps/CMakeLists.txt" h2)
file(SHA256 "${CMAKE_SOURCE_DIR}/deps/sqlite/CMakeLists.txt" h3)
file(SHA256 "${CMAKE_SOURCE_DIR}/cmake/SojuMode.cmake" h4)
file(SHA256 "${CMAKE_SOURCE_DIR}/deps/fetch.cmake" h5)
file(SHA256 "${CMAKE_SOURCE_DIR}/deps/patches/openssl-callback-types.patch" h6)
file(SHA256 "${CMAKE_SOURCE_DIR}/deps/patches/lws-address-size.patch" h7)
string(SHA256 recipe "${h1}${h2}${h3}${h4}${h5}${h6}${h7}${CMAKE_C_COMPILER}${SOJU_TRIPLE}")
string(SUBSTRING "${recipe}" 0 12 recipe)
string(TOLOWER "${CMAKE_C_COMPILER_ID}" cc_id)
set(key "${cc_id}-${CMAKE_C_COMPILER_VERSION}-${SOJU_MODE}-${recipe}")
set(SOJU_DEPS_PREFIX "${SOJU_DEPS_ROOT}/${key}")

file(MAKE_DIRECTORY "${SOJU_DEPS_ROOT}")
file(LOCK "${SOJU_DEPS_ROOT}/${key}.lock" GUARD PROCESS TIMEOUT 7200)
if(NOT EXISTS "${SOJU_DEPS_PREFIX}/.done")
	message(STATUS "Building dependencies into ${SOJU_DEPS_PREFIX} (once)")
	set(bdir "${SOJU_DEPS_ROOT}/${key}-build")
	file(REMOVE_RECURSE "${bdir}" "${SOJU_DEPS_PREFIX}")
	execute_process(
		COMMAND ${CMAKE_COMMAND} -S "${CMAKE_SOURCE_DIR}/deps" -B "${bdir}" -G Ninja
			-DCMAKE_C_COMPILER=${CMAKE_C_COMPILER}
			-DCMAKE_INSTALL_PREFIX=${SOJU_DEPS_PREFIX}
			-DSOJU_MODE=${SOJU_MODE}
			"-DSOJU_DEP_CFLAGS=${SOJU_DEP_CFLAGS}"
			"-DSOJU_DEP_LDFLAGS=${SOJU_DEP_LDFLAGS}"
			-DSOJU_DOWNLOAD_DIR=${SOJU_DOWNLOAD_DIR}
			-DSOJU_WITH_MIMALLOC=${SOJU_USE_MIMALLOC}
			-DSOJU_MIMALLOC_DEBUG=${SOJU_MIMALLOC_DEBUG}
			-DSOJU_MIMALLOC_SECURE=${SOJU_MIMALLOC_SECURE}
			-DSOJU_MIMALLOC_TRACK=${SOJU_MIMALLOC_TRACK}
			-DSOJU_OPENSSL_NO_ASM=${SOJU_OPENSSL_NO_ASM}
			-DSOJU_BUNDLE_LIBXCRYPT=${SOJU_BUNDLE_LIBXCRYPT}
			-DSOJU_MUSL=${SOJU_MUSL}
		RESULT_VARIABLE res)
	if(NOT res EQUAL 0)
		message(FATAL_ERROR "Configuring dependencies failed")
	endif()
	execute_process(COMMAND ${CMAKE_COMMAND} --build "${bdir}" RESULT_VARIABLE res)
	if(NOT res EQUAL 0)
		message(FATAL_ERROR "Building dependencies failed (logs in ${bdir})")
	endif()
	file(TOUCH "${SOJU_DEPS_PREFIX}/.done")
	file(REMOVE_RECURSE "${bdir}")
endif()
file(LOCK "${SOJU_DEPS_ROOT}/${key}.lock" RELEASE)

set(P "${SOJU_DEPS_PREFIX}")
function(soju_import name lib)
	add_library(soju::${name} STATIC IMPORTED GLOBAL)
	set_target_properties(soju::${name} PROPERTIES
		IMPORTED_LOCATION "${P}/lib/${lib}"
		INTERFACE_INCLUDE_DIRECTORIES "${P}/include"
		INTERFACE_LINK_LIBRARIES "${ARGN}")
endfunction()

find_package(Threads REQUIRED)
soju_import(uv libuv.a Threads::Threads dl)
soju_import(crypto libcrypto.a Threads::Threads)
soju_import(ssl libssl.a soju::crypto)
soju_import(lws libwebsockets.a soju::ssl soju::uv)
soju_import(utf8proc libutf8proc.a)
target_compile_definitions(soju::utf8proc INTERFACE UTF8PROC_STATIC)
soju_import(yyjson libyyjson.a)
soju_import(sqlite3 libsqlite3.a Threads::Threads m)
soju_import(cmocka libcmocka.a)

if(SOJU_BUNDLE_LIBXCRYPT)
	soju_import(xcrypt libcrypt.a)
else()
	find_library(SOJU_SYSTEM_LIBCRYPT NAMES crypt REQUIRED)
	find_path(SOJU_SYSTEM_CRYPT_H crypt.h REQUIRED)
	add_library(soju::xcrypt UNKNOWN IMPORTED GLOBAL)
	set_target_properties(soju::xcrypt PROPERTIES
		IMPORTED_LOCATION "${SOJU_SYSTEM_LIBCRYPT}"
		INTERFACE_INCLUDE_DIRECTORIES "${SOJU_SYSTEM_CRYPT_H}")
endif()

if(SOJU_USE_MIMALLOC)
	# The object file overrides malloc for the whole process, including the
	# dependencies (static override, see mimalloc's readme)
	# Named after the variant: mimalloc.o, mimalloc-secure.o, ...
	file(GLOB mi_obj "${P}/lib/mimalloc*.o")
	list(LENGTH mi_obj n)
	if(NOT n EQUAL 1)
		message(FATAL_ERROR "Expected one mimalloc object in ${P}/lib, found: ${mi_obj}")
	endif()
	add_library(soju::mimalloc_obj OBJECT IMPORTED GLOBAL)
	set_target_properties(soju::mimalloc_obj PROPERTIES
		IMPORTED_OBJECTS "${mi_obj}"
		INTERFACE_INCLUDE_DIRECTORIES "${P}/include")
endif()
