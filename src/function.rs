//! RFC 函数/表/结构体的参数读写封装。
//!
//! 注：部分 getter（RfcRow 的 get_float/get_xstring）当前主流程未直接调用
//! （表输出统一按字符串读），但作为公共 API 保留，供未来按类型读取表字段或
//! 外部库直接使用。

#![allow(dead_code)]
use crate::error::{check_rc, RfcError};
use crate::ffi::*;
use crate::string_utils::{sap_uc_to_string, str_to_sap_uc};
use std::os::raw::{c_int, c_void};

/// 自适应字符串读取：先用 `max_len` 试读，若 SDK 报 `RFC_BUFFER_TOO_SMALL`，
/// 则用返回的 `actual_len` 重新分配缓冲区再读一次，避免调用方必须精确预估长度。
///
/// # Safety
/// `data_handle` 必须是有效的函数句柄或结构体句柄；`name_uc` 必须为 0 结尾的 SAP UC。
pub(crate) unsafe fn read_string_adaptive(
    data_handle: *mut c_void,
    name_uc: *const SAP_UC,
    max_len: usize,
) -> Result<String, RfcError> {
    let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
    let mut buf = vec![0u16; max_len + 1];
    let mut actual_len: c_int = 0;

    let rc = RfcGetString(
        data_handle,
        name_uc,
        buf.as_mut_ptr(),
        buf.len() as c_int,
        &mut actual_len,
        &mut error_info,
    );

    if rc == RFC_BUFFER_TOO_SMALL && actual_len > 0 {
        // 真实长度比预估大：按 actual_len 重新分配，再读一次
        let mut big = vec![0u16; actual_len as usize + 1];
        let mut error_info2 = std::mem::zeroed::<RFC_ERROR_INFO>();
        let mut actual_len2: c_int = 0;
        let rc2 = RfcGetString(
            data_handle,
            name_uc,
            big.as_mut_ptr(),
            big.len() as c_int,
            &mut actual_len2,
            &mut error_info2,
        );
        check_rc(rc2, &error_info2)?;
        return Ok(sap_uc_to_string(big.as_ptr(), actual_len2 as usize));
    }

    check_rc(rc, &error_info)?;
    Ok(sap_uc_to_string(buf.as_ptr(), actual_len as usize))
}

/// 读取浮点字段（RFC_FLOAT = double）。
///
/// # Safety
/// `data_handle` 必须有效；`name_uc` 为 0 结尾 SAP UC。
pub(crate) unsafe fn read_float(
    data_handle: *mut c_void,
    name_uc: *const SAP_UC,
) -> Result<f64, RfcError> {
    let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
    let mut value: f64 = 0.0;
    let rc = RfcGetFloat(data_handle, name_uc, &mut value, &mut error_info);
    check_rc(rc, &error_info)?;
    Ok(value)
}

/// 自适应二进制读取：先用预估容量试读，过小则按真实长度重读。
/// 返回原始字节。调用方自行决定如何编码（本项目统一 Base64）。
unsafe fn read_xstring_adaptive(
    data_handle: *mut c_void,
    name_uc: *const SAP_UC,
    initial_capacity: usize,
) -> Result<Vec<u8>, RfcError> {
    let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
    let mut buf = vec![0u8; initial_capacity.max(64)];
    let mut actual_len: u32 = 0;

    let rc = RfcGetXString(
        data_handle,
        name_uc,
        buf.as_mut_ptr(),
        buf.len() as u32,
        &mut actual_len,
        &mut error_info,
    );

    if rc == RFC_BUFFER_TOO_SMALL && actual_len > 0 {
        let mut big = vec![0u8; actual_len as usize];
        let mut error_info2 = std::mem::zeroed::<RFC_ERROR_INFO>();
        let mut actual_len2: u32 = 0;
        let rc2 = RfcGetXString(
            data_handle,
            name_uc,
            big.as_mut_ptr(),
            big.len() as u32,
            &mut actual_len2,
            &mut error_info2,
        );
        check_rc(rc2, &error_info2)?;
        big.truncate(actual_len2 as usize);
        return Ok(big);
    }

    check_rc(rc, &error_info)?;
    buf.truncate(actual_len as usize);
    Ok(buf)
}

/// RFC 函数调用对象，离开作用域自动销毁
pub struct RfcFunction {
    pub(crate) handle: RFC_FUNCTION_HANDLE,
    pub(crate) conn_handle: RFC_CONNECTION_HANDLE,
}

/// RFC 表行（结构体句柄）
pub struct RfcRow {
    handle: RFC_STRUCTURE_HANDLE,
}

/// RFC 表参数
pub struct RfcTable {
    handle: RFC_TABLE_HANDLE,
}

impl RfcFunction {
    /// 设置字符串类型输入参数
    pub fn set_chars(&mut self, param_name: &str, value: &str) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let value_uc = str_to_sap_uc(value);

            let rc = RfcSetString(
                self.handle,
                name_uc.as_ptr(),
                value_uc.as_ptr(),
                // 长度单位是 UTF-16 代码单元：非 BMP 字符（部分 HR 人名生僻字）
                // 占 2 个单元，chars().count() 会少算一半导致静默截断。
                value.encode_utf16().count() as c_int,
                &mut error_info,
            );
            check_rc(rc, &error_info)
        }
    }

    /// 执行远程函数调用
    pub fn invoke(&mut self) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let rc = RfcInvoke(self.conn_handle, self.handle, &mut error_info);
            check_rc(rc, &error_info)
        }
    }

    /// 读取字符串类型输出参数。
    /// `max_len` 仅为初始缓冲区大小；若 SAP 返回更长，会自动按真实长度重读。
    pub fn get_chars(&mut self, param_name: &str, max_len: usize) -> Result<String, RfcError> {
        unsafe {
            let name_uc = str_to_sap_uc(param_name);
            read_string_adaptive(self.handle, name_uc.as_ptr(), max_len)
        }
    }

    /// 设置整数类型输入参数
    pub fn set_int(&mut self, param_name: &str, value: i32) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let rc = RfcSetInt(self.handle, name_uc.as_ptr(), value, &mut error_info);
            check_rc(rc, &error_info)
        }
    }

    /// 读取整数类型输出参数
    pub fn get_int(&mut self, param_name: &str) -> Result<i32, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let mut value: c_int = 0;
            let rc = RfcGetInt(self.handle, name_uc.as_ptr(), &mut value, &mut error_info);
            check_rc(rc, &error_info)?;
            Ok(value)
        }
    }

    /// 设置浮点参数（RFC_FLOAT）
    pub fn set_float(&mut self, param_name: &str, value: f64) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let rc = RfcSetFloat(self.handle, name_uc.as_ptr(), value, &mut error_info);
            check_rc(rc, &error_info)
        }
    }

    /// 读取浮点参数
    pub fn get_float(&mut self, param_name: &str) -> Result<f64, RfcError> {
        unsafe {
            let name_uc = str_to_sap_uc(param_name);
            read_float(self.handle, name_uc.as_ptr())
        }
    }

    /// 设置 8 字节整数参数（RFC_INT8）
    pub fn set_int8(&mut self, param_name: &str, value: i64) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let rc = RfcSetInt8(self.handle, name_uc.as_ptr(), value, &mut error_info);
            check_rc(rc, &error_info)
        }
    }

    /// 读取 8 字节整数参数（RFC_INT8）
    pub fn get_int8(&mut self, param_name: &str) -> Result<i64, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let mut value: i64 = 0;
            let rc = RfcGetInt8(self.handle, name_uc.as_ptr(), &mut value, &mut error_info);
            check_rc(rc, &error_info)?;
            Ok(value)
        }
    }

    /// 设置二进制参数（XSTRING/BYTE），传入原始字节
    pub fn set_xstring(&mut self, param_name: &str, bytes: &[u8]) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let rc = RfcSetXString(
                self.handle,
                name_uc.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut error_info,
            );
            check_rc(rc, &error_info)
        }
    }

    /// 读取二进制参数，返回原始字节
    pub fn get_xstring(
        &mut self,
        param_name: &str,
        initial_capacity: usize,
    ) -> Result<Vec<u8>, RfcError> {
        unsafe {
            let name_uc = str_to_sap_uc(param_name);
            read_xstring_adaptive(self.handle, name_uc.as_ptr(), initial_capacity)
        }
    }

    /// 获取表参数句柄
    pub fn get_table(&mut self, param_name: &str) -> Result<RfcTable, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let mut table_handle: RFC_TABLE_HANDLE = std::ptr::null_mut();
            let rc = RfcGetTable(
                self.handle,
                name_uc.as_ptr(),
                &mut table_handle,
                &mut error_info,
            );
            check_rc(rc, &error_info)?;
            Ok(RfcTable {
                handle: table_handle,
            })
        }
    }

    /// 获取顶层结构体参数句柄，包成 RfcRow 返回。
    /// 结构体句柄与表行句柄同类型（RFC_STRUCTURE_HANDLE），复用 RfcRow 的所有 set/get 方法。
    pub fn get_structure(&mut self, param_name: &str) -> Result<RfcRow, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(param_name);
            let mut struct_handle: RFC_STRUCTURE_HANDLE = std::ptr::null_mut();
            let rc = RfcGetStructure(
                self.handle,
                name_uc.as_ptr(),
                &mut struct_handle,
                &mut error_info,
            );
            check_rc(rc, &error_info)?;
            Ok(RfcRow {
                handle: struct_handle,
            })
        }
    }
}

impl RfcTable {
    /// 获取表行数
    pub fn row_count(&self) -> Result<u32, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let mut count: u32 = 0;
            let rc = RfcGetRowCount(self.handle, &mut count, &mut error_info);
            check_rc(rc, &error_info)?;
            Ok(count)
        }
    }

    /// 追加一行，返回该行用于设置字段值
    pub fn append_row(&mut self) -> Result<RfcRow, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let row = RfcAppendNewRow(self.handle, &mut error_info);
            check_rc(error_info.code, &error_info)?;
            if row.is_null() {
                return Err(RfcError {
                    code: error_info.code,
                    message: "RfcAppendNewRow 返回 null 句柄".into(),
                    ..Default::default()
                });
            }
            Ok(RfcRow { handle: row })
        }
    }

    /// 将游标移动到指定行（0-based），然后取该行
    pub fn get_row(&self, index: u32) -> Result<RfcRow, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let rc = RfcMoveTo(self.handle, index, &mut error_info);
            check_rc(rc, &error_info)?;
            let row = RfcGetCurrentRow(self.handle, &mut error_info);
            check_rc(error_info.code, &error_info)?;
            if row.is_null() {
                return Err(RfcError {
                    code: error_info.code,
                    message: format!("RfcGetCurrentRow 返回 null 句柄 (index: {})", index),
                    ..Default::default()
                });
            }
            Ok(RfcRow { handle: row })
        }
    }
}

impl RfcRow {
    /// 设置当前行的字符串字段
    pub fn set_chars(&self, field_name: &str, value: &str) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(field_name);
            let value_uc = str_to_sap_uc(value);
            let rc = RfcSetString(
                self.handle,
                name_uc.as_ptr(),
                value_uc.as_ptr(),
                // 长度单位是 UTF-16 代码单元：非 BMP 字符（部分 HR 人名生僻字）
                // 占 2 个单元，chars().count() 会少算一半导致静默截断。
                value.encode_utf16().count() as c_int,
                &mut error_info,
            );
            check_rc(rc, &error_info)
        }
    }

    /// 读取当前行的字符串字段。
    /// `max_len` 仅为初始缓冲区大小；若 SAP 返回更长，会自动按真实长度重读。
    pub fn get_chars(&self, field_name: &str, max_len: usize) -> Result<String, RfcError> {
        unsafe {
            let name_uc = str_to_sap_uc(field_name);
            read_string_adaptive(self.handle, name_uc.as_ptr(), max_len)
        }
    }

    /// 设置当前行的整数字段
    pub fn set_int(&self, field_name: &str, value: i32) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(field_name);
            let rc = RfcSetInt(self.handle, name_uc.as_ptr(), value, &mut error_info);
            check_rc(rc, &error_info)
        }
    }

    /// 读取当前行的整数字段（INT/INT2/INT1 均归到 i32）
    pub fn get_int(&self, field_name: &str) -> Result<i32, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(field_name);
            let mut value: c_int = 0;
            let rc = RfcGetInt(self.handle, name_uc.as_ptr(), &mut value, &mut error_info);
            check_rc(rc, &error_info)?;
            Ok(value)
        }
    }

    /// 设置当前行的浮点字段
    pub fn set_float(&self, field_name: &str, value: f64) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(field_name);
            let rc = RfcSetFloat(self.handle, name_uc.as_ptr(), value, &mut error_info);
            check_rc(rc, &error_info)
        }
    }

    /// 读取当前行的浮点字段
    pub fn get_float(&self, field_name: &str) -> Result<f64, RfcError> {
        unsafe {
            let name_uc = str_to_sap_uc(field_name);
            read_float(self.handle, name_uc.as_ptr())
        }
    }

    /// 设置当前行的 8 字节整数字段
    pub fn set_int8(&self, field_name: &str, value: i64) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(field_name);
            let rc = RfcSetInt8(self.handle, name_uc.as_ptr(), value, &mut error_info);
            check_rc(rc, &error_info)
        }
    }

    /// 读取当前行的 8 字节整数字段（INT8）
    pub fn get_int8(&self, field_name: &str) -> Result<i64, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(field_name);
            let mut value: i64 = 0;
            let rc = RfcGetInt8(self.handle, name_uc.as_ptr(), &mut value, &mut error_info);
            check_rc(rc, &error_info)?;
            Ok(value)
        }
    }

    /// 设置当前行的二进制字段
    pub fn set_xstring(&self, field_name: &str, bytes: &[u8]) -> Result<(), RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = str_to_sap_uc(field_name);
            let rc = RfcSetXString(
                self.handle,
                name_uc.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut error_info,
            );
            check_rc(rc, &error_info)
        }
    }

    /// 读取当前行的二进制字段
    pub fn get_xstring(
        &self,
        field_name: &str,
        initial_capacity: usize,
    ) -> Result<Vec<u8>, RfcError> {
        unsafe {
            let name_uc = str_to_sap_uc(field_name);
            read_xstring_adaptive(self.handle, name_uc.as_ptr(), initial_capacity)
        }
    }
}

/// 析构自动销毁函数对象，释放内存
impl Drop for RfcFunction {
    fn drop(&mut self) {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let _ = RfcDestroyFunction(self.handle, &mut error_info);
        }
    }
}

/// 标量字段读取抽象：让 `read_scalar_by_type` 能同时用于函数参数和表行字段。
/// RfcFunction（&mut self）和 RfcRow（&self）的接收者不同，故用 `&mut Self` 统一。
pub trait ScalarReader {
    fn read_chars(&mut self, name: &str, max_len: usize) -> Result<String, RfcError>;
    fn read_int(&mut self, name: &str) -> Result<i32, RfcError>;
    fn read_int8(&mut self, name: &str) -> Result<i64, RfcError>;
    fn read_float(&mut self, name: &str) -> Result<f64, RfcError>;
    fn read_xstring(&mut self, name: &str, cap: usize) -> Result<Vec<u8>, RfcError>;
}

impl ScalarReader for RfcFunction {
    fn read_chars(&mut self, name: &str, max_len: usize) -> Result<String, RfcError> {
        self.get_chars(name, max_len)
    }
    fn read_int(&mut self, name: &str) -> Result<i32, RfcError> {
        self.get_int(name)
    }
    fn read_int8(&mut self, name: &str) -> Result<i64, RfcError> {
        self.get_int8(name)
    }
    fn read_float(&mut self, name: &str) -> Result<f64, RfcError> {
        self.get_float(name)
    }
    fn read_xstring(&mut self, name: &str, cap: usize) -> Result<Vec<u8>, RfcError> {
        self.get_xstring(name, cap)
    }
}

impl ScalarReader for RfcRow {
    fn read_chars(&mut self, name: &str, max_len: usize) -> Result<String, RfcError> {
        self.get_chars(name, max_len)
    }
    fn read_int(&mut self, name: &str) -> Result<i32, RfcError> {
        self.get_int(name)
    }
    fn read_int8(&mut self, name: &str) -> Result<i64, RfcError> {
        self.get_int8(name)
    }
    fn read_float(&mut self, name: &str) -> Result<f64, RfcError> {
        self.get_float(name)
    }
    fn read_xstring(&mut self, name: &str, cap: usize) -> Result<Vec<u8>, RfcError> {
        self.get_xstring(name, cap)
    }
}
