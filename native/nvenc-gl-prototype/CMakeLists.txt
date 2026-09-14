cmake_minimum_required(VERSION 3.22)
project(luma_nvenc_gl_prototype LANGUAGES CXX)

add_library(luma_nvenc_gl STATIC nvenc_gl_encoder.cpp)
target_compile_features(luma_nvenc_gl PUBLIC cxx_std_20)
target_include_directories(luma_nvenc_gl PUBLIC ${CMAKE_CURRENT_SOURCE_DIR})
target_link_libraries(luma_nvenc_gl PUBLIC dl)

# The production hook supplies the current EGL/GL context. This target is a
# compile-only API probe; it deliberately does not load into any game.
add_executable(nvenc_gl_api_smoke api_smoke.cpp)
target_link_libraries(nvenc_gl_api_smoke PRIVATE luma_nvenc_gl EGL GL)
