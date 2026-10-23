# Publish verified, immutable archives atomically. ExternalProject's default
# downloader truncates shared files when multiple presets bootstrap together.
function(soju_fetch name)
	get_filename_component(filename "${${name}_URL}" NAME)
	set(archive "${SOJU_DOWNLOAD_DIR}/${filename}")
	file(MAKE_DIRECTORY "${SOJU_DOWNLOAD_DIR}")
	file(LOCK "${archive}.lock" GUARD FUNCTION TIMEOUT 600)
	if(EXISTS "${archive}")
		file(SHA256 "${archive}" actual)
		if(actual STREQUAL "${${name}_SHA256}")
			set(${name}_URL "${archive}" PARENT_SCOPE)
			return()
		endif()
	endif()
	file(DOWNLOAD "${${name}_URL}" "${archive}.partial"
		EXPECTED_HASH "SHA256=${${name}_SHA256}" TLS_VERIFY ON
		TIMEOUT 300 INACTIVITY_TIMEOUT 30 STATUS status)
	list(GET status 0 code)
	if(NOT code EQUAL 0)
		message(FATAL_ERROR "Downloading ${name}: ${status}")
	endif()
	file(RENAME "${archive}.partial" "${archive}")
	set(${name}_URL "${archive}" PARENT_SCOPE)
endfunction()
