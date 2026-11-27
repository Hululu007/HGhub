// smbc_* 空实现（C 版，只链进 MPVKit 动态库，不进主程序）。
//
// LGPL 的 Libavformat 编译时开了 SMB 支持，`libsmbclient.o` 引用了
// 22 个 `smbc_*` 符号，但 Libsmbclient 本身是 GPL，不在 MPVKit 的
// LGPL 产品里，所以这些符号没处链。v0.0.124 用 `-Wl,-U` 让它们保持
// 未定义，结果 dyld 启动时就要解析、直接 abort（启动闪退）；v0.0.125
// 起在 Rust 里写了空实现（smbc_stubs.rs）让主程序链接有着落。
//
// v0.0.127 起 MPVKit 改走懒加载：28 个静态库打成一个动态库
// （MPVKit.framework），启动时不加载，选 mpv 播放时才 dlopen。
// dlopen 那一刻（chained fixups 下全部符号立即绑定）这 22 个符号
// 必须有着落，所以空实现直接链进动态库内部（自包含），不依赖主程序
// 导出——主程序里已没有任何东西引用 smbc_*，Rust 版空实现已删掉。
//
// 这里把 22 个符号写成空实现（返回错误/NULL），让动态库链接有着落。
// 正常使用永远调不到它们——本应用只播网盘直链，不打开 smb:// 链接，
// smb:// 播放不支持。不要为此链 GPL 的 Libsmbclient。
//
// 签名按 libsmbclient.h 的公开 API 写（不透明指针一律 void*），只求
// 链接时名字对上，永远不被调用所以不必精确到结构体内。

#include <stddef.h>
#include <sys/types.h>

void *smbc_new_context(void) { return NULL; }
void *smbc_init_context(void *context) { (void)context; return NULL; }
void *smbc_init(void) { return NULL; }
int smbc_free_context(void *context, int shutdown) { (void)context; (void)shutdown; return -1; }
int smbc_set_context(void *context) { (void)context; return -1; }
int smbc_open(const char *fname, int flags, unsigned int mode) { (void)fname; (void)flags; (void)mode; return -1; }
int smbc_close(int fd) { (void)fd; return -1; }
ssize_t smbc_read(int fd, void *buf, size_t count) { (void)fd; (void)buf; (void)count; return -1; }
ssize_t smbc_write(int fd, const void *buf, size_t count) { (void)fd; (void)buf; (void)count; return -1; }
off_t smbc_lseek(int fd, off_t offset, int whence) { (void)fd; (void)offset; (void)whence; return -1; }
int smbc_fstat(int fd, void *st) { (void)fd; (void)st; return -1; }
int smbc_stat(const char *path, void *st) { (void)path; (void)st; return -1; }
int smbc_unlink(const char *path) { (void)path; return -1; }
int smbc_rename(const char *oldpath, const char *newpath) { (void)oldpath; (void)newpath; return -1; }
int smbc_rmdir(const char *path) { (void)path; return -1; }
void *smbc_opendir(const char *path) { (void)path; return NULL; }
int smbc_closedir(void *dir) { (void)dir; return -1; }
void *smbc_readdir(void *dir) { (void)dir; return NULL; }
void smbc_setFunctionAuthDataWithContext(void *c, void *func) { (void)c; (void)func; }
void smbc_setOptionUserData(void *c, void *user_data) { (void)c; (void)user_data; }
void smbc_setTimeout(void *c, int timeout) { (void)c; (void)timeout; }
void smbc_setWorkgroup(void *c, const char *workgroup) { (void)c; (void)workgroup; }
