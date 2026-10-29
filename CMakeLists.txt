cmake_minimum_required(VERSION 3.29)
project(soju VERSION 0.1.0 LANGUAGES C)

set(SOJU_MODE "debug" CACHE STRING "Build mode (PLAN.md 2.2)")
set(SOJU_VERSION_STRING "${PROJECT_VERSION}" CACHE STRING "Version printed by -version")

set(CMAKE_C_STANDARD 23)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_C_EXTENSIONS OFF)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
set(CMAKE_BUILD_TYPE "" CACHE STRING "Unused: flags come from SOJU_MODE" FORCE)

include(cmake/SojuMode.cmake)
include(cmake/SojuDeps.cmake)

string(APPEND CMAKE_C_FLAGS " ${SOJU_CFLAGS}")
string(APPEND CMAKE_EXE_LINKER_FLAGS " ${SOJU_LDFLAGS}")

if(SOJU_MODE STREQUAL "valgrind")
	find_program(VALGRIND valgrind REQUIRED)
	# The malloc override is linked statically: tell valgrind there is no
	# malloc library to intercept (mimalloc reports to it via MI_TRACK)
	set(CMAKE_TEST_LAUNCHER ${VALGRIND} --error-exitcode=99 --leak-check=full
		--show-leak-kinds=all --errors-for-leak-kinds=all --track-origins=yes
		--soname-synonyms=somalloc=NONE
		--suppressions=${CMAKE_SOURCE_DIR}/tests/valgrind.supp)
endif()
enable_testing()

# Sanitizer runtime options for every test: fail on the first report, and
# use the suppression files (PLAN.md 2.4)
set(SOJU_TEST_ENV
	"ASAN_OPTIONS=detect_leaks=1:strict_string_checks=1:detect_stack_use_after_return=1:check_initialization_order=1:detect_invalid_pointer_pairs=1"
	"LSAN_OPTIONS=suppressions=${CMAKE_SOURCE_DIR}/tests/lsan.supp:print_suppressions=0"
	"UBSAN_OPTIONS=print_stacktrace=1:halt_on_error=1"
	"MSAN_OPTIONS=halt_on_error=1"
	"TSAN_OPTIONS=halt_on_error=1:second_deadlock_stack=1"
)
function(soju_test_env name)
	set_property(TEST "${name}" APPEND PROPERTY ENVIRONMENT ${SOJU_TEST_ENV} ${ARGN})
endfunction()

# Our code: everything under src/ except the programs' main files
file(GLOB_RECURSE soju_sources CONFIGURE_DEPENDS src/*.c)
list(FILTER soju_sources EXCLUDE REGEX "/src/main/")
add_library(soju_core STATIC ${soju_sources})
target_include_directories(soju_core PUBLIC src "${CMAKE_BINARY_DIR}/gen")
target_compile_options(soju_core PRIVATE ${SOJU_WARNINGS})
target_compile_definitions(soju_core PUBLIC _GNU_SOURCE
	"SOJU_MODE_$<UPPER_CASE:$<MAKE_C_IDENTIFIER:${SOJU_MODE}>>=1")
target_link_libraries(soju_core PUBLIC
	soju::curl soju::ssl soju::crypto soju::uv soju::utf8proc soju::yyjson
	soju::sqlite3 soju::xcrypt)
configure_file(src/version.h.in "${CMAKE_BINARY_DIR}/gen/version.h" @ONLY)

# Links the malloc override first, so it wins over libc's malloc
function(soju_executable name)
	add_executable(${name} ${ARGN})
	target_compile_options(${name} PRIVATE ${SOJU_WARNINGS})
	if(SOJU_USE_MIMALLOC)
		target_sources(${name} PRIVATE $<TARGET_OBJECTS:soju::mimalloc_obj>)
		target_compile_definitions(${name} PRIVATE SOJU_MIMALLOC=1)
	endif()
	target_link_libraries(${name} PRIVATE soju_core)
endfunction()

soju_executable(soju src/main/soju.c src/main/buildinfo.c)
soju_executable(sojuctl src/main/sojuctl.c)
soju_executable(hello spikes/hello.c src/main/buildinfo.c)
target_link_libraries(hello PRIVATE soju::cmocka)
add_test(NAME hello COMMAND hello)
soju_test_env(hello)

# Unit tests: one executable per tests/unit/test_*.c; each cmocka test in it
# is registered as its own CTest test, so every test runs in its own
# process (CMOCKA_TEST_FILTER selects it).
add_library(soju_testsupport STATIC tests/support/testsupport.c tests/support/server_fixture.c
	tests/support/http_mock.c)
target_link_libraries(soju_testsupport PUBLIC soju_core soju::cmocka)
target_include_directories(soju_testsupport PUBLIC tests/support)
target_compile_options(soju_testsupport PRIVATE ${SOJU_WARNINGS})

file(GLOB unit_tests CONFIGURE_DEPENDS tests/unit/test_*.c tests/integration/test_*.c)
foreach(src IN LISTS unit_tests)
	get_filename_component(exe "${src}" NAME_WE)
	soju_executable(${exe} "${src}")
	target_link_libraries(${exe} PRIVATE soju_testsupport)
	file(STRINGS "${src}" lines REGEX "cmocka_unit_test[a-z_]*\\([a-z0-9_]+")
	foreach(line IN LISTS lines)
		string(REGEX MATCHALL "cmocka_unit_test[a-z_]*\\([a-z0-9_]+" calls "${line}")
		foreach(call IN LISTS calls)
			string(REGEX REPLACE ".*\\(" "" t "${call}")
			if(src MATCHES "/integration/")
				foreach(loops 1 4)
					foreach(delay 0 20)
						set(name "${exe}.${t}.loops${loops}.delay${delay}")
						add_test(NAME "${name}" COMMAND ${exe})
						set_tests_properties("${name}" PROPERTIES LABELS integration TIMEOUT 120)
						soju_test_env("${name}" "CMOCKA_TEST_FILTER=${t}" "SOJU_LOOPS=${loops}" "SOJU_DB_DELAY=${delay}")
					endforeach()
				endforeach()
			else()
				add_test(NAME "${exe}.${t}" COMMAND ${exe})
				set_tests_properties("${exe}.${t}" PROPERTIES LABELS unit TIMEOUT 120)
				soju_test_env("${exe}.${t}" "CMOCKA_TEST_FILTER=${t}")
			endif()
		endforeach()
	endforeach()
endforeach()

# Fuzz harnesses (fuzz/fuzz_*.c). With SOJU_MODE=fuzz they are libFuzzer
# binaries; in every other mode they are linked with a replay driver and
# run over their committed seeds (fuzz/seeds/<name>/) as regression tests.
#
# fuzz/diff_*.c are differential harnesses comparing the C port with
# soju-rs through tests/oracle (built by tools/oracle.sh); without the
# oracle binary they only run the C side.
set(SOJU_ORACLE "${CMAKE_SOURCE_DIR}/build/oracle/release/soju-oracle" CACHE FILEPATH
	"Differential oracle binary (tools/oracle.sh)")
file(GLOB fuzzers CONFIGURE_DEPENDS fuzz/fuzz_*.c fuzz/diff_*.c)
foreach(src IN LISTS fuzzers)
	get_filename_component(exe "${src}" NAME_WE)
	set(extra "")
	if(exe MATCHES "^diff_")
		set(extra fuzz/oracle_ops.c)
	endif()
	if(SOJU_MODE STREQUAL "fuzz")
		soju_executable(${exe} "${src}" ${extra})
		target_link_options(${exe} PRIVATE -fsanitize=fuzzer)
	else()
		soju_executable(${exe} "${src}" fuzz/replay.c ${extra})
		if(IS_DIRECTORY "${CMAKE_SOURCE_DIR}/fuzz/seeds/${exe}")
			add_test(NAME "${exe}.seeds"
				COMMAND ${exe} "${CMAKE_SOURCE_DIR}/fuzz/seeds/${exe}")
			set_tests_properties("${exe}.seeds" PROPERTIES LABELS fuzz-replay TIMEOUT 300)
			soju_test_env("${exe}.seeds")
		endif()
	endif()
	if(exe MATCHES "^diff_")
		target_compile_definitions(${exe} PRIVATE "SOJU_ORACLE_DEFAULT=\"${SOJU_ORACLE}\"")
		target_include_directories(${exe} PRIVATE fuzz)
	endif()
endforeach()

# Phase 0 spikes (spikes/*.c): throwaway programs kept building and passing
# so their conclusions stay true when dependencies are upgraded.
file(GLOB spikes CONFIGURE_DEPENDS spikes/spike_*.c)
foreach(src IN LISTS spikes)
	get_filename_component(exe "${src}" NAME_WE)
	soju_executable(${exe} "${src}")
	add_test(NAME "${exe}" COMMAND ${exe})
	set_tests_properties("${exe}" PROPERTIES LABELS spike TIMEOUT 120)
	soju_test_env("${exe}")
endforeach()

# Every executable is also a smoke test of the link
add_test(NAME soju.version COMMAND soju -version)
set_tests_properties(soju.version PROPERTIES
	PASS_REGULAR_EXPRESSION "soju ${SOJU_VERSION_STRING}" LABELS smoke)
soju_test_env(soju.version)
add_test(NAME soju.build-info COMMAND soju -build-info)
set_tests_properties(soju.build-info PROPERTIES
	PASS_REGULAR_EXPRESSION "libcurl" LABELS smoke)
soju_test_env(soju.build-info)

install(TARGETS soju sojuctl RUNTIME DESTINATION bin)
