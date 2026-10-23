# Builds the SQLite amalgamation with the options of rusqlite's bundled
# build (libsqlite3-sys), so the database behaves like soju-rs's. Copied
# into the amalgamation's source directory by deps/CMakeLists.txt.
cmake_minimum_required(VERSION 3.29)
project(sqlite3 C)

add_library(sqlite3 STATIC sqlite3.c)
target_compile_definitions(sqlite3 PRIVATE
	SQLITE_CORE
	SQLITE_DEFAULT_FOREIGN_KEYS=1
	SQLITE_ENABLE_API_ARMOR
	SQLITE_ENABLE_COLUMN_METADATA
	SQLITE_ENABLE_DBSTAT_VTAB
	SQLITE_ENABLE_FTS3
	SQLITE_ENABLE_FTS3_PARENTHESIS
	SQLITE_ENABLE_FTS5
	SQLITE_ENABLE_JSON1
	SQLITE_ENABLE_MEMORY_MANAGEMENT
	SQLITE_ENABLE_RTREE
	SQLITE_ENABLE_STAT4
	SQLITE_SOUNDEX
	SQLITE_USE_URI
	HAVE_USLEEP=1
	HAVE_ISNAN
	# soju never loads extensions; rusqlite enables it, we don't
	SQLITE_OMIT_LOAD_EXTENSION
	# One DB thread owns the connection (PLAN.md 2.1 rule 1)
	SQLITE_THREADSAFE=2
)
install(TARGETS sqlite3 ARCHIVE DESTINATION lib)
install(FILES sqlite3.h sqlite3ext.h DESTINATION include)
