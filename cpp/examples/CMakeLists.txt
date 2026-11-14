# The examples: one program per source file, each with the session of session.h.
file(GLOB IBX_EXAMPLES CONFIGURE_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/*.cpp")
foreach(source ${IBX_EXAMPLES})
  get_filename_component(name ${source} NAME_WE)
  add_executable(${name} ${source})
  target_link_libraries(${name} PRIVATE ibx)
  ibx_copy_core(${name})
endforeach()
