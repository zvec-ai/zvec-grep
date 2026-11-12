# Build the Node binding and the C API against the same zvec targets.
file(GLOB ZG_NODE_SOURCES CONFIGURE_DEPENDS "${ZG_NODE_SOURCE}/src/binding/*.cc")
add_library(zvec_node_binding SHARED ${ZG_NODE_SOURCES})
target_compile_features(zvec_node_binding PRIVATE cxx_std_17)
target_include_directories(zvec_node_binding PRIVATE
    "${ZG_NODE_API}" "${ZG_NODE_HEADERS}" "${CMAKE_SOURCE_DIR}/src")
target_compile_definitions(zvec_node_binding PRIVATE
    NAPI_VERSION=8 NODE_ADDON_API_CPP_EXCEPTIONS NODE_ADDON_API_CPP_EXCEPTIONS_ALL)
target_link_libraries(zvec_node_binding PRIVATE zvec)
if(MSVC)
    target_link_libraries(zvec_node_binding PRIVATE "${ZG_NODE_LIBRARY}" delayimp)
    target_sources(zvec_node_binding PRIVATE "${ZG_NODE_SOURCE}/cmake/exports.def")
endif()
set_property(TARGET zvec_node_binding PROPERTY LINK_LIBRARY_OVERRIDE
    "WHOLE_ARCHIVE,zvec_core,zvec_turbo")
set_target_properties(zvec_node_binding PROPERTIES PREFIX "" SUFFIX ".node"
    LIBRARY_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/lib")
if(APPLE)
    target_link_options(zvec_node_binding PRIVATE "LINKER:-undefined,dynamic_lookup"
        "LINKER:-exported_symbols_list,${ZG_NODE_SOURCE}/cmake/exports.mac"
        "LINKER:-dead_strip")
elseif(UNIX)
    target_link_options(zvec_node_binding PRIVATE
        "LINKER:--version-script=${ZG_NODE_SOURCE}/cmake/exports.map"
        "LINKER:--gc-sections")
endif()

# Generator expressions cover both single- and multi-configuration generators.
file(GENERATE OUTPUT "${CMAKE_BINARY_DIR}/zvec-artifacts-$<CONFIG>.json" CONTENT
    "{\"node\":\"$<TARGET_FILE:zvec_node_binding>\",\"capi\":\"$<TARGET_FILE:zvec_c_api>\",\"linker\":\"$<TARGET_LINKER_FILE:zvec_c_api>\"}\n")
