/* 空文件：TinyUSB 绑定由 pc_comm.rs 手动声明。
 * 此文件阻止 esp-idf-sys bindgen 解析 TinyUSB 头文件
 * (osal_freertos.h 的 taskENTER_CRITICAL 参数不匹配 Xtensa 移植)。
 * 组件仍由 CMake 正常编译链接。 */
