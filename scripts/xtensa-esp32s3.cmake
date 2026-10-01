# CMake toolchain file for the Xtensa ESP32-S3, with the compiler found on PATH.
set(CMAKE_SYSTEM_NAME Generic)
set(CMAKE_SYSTEM_PROCESSOR xtensa)
set(CMAKE_C_COMPILER xtensa-esp32s3-elf-gcc)
set(CMAKE_ASM_COMPILER xtensa-esp32s3-elf-gcc)
set(CMAKE_AR xtensa-esp32s3-elf-ar)
set(CMAKE_RANLIB xtensa-esp32s3-elf-ranlib)
# A freestanding compiler cannot link a hosted executable, so the compiler
# check has to stop at a static library. (Link tests are what break an
# autotools configure for this target.)
set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)
set(CMAKE_C_FLAGS_INIT "-mlongcalls -ffunction-sections -fdata-sections")
