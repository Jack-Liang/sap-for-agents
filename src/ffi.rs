//! 底层 C FFI 最小绑定，仅覆盖基础客户端调用
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::os::raw::{c_int, c_void};

// 基础类型映射
pub type SAP_UC = u16; // SAP 宽字符（UTF-16）
pub type RFC_RC = c_int; // 函数返回码
pub type RFC_BYTE = u8; // 原始字节（RFC_BYTE = SAP_RAW = unsigned char）
pub type RFC_CONNECTION_HANDLE = *mut c_void;
pub type RFC_FUNCTION_HANDLE = *mut c_void;
pub type RFC_FUNCTION_DESC_HANDLE = *mut c_void;
pub type RFC_TABLE_HANDLE = *mut c_void;
pub type RFC_STRUCTURE_HANDLE = *mut c_void;

// RFC_RC 返回码常量：值严格对应 nwrfcsdk/include/sapnwrfc.h 中 _RFC_RC 枚举
// （C 枚举从 0 递增）。曾有若干值凭记忆写错（如把 BUFFER_TOO_SMALL 写成 29，
// 实际为 23），导致自适应重读与连接重连判定失效，现逐项对照 SDK 头文件核对。
// 全部用命名常量，避免其它模块再写魔法数字。
pub const RFC_OK: RFC_RC = 0;
pub const RFC_COMMUNICATION_FAILURE: RFC_RC = 1;
pub const RFC_LOGON_FAILURE: RFC_RC = 2;
pub const RFC_ABAP_RUNTIME_FAILURE: RFC_RC = 3; // SYSTEM_FAILURE / shortdump
pub const RFC_ABAP_MESSAGE: RFC_RC = 4;
pub const RFC_ABAP_EXCEPTION: RFC_RC = 5;
pub const RFC_CLOSED: RFC_RC = 6; // 连接被对端/gateway 关闭
#[allow(dead_code)]
pub const RFC_CANCELED: RFC_RC = 7;
pub const RFC_TIMEOUT: RFC_RC = 8;
#[allow(dead_code)]
pub const RFC_MEMORY_INSUFFICIENT: RFC_RC = 9;
#[allow(dead_code)]
pub const RFC_INVALID_HANDLE: RFC_RC = 13;
pub const RFC_RETRY: RFC_RC = 14; // RfcListenAndDispatch 超时期间无入站调用
pub const RFC_EXTERNAL_FAILURE: RFC_RC = 15; // 外部回调失败 → 回传 SYSTEM_FAILURE
pub const RFC_NOT_FOUND: RFC_RC = 17; // 函数/结构定义不存在（Metadata API）
pub const RFC_INVALID_PARAMETER: RFC_RC = 20;
pub const RFC_CONVERSION_FAILURE: RFC_RC = 22; // 类型转换失败（连接仍健康，不应触发重连）
/// 缓冲区过小：调用方传的 max_len 不足，actual_len 给出真实长度。
/// 用于 get_chars/get_xstring 的自适应重试逻辑。
pub const RFC_BUFFER_TOO_SMALL: RFC_RC = 23;
pub const RFC_AUTHORIZATION_FAILURE: RFC_RC = 29;

// RFC_DIRECTION 位掩码（用于注册函数参数时声明方向）
pub const RFC_DIRECTION_IMPORT: c_int = 0x01;
pub const RFC_DIRECTION_EXPORT: c_int = 0x02;
pub const RFC_DIRECTION_CHANGING: c_int = 0x03; // IMPORT | EXPORT
pub const RFC_DIRECTION_TABLES: c_int = 0x07; // 0x04 | CHANGING

// ABAP 数据类型（节选自 sapnwrfc.h RFCTYPE 枚举）
// 公开为 pub mod，供 server_config 等模块按名引用。
#[allow(dead_code)]
pub mod rfctype {
    pub const CHAR: i32 = 0;
    pub const DATE: i32 = 1;
    pub const BCD: i32 = 2;
    pub const TIME: i32 = 3;
    pub const BYTE: i32 = 4;
    pub const TABLE: i32 = 5;
    pub const NUM: i32 = 6;
    pub const FLOAT: i32 = 7;
    pub const INT: i32 = 8;
    pub const INT2: i32 = 9;
    pub const INT1: i32 = 10;
    pub const STRUCTURE: i32 = 17;
    pub const STRING: i32 = 29;
    pub const XSTRING: i32 = 30;
    pub const INT8: i32 = 31;
}
pub use rfctype::{CHAR as RFCTYPE_CHAR, STRUCTURE as RFCTYPE_STRUCTURE, TABLE as RFCTYPE_TABLE};

/// 参数名/字段名（RFC_ABAP_NAME = RFC_CHAR[30+1]，0 终止）
pub type RFC_ABAP_NAME = [SAP_UC; 31];
/// 参数默认值
pub type RFC_PARAMETER_DEFVALUE = [SAP_UC; 31];
/// 参数描述文本（RFC_PARAMETER_TEXT = RFC_CHAR[79+1]）
pub type RFC_PARAMETER_TEXT = [SAP_UC; 80];

// 连接参数结构体
#[repr(C)]
pub struct RFC_CONNECTION_PARAMETER {
    pub name: *const SAP_UC,
    pub value: *const SAP_UC,
}

// 错误信息结构体（严格对齐 sapnwrfc.h 的 _RFC_ERROR_INFO）
// 注意每个 abap_msg_xxx 字段都含 +1 的 null 终止符位
#[repr(C)]
pub struct RFC_ERROR_INFO {
    pub code: RFC_RC,                 // RFC_RC (int)
    pub group: c_int,                 // RFC_ERROR_GROUP (enum = int)
    pub key: [SAP_UC; 128],           // SAP_UC key[128]
    pub message: [SAP_UC; 512],       // SAP_UC message[512]
    pub abap_msg_class: [SAP_UC; 21], // SAP_UC abapMsgClass[20+1]
    pub abap_msg_type: [SAP_UC; 2],   // SAP_UC abapMsgType[1+1]
    pub abap_msg_number: [SAP_UC; 4], // RFC_NUM abapMsgNumber[3+1]（RFC_NUM=SAP_UC）
    pub abap_msg_v1: [SAP_UC; 51],    // SAP_UC abapMsgV1[50+1]
    pub abap_msg_v2: [SAP_UC; 51],    // SAP_UC abapMsgV2[50+1]
    pub abap_msg_v3: [SAP_UC; 51],    // SAP_UC abapMsgV3[50+1]
    pub abap_msg_v4: [SAP_UC; 51],    // SAP_UC abapMsgV4[50+1]
}

// Windows NWRFC SDK 使用 _stdcall 调用约定，等同于 "system"（Windows 上 stdcall = system）
extern "system" {
    // 连接管理
    pub fn RfcOpenConnection(
        connectionParams: *const RFC_CONNECTION_PARAMETER,
        paramCount: c_int,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_CONNECTION_HANDLE;

    pub fn RfcCloseConnection(
        connection: RFC_CONNECTION_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 函数元数据与实例
    pub fn RfcGetFunctionDesc(
        connection: RFC_CONNECTION_HANDLE,
        funcName: *const SAP_UC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_FUNCTION_DESC_HANDLE;

    pub fn RfcCreateFunction(
        funcDesc: RFC_FUNCTION_DESC_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_FUNCTION_HANDLE;

    pub fn RfcDestroyFunction(
        funcHandle: RFC_FUNCTION_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 字符串 / 整数读写（dataHandle 可为函数或结构体）
    pub fn RfcSetString(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        value: *const SAP_UC,
        valueLen: c_int,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetString(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        value: *mut SAP_UC,
        maxLen: c_int,
        actualLen: *mut c_int,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcSetInt(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        value: c_int,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetInt(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        value: *mut c_int,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 浮点（RFC_FLOAT = double）
    pub fn RfcSetFloat(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        value: f64,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetFloat(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        value: *mut f64,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 8 字节整数（RFC_INT8 = long long）
    pub fn RfcSetInt8(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        value: i64,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetInt8(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        value: *mut i64,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 二进制变长（XSTRING）：SAP_RAW = unsigned char
    pub fn RfcSetXString(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        byteValue: *const u8,
        valueLength: u32,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetXString(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        byteBuffer: *mut u8,
        bufferLength: u32,
        xstringLength: *mut u32,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 执行远程调用
    pub fn RfcInvoke(
        connection: RFC_CONNECTION_HANDLE,
        funcHandle: RFC_FUNCTION_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 表操作
    pub fn RfcGetTable(
        funcHandle: RFC_FUNCTION_HANDLE,
        name: *const SAP_UC,
        tableHandle: *mut RFC_TABLE_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 结构体参数：从函数取一个结构体参数的句柄（与表行句柄同类型 RFC_STRUCTURE_HANDLE）
    pub fn RfcGetStructure(
        dataHandle: *mut c_void,
        name: *const SAP_UC,
        structHandle: *mut RFC_STRUCTURE_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetRowCount(
        tableHandle: RFC_TABLE_HANDLE,
        rowCount: *mut u32,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcMoveTo(
        tableHandle: RFC_TABLE_HANDLE,
        index: u32,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetCurrentRow(
        tableHandle: RFC_TABLE_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_STRUCTURE_HANDLE;

    pub fn RfcAppendNewRow(
        tableHandle: RFC_TABLE_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_STRUCTURE_HANDLE;

    // 函数元数据
    pub fn RfcGetParameterCount(
        funcDesc: RFC_FUNCTION_DESC_HANDLE,
        count: *mut u32,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetParameterDescByIndex(
        funcDesc: RFC_FUNCTION_DESC_HANDLE,
        index: u32,
        paramDesc: *mut RFC_PARAMETER_DESC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    // 结构体字段元数据（用于读取表行字段的精确长度）
    pub fn RfcGetFieldDescByIndex(
        typeHandle: RFC_TYPE_DESC_HANDLE,
        index: u32,
        fieldDesc: *mut RFC_FIELD_DESC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    pub fn RfcGetFieldCount(
        typeHandle: RFC_TYPE_DESC_HANDLE,
        count: *mut u32,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    /// 按精确名查 DDIC 结构/表/类型的描述符（用于查数据字典字段）。
    /// 返回的 typeHandle 可传给 RfcGetFieldCount/RfcGetFieldDescByIndex 读字段。
    /// 失败时 errorInfo->code != RFC_OK，且可能返回 null handle。
    pub fn RfcGetTypeDesc(
        rfcHandle: RFC_CONNECTION_HANDLE,
        typeName: *const SAP_UC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_TYPE_DESC_HANDLE;

    // ============ Server 端 API（被 SAP 回调）============

    /// 注册到 SAP Gateway，返回 server connection handle
    pub fn RfcRegisterServer(
        connectionParams: *const RFC_CONNECTION_PARAMETER,
        paramCount: u32,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_CONNECTION_HANDLE;

    /// 在 server handle 上监听并分发入站 RFC 调用（阻塞，timeout 秒）
    pub fn RfcListenAndDispatch(
        rfcHandle: RFC_CONNECTION_HANDLE,
        timeout: c_int,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    /// 注册回调：把 (sysId, funcDesc) 绑定到 serverFunction。
    /// sysId 传 null 表示对所有系统生效。
    pub fn RfcInstallServerFunction(
        sysId: *const SAP_UC,
        funcDescHandle: RFC_FUNCTION_DESC_HANDLE,
        serverFunction: *const c_void, // RFC_SERVER_FUNCTION 函数指针
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    /// 动态创建函数描述符（用于 server 端定义自己的 Z 函数元数据）
    pub fn RfcCreateFunctionDesc(
        name: *const SAP_UC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_FUNCTION_DESC_HANDLE;

    /// 向函数描述符添加参数定义
    pub fn RfcAddParameter(
        funcDesc: RFC_FUNCTION_DESC_HANDLE,
        paramDescr: *const RFC_PARAMETER_DESC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    /// 删除函数描述符（保留供未来清理用）
    #[allow(dead_code)]
    pub fn RfcDestroyFunctionDesc(
        funcDescHandle: RFC_FUNCTION_DESC_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    /// 按名字查 SDK 仓库缓存的函数描述符（repositoryID = 后端 SYSID），
    /// 不在缓存时返回 null 句柄（配合 Remove 用于精确计数）。
    pub fn RfcGetCachedFunctionDesc(
        repositoryID: *const SAP_UC,
        funcName: *const SAP_UC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_FUNCTION_DESC_HANDLE;

    /// 按名字从 SDK 仓库缓存移除函数描述符（repositoryID = 后端 SYSID）。
    /// SDK 头文件注明的设计场景：后端签名已变、而外部 RFC 程序仍在运行——
    /// 继续用旧缓存会得到缺字段/错值。⚠️ 实测名字不在缓存时也返回 RFC_OK
    /// （不区分"没找到"），精确计数须先用 GetCachedFunctionDesc 判存在。
    pub fn RfcRemoveFunctionDesc(
        repositoryID: *const SAP_UC,
        functionName: *const SAP_UC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    /// 按名字查 SDK 仓库缓存的类型（结构/表）描述符（repositoryID = 后端 SYSID），
    /// 不在缓存时返回 null 句柄。
    pub fn RfcGetCachedTypeDesc(
        repositoryID: *const SAP_UC,
        typeName: *const SAP_UC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_TYPE_DESC_HANDLE;

    /// 按名字从 SDK 仓库缓存移除类型（结构/表）描述符（repositoryID = 后端 SYSID）。
    pub fn RfcRemoveTypeDesc(
        repositoryID: *const SAP_UC,
        typeName: *const SAP_UC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    /// 取函数名（回调里从 funcHandle 反查函数名，用于路由）
    pub fn RfcGetFunctionName(
        funcDesc: RFC_FUNCTION_DESC_HANDLE,
        bufferForName: *mut SAP_UC,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_RC;

    /// 从函数实例句柄取描述符句柄（回调里 funcHandle → funcDesc → name）
    pub fn RfcDescribeFunction(
        funcHandle: RFC_FUNCTION_HANDLE,
        errorInfo: *mut RFC_ERROR_INFO,
    ) -> RFC_FUNCTION_DESC_HANDLE;
}

/// 嵌套类型描述句柄（用于结构体/表字段的子类型）
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RFC_TYPE_DESC_HANDLE {
    pub handle: *mut c_void,
}

/// 参数描述（对齐 sapnwrfc.h 中 _RFC_PARAMETER_DESC 内存布局）
#[repr(C)]
pub struct RFC_PARAMETER_DESC {
    pub name: RFC_ABAP_NAME,
    /// RFCTYPE_* 常量
    pub type_: c_int,
    /// RFC_DIRECTION 位掩码
    pub direction: c_int,
    pub nucLength: u32,
    pub ucLength: u32,
    pub decimals: u32,
    pub typeDescHandle: RFC_TYPE_DESC_HANDLE,
    pub defaultValue: RFC_PARAMETER_DEFVALUE,
    pub parameterText: RFC_PARAMETER_TEXT,
    pub optional: RFC_BYTE,
    pub extendedDescription: *mut c_void,
}

/// 字段描述（对齐 sapnwrfc.h 中 _RFC_FIELD_DESC 内存布局）
#[repr(C)]
pub struct RFC_FIELD_DESC {
    pub name: RFC_ABAP_NAME,
    pub type_: c_int,
    pub nucLength: u32,
    pub nucOffset: u32,
    pub ucLength: u32,
    pub ucOffset: u32,
    pub decimals: u32,
    pub typeDescHandle: RFC_TYPE_DESC_HANDLE,
    pub extendedDescription: *mut c_void,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断言 RFCTYPE 常量值与 SAP NWRFC SDK 头文件枚举一致。
    /// 这些值被 server_config / metadata / api 依赖，错误会导致类型派发全乱。
    #[test]
    fn rfctype_constants_match_sdk_header() {
        assert_eq!(rfctype::CHAR, 0);
        assert_eq!(rfctype::DATE, 1);
        assert_eq!(rfctype::BCD, 2);
        assert_eq!(rfctype::TIME, 3);
        assert_eq!(rfctype::BYTE, 4);
        assert_eq!(rfctype::TABLE, 5);
        assert_eq!(rfctype::NUM, 6);
        assert_eq!(rfctype::FLOAT, 7);
        assert_eq!(rfctype::INT, 8);
        assert_eq!(rfctype::INT2, 9);
        assert_eq!(rfctype::INT1, 10);
        assert_eq!(rfctype::STRUCTURE, 17);
        assert_eq!(rfctype::STRING, 29);
        assert_eq!(rfctype::XSTRING, 30);
        assert_eq!(rfctype::INT8, 31);
    }

    #[test]
    fn direction_constants_are_bitmasks() {
        // RFC_DIRECTION 位掩码（sapnwrfc.h）
        assert_eq!(RFC_DIRECTION_IMPORT, 0x01);
        assert_eq!(RFC_DIRECTION_EXPORT, 0x02);
        assert_eq!(RFC_DIRECTION_CHANGING, 0x03); // IMPORT | EXPORT
        assert_eq!(RFC_DIRECTION_TABLES, 0x07);
    }

    #[test]
    fn rc_constants_match_sdk_enum() {
        // 值取自 nwrfcsdk/include/sapnwrfc.h 的 _RFC_RC 枚举（C 枚举从 0 递增）
        assert_eq!(RFC_OK, 0);
        assert_eq!(RFC_COMMUNICATION_FAILURE, 1);
        assert_eq!(RFC_LOGON_FAILURE, 2);
        assert_eq!(RFC_ABAP_RUNTIME_FAILURE, 3);
        assert_eq!(RFC_ABAP_MESSAGE, 4);
        assert_eq!(RFC_ABAP_EXCEPTION, 5);
        assert_eq!(RFC_CLOSED, 6);
        assert_eq!(RFC_CANCELED, 7);
        assert_eq!(RFC_TIMEOUT, 8);
        assert_eq!(RFC_MEMORY_INSUFFICIENT, 9);
        assert_eq!(RFC_INVALID_HANDLE, 13);
        assert_eq!(RFC_RETRY, 14);
        assert_eq!(RFC_EXTERNAL_FAILURE, 15);
        assert_eq!(RFC_NOT_FOUND, 17);
        assert_eq!(RFC_INVALID_PARAMETER, 20);
        assert_eq!(RFC_CONVERSION_FAILURE, 22);
        assert_eq!(RFC_BUFFER_TOO_SMALL, 23);
        assert_eq!(RFC_AUTHORIZATION_FAILURE, 29);
    }
}
