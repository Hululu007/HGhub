//! smbc_* 空实现（只在 iOS 编译）。
//!
//! LGPL 的 Libavformat 编译时开了 SMB 支持，`libsmbclient.o` 引用了
//! 22 个 `smbc_*` 符号，但 Libsmbclient 本身是 GPL，不在 MPVKit 的
//! LGPL 产品里，所以这些符号没处链。v0.0.124 用 `-Wl,-U` 让它们保持
//! 未定义，结果 dyld 启动时就要解析、报 Symbol not found、直接 abort，
//! 连 main 都进不去（启动闪退）。
//!
//! 这里把 22 个符号写成空实现（返回错误/NULL），让链接时符号有着落。
//! 正常使用永远调不到它们——本应用只播网盘直链，不打开 smb:// 链接，
//! smb:// 播放不支持。不要为此链 GPL 的 Libsmbclient。
//!
//! `#[no_mangle] pub extern "C"`：符号对外可见，LTO 不会当死代码删掉；
//! 删了就又回到 124 的启动闪退。不要去掉。签名按 libsmbclient.h 的
//! 公开 API 写（不透明指针一律 `*mut c_void`），只求链接时名字对上，
//! 永远不被调用所以不必精确到结构体内。

use std::ffi::{c_char, c_int, c_uint, c_void};

type SmbcCtx = *mut c_void;
type SmbcFile = *mut c_void;
type SmbcDirent = *mut c_void;
type Stat = *mut c_void;
type AuthDataFn = *mut c_void;

#[no_mangle]
pub extern "C" fn smbc_new_context() -> SmbcCtx {
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn smbc_init_context(_context: SmbcCtx) -> SmbcCtx {
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn smbc_init() -> SmbcCtx {
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn smbc_free_context(_context: SmbcCtx, _shutdown: c_int) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_set_context(_context: SmbcCtx) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_open(_fname: *const c_char, _flags: c_int, _mode: c_uint) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_close(_fd: c_int) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_read(_fd: c_int, _buf: *mut c_void, _count: usize) -> i64 {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_write(_fd: c_int, _buf: *const c_void, _count: usize) -> i64 {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_lseek(_fd: c_int, _offset: i64, _whence: c_int) -> i64 {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_fstat(_fd: c_int, _st: Stat) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_stat(_path: *const c_char, _st: Stat) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_unlink(_path: *const c_char) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_rename(_oldpath: *const c_char, _newpath: *const c_char) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_rmdir(_path: *const c_char) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_opendir(_path: *const c_char) -> SmbcFile {
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn smbc_closedir(_dir: SmbcFile) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn smbc_readdir(_dir: SmbcFile) -> SmbcDirent {
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn smbc_setFunctionAuthDataWithContext(_c: SmbcCtx, _func: AuthDataFn) {}

#[no_mangle]
pub extern "C" fn smbc_setOptionUserData(_c: SmbcCtx, _user_data: *mut c_void) {}

#[no_mangle]
pub extern "C" fn smbc_setTimeout(_c: SmbcCtx, _timeout: c_int) {}

#[no_mangle]
pub extern "C" fn smbc_setWorkgroup(_c: SmbcCtx, _workgroup: *const c_char) {}
