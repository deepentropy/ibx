# The C++ library of ibx: the classes of the official C++ client, on the
# Rust client (ibx#235).
#
#   cmake -S cpp -B build/cpp -DCMAKE_BUILD_TYPE=Release
#   cmake --build build/cpp
#
# Targets: `ibx` (static library, link it), `ibx_core` (the Rust client as
# a shared library, built here with cargo).
cmake_minimum_required(VERSION 3.16)
project(ibx_cpp VERSION 0.8.0 LANGUAGES CXX)

option(IBX_BUILD_EXAMPLES "Build the examples" ON)
option(IBX_BUILD_TESTS "Build the tests (the Rust client is then built with its test helpers)" OFF)
option(IBX_PROTOBUF "Build the protobuf forms of the requests and callbacks (needs protobuf and IBX_PROTO_DIR)" OFF)
set(IBX_PROTO_DIR "" CACHE PATH "The folder of the .proto files of the official client (for IBX_PROTOBUF)")
set(IBX_OFFICIAL_CLIENT_DIR "" CACHE PATH "The source folder of the official C++ client, for the signature tests")

if(NOT CMAKE_CXX_STANDARD)
  set(CMAKE_CXX_STANDARD 14)
endif()
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

# ── The Rust client, with its C entry layer ──

find_program(IBX_CARGO cargo REQUIRED)
set(IBX_CARGO_DIR "${CMAKE_BINARY_DIR}/cargo")
if(CMAKE_BUILD_TYPE STREQUAL "Debug")
  set(IBX_CARGO_PROFILE_ARGS "")
  set(IBX_CARGO_OUT "${IBX_CARGO_DIR}/debug")
else()
  set(IBX_CARGO_PROFILE_ARGS "--release")
  set(IBX_CARGO_OUT "${IBX_CARGO_DIR}/release")
endif()
if(WIN32)
  set(IBX_CORE_FILE "${IBX_CARGO_OUT}/ibx.dll")
  set(IBX_CORE_IMPLIB "${IBX_CARGO_OUT}/ibx.dll.lib")
elseif(APPLE)
  set(IBX_CORE_FILE "${IBX_CARGO_OUT}/libibx.dylib")
else()
  set(IBX_CORE_FILE "${IBX_CARGO_OUT}/libibx.so")
endif()

set(IBX_CARGO_FEATURES "capi")
if(IBX_BUILD_TESTS)
  set(IBX_CARGO_FEATURES "capi,test-support")
endif()

# cargo decides itself whether anything is to be built.
add_custom_target(ibx_rust ALL
  COMMAND ${IBX_CARGO} build --lib --features ${IBX_CARGO_FEATURES} ${IBX_CARGO_PROFILE_ARGS}
          --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/../Cargo.toml" --target-dir "${IBX_CARGO_DIR}"
  BYPRODUCTS "${IBX_CORE_FILE}" "${IBX_CORE_IMPLIB}"
  COMMENT "Building the Rust client (cargo)"
  VERBATIM)

add_library(ibx_core SHARED IMPORTED GLOBAL)
set_target_properties(ibx_core PROPERTIES IMPORTED_LOCATION "${IBX_CORE_FILE}")
if(WIN32)
  set_target_properties(ibx_core PROPERTIES IMPORTED_IMPLIB "${IBX_CORE_IMPLIB}")
else()
  set_target_properties(ibx_core PROPERTIES IMPORTED_NO_SONAME TRUE)
endif()
add_dependencies(ibx_core ibx_rust)

# ── The C++ classes ──

set(IBX_SOURCES
  src/Decimal.cpp
  src/client.cpp
  src/generated.cpp
  src/runtime.cpp)

if(IBX_PROTOBUF)
  include(cmake/protobuf.cmake)
endif()

add_library(ibx STATIC ${IBX_SOURCES})
target_include_directories(ibx PUBLIC
  $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
  $<INSTALL_INTERFACE:include/ibx>)
target_include_directories(ibx PRIVATE src)
target_link_libraries(ibx PUBLIC ibx_core)
add_dependencies(ibx ibx_rust)
find_package(Threads REQUIRED)
target_link_libraries(ibx PUBLIC Threads::Threads)
if(MSVC)
  target_compile_options(ibx PRIVATE /W4 /utf-8)
else()
  target_compile_options(ibx PRIVATE -Wall -Wextra)
endif()
if(IBX_PROTOBUF)
  ibx_add_protobuf(ibx)
endif()

# A program finds the Rust client beside it.
function(ibx_copy_core target)
  add_custom_command(TARGET ${target} POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E copy_if_different "${IBX_CORE_FILE}" "$<TARGET_FILE_DIR:${target}>"
    VERBATIM)
endfunction()

if(IBX_BUILD_EXAMPLES)
  add_subdirectory(examples)
endif()

if(IBX_BUILD_TESTS)
  enable_testing()
  add_subdirectory(tests)
endif()

# ── Install: the headers, the static library and the Rust client ──

include(GNUInstallDirs)
install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR}/ibx)
install(TARGETS ibx ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR})
if(WIN32)
  install(FILES "${IBX_CORE_FILE}" DESTINATION ${CMAKE_INSTALL_BINDIR})
  install(FILES "${IBX_CORE_IMPLIB}" DESTINATION ${CMAKE_INSTALL_LIBDIR})
else()
  install(FILES "${IBX_CORE_FILE}" DESTINATION ${CMAKE_INSTALL_LIBDIR})
endif()
