cmake_minimum_required(VERSION 3.30)
project(MyricaWebKitSources NONE)
include(FetchContent)
find_package(Git REQUIRED)

# Cargo owns this build directory. FetchContent owns downloads and their state;
# the application source tree never contains a second WebKit checkout.
set(_webkit_revision "d4179abe1a823d22f86ba788cb3162dbcebcfa62")
FetchContent_Declare(webkit
    DOWNLOAD_COMMAND
        "${GIT_EXECUTABLE}" init <SOURCE_DIR>
        COMMAND "${GIT_EXECUTABLE}" -C <SOURCE_DIR> config remote.origin.url https://github.com/petitstrawberry/WebKit
        COMMAND "${GIT_EXECUTABLE}" -C <SOURCE_DIR> config remote.origin.promisor true
        COMMAND "${GIT_EXECUTABLE}" -C <SOURCE_DIR> config remote.origin.partialclonefilter blob:none
        COMMAND "${GIT_EXECUTABLE}" -C <SOURCE_DIR> fetch --depth=1 --filter=blob:none origin "${_webkit_revision}"
        COMMAND "${GIT_EXECUTABLE}" -C <SOURCE_DIR> sparse-checkout set --no-cone
            "/*" "!/*/" "/Source/" "/Tools/"
            "!/Source/ThirdParty/libwebrtc/"
        COMMAND "${GIT_EXECUTABLE}" -C <SOURCE_DIR> checkout --detach "${_webkit_revision}"
    UPDATE_COMMAND ""
    SOURCE_SUBDIR _myrica_download_only)
FetchContent_MakeAvailable(webkit)
file(WRITE "${CMAKE_BINARY_DIR}/webkit.path" "${webkit_SOURCE_DIR}")

foreach(_name IN ITEMS brotli woff2)
    if(_name STREQUAL "brotli")
        set(_revision ed738e842d2fbdf2d6459e39267a633c4a9b2f5d)
    else()
        set(_revision 1bccf208bca986e53a647dfe4811322adb06ecf8)
    endif()
    FetchContent_Declare(${_name}
        URL "https://codeload.github.com/google/${_name}/tar.gz/${_revision}"
        SOURCE_SUBDIR _myrica_download_only)
    FetchContent_MakeAvailable(${_name})
    file(MAKE_DIRECTORY "${CMAKE_BINARY_DIR}/fonts")
    file(CREATE_LINK "${${_name}_SOURCE_DIR}" "${CMAKE_BINARY_DIR}/fonts/${_name}" SYMBOLIC)
endforeach()

add_subdirectory("${webkit_SOURCE_DIR}/Source/WebCore/scarlet/Dependencies"
                 "${CMAKE_BINARY_DIR}/dependencies")
