# The protobuf forms of the requests and callbacks (IBX_PROTOBUF).
#
#   cmake -S cpp -B build/cpp -DIBX_PROTOBUF=ON -DIBX_PROTO_DIR=<folder of the .proto files>
#
# The messages are the ones of the official client: its .proto files are
# read where they are installed (IBX_PROTO_DIR), protoc writes their C++
# sources into the build folder and they are compiled into the library.
# Nothing of them is copied into this repository.

if(NOT IBX_PROTO_DIR)
  message(FATAL_ERROR
    "IBX_PROTOBUF needs IBX_PROTO_DIR: the folder of the .proto files of the official client "
    "(its source/proto folder), for example -DIBX_PROTO_DIR=C:/api/source/proto")
endif()
file(GLOB IBX_PROTO_FILES CONFIGURE_DEPENDS "${IBX_PROTO_DIR}/*.proto")
if(NOT IBX_PROTO_FILES)
  message(FATAL_ERROR "IBX_PROTO_DIR has no .proto file: ${IBX_PROTO_DIR}")
endif()

# The package's own configuration first, then the module of CMake.
find_package(Protobuf CONFIG QUIET)
if(NOT Protobuf_FOUND)
  find_package(Protobuf REQUIRED)
endif()
if(TARGET protobuf::protoc)
  set(IBX_PROTOC $<TARGET_FILE:protobuf::protoc>)
elseif(Protobuf_PROTOC_EXECUTABLE)
  set(IBX_PROTOC "${Protobuf_PROTOC_EXECUTABLE}")
else()
  find_program(IBX_PROTOC protoc REQUIRED)
endif()

# Current protobuf needs C++17.
if(CMAKE_CXX_STANDARD LESS 17)
  set(CMAKE_CXX_STANDARD 17)
endif()

# One source and one header for each .proto file, in the build folder.
set(IBX_PROTO_OUT "${CMAKE_CURRENT_BINARY_DIR}/proto")
file(MAKE_DIRECTORY "${IBX_PROTO_OUT}")
set(IBX_PROTO_SOURCES "")
set(IBX_PROTO_HEADERS "")
foreach(proto ${IBX_PROTO_FILES})
  get_filename_component(name "${proto}" NAME_WE)
  list(APPEND IBX_PROTO_SOURCES "${IBX_PROTO_OUT}/${name}.pb.cc")
  list(APPEND IBX_PROTO_HEADERS "${IBX_PROTO_OUT}/${name}.pb.h")
endforeach()
add_custom_command(
  OUTPUT ${IBX_PROTO_SOURCES} ${IBX_PROTO_HEADERS}
  COMMAND ${IBX_PROTOC} "--proto_path=${IBX_PROTO_DIR}" "--cpp_out=${IBX_PROTO_OUT}" ${IBX_PROTO_FILES}
  DEPENDS ${IBX_PROTO_FILES}
  COMMENT "Writing the C++ sources of the protobuf messages (protoc)"
  VERBATIM)

# The messages are compiled on their own: the warnings of the library's
# sources are not asked of them.
add_library(ibx_proto_messages OBJECT ${IBX_PROTO_SOURCES})
target_include_directories(ibx_proto_messages PUBLIC "${IBX_PROTO_OUT}")
target_link_libraries(ibx_proto_messages PUBLIC protobuf::libprotobuf)
if(MSVC)
  target_compile_options(ibx_proto_messages PRIVATE /w /utf-8)
else()
  target_compile_options(ibx_proto_messages PRIVATE -w)
endif()

list(APPEND IBX_SOURCES src/proto.cpp src/proto_generated.cpp $<TARGET_OBJECTS:ibx_proto_messages>)

# The headers of the messages are installed beside the library's.
install(FILES ${IBX_PROTO_HEADERS} DESTINATION include/ibx)

# What a target of the library takes from the protobuf forms: the
# definition, the headers of the messages and the protobuf library.
function(ibx_add_protobuf target)
  target_compile_definitions(${target} PUBLIC IBX_PROTOBUF)
  # As headers of another library: their warnings are not this one's.
  target_include_directories(${target} SYSTEM PUBLIC $<BUILD_INTERFACE:${IBX_PROTO_OUT}>)
  target_link_libraries(${target} PUBLIC protobuf::libprotobuf)
  add_dependencies(${target} ibx_proto_messages)
endfunction()
