# The tests of the C++ library. They need the Rust client built with its
# test helpers, which IBX_BUILD_TESTS asks for.

# Offline: every field and callback parameter, the classes, and the
# recorded scenarios replayed through the C++ client.
add_executable(ibx_tests test_library.cpp test_replay.cpp)
target_link_libraries(ibx_tests PRIVATE ibx)
target_include_directories(ibx_tests PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/../src")
if(MSVC)
  target_compile_options(ibx_tests PRIVATE /W4 /utf-8)
else()
  target_compile_options(ibx_tests PRIVATE -Wall -Wextra)
endif()
ibx_copy_core(ibx_tests)
add_test(NAME library COMMAND ibx_tests)

# A session on the paper account, every callback written as a row. Not run
# by ctest: it needs the login of the environment (IB_USERNAME, IB_PASSWORD).
add_executable(live_paper live_paper.cpp)
target_link_libraries(live_paper PRIVATE ibx)
target_include_directories(live_paper PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/../src")
ibx_copy_core(live_paper)

# The names and signatures of the official C++ client, checked against its
# own headers when their folder is given (IBX_OFFICIAL_CLIENT_DIR).
find_package(Python3 COMPONENTS Interpreter)
if(Python3_FOUND AND IBX_OFFICIAL_CLIENT_DIR)
  add_test(NAME signatures
    COMMAND ${Python3_EXECUTABLE} "${CMAKE_CURRENT_SOURCE_DIR}/check_signatures.py"
            "${IBX_OFFICIAL_CLIENT_DIR}" "${CMAKE_CURRENT_SOURCE_DIR}/../include")
endif()

# The generated sources are the ones the tables give.
if(Python3_FOUND)
  add_test(NAME generated
    COMMAND ${Python3_EXECUTABLE} "${CMAKE_CURRENT_SOURCE_DIR}/../gen/generate.py" --check)
endif()
