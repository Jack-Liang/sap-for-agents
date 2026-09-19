use crate::error::{check_rc, RfcError};
use crate::ffi::*;
use crate::function::RfcFunction;
use crate::string_utils::str_to_sap_uc;

/// RFC 连接对象，拥有连接所有权，离开作用域自动关闭
pub struct RfcConnection {
    handle: RFC_CONNECTION_HANDLE,
}

// SAFETY: `RfcConnection` 内部唯一非 Send 成员是裸指针 `RFC_CONNECTION_HANDLE`。
// 在本服务中，所有对连接的访问都被 `Arc<Mutex<RfcConnection>>` 串行化，
// 同一时刻只有一个线程持有连接；SAP NWRFC SDK 允许在「不同线程串行」使用
// 同一连接句柄（不允许并发），因此把它标记为 Send 是 sound 的。
// 注意：仍不可跨 await 点持有 Mutex 守卫，本服务中所有 FFI 调用都通过
// spawn_blocking 在专用阻塞线程执行，从根本上规避跨 await 点问题。
unsafe impl Send for RfcConnection {}

impl RfcConnection {
    /// 通过键值对参数建立直连
    pub fn new(params: &[(&str, &str)]) -> Result<Self, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();

            // 转换所有参数为 SAP 宽字符格式
            let param_owned: Vec<(Vec<u16>, Vec<u16>)> = params
                .iter()
                .map(|(k, v)| (str_to_sap_uc(k), str_to_sap_uc(v)))
                .collect();

            // 构造 SDK 要求的参数数组
            let rfc_params: Vec<RFC_CONNECTION_PARAMETER> = param_owned
                .iter()
                .map(|(k, v)| RFC_CONNECTION_PARAMETER {
                    name: k.as_ptr(),
                    value: v.as_ptr(),
                })
                .collect();

            // 调用 C API 建立连接
            let handle = RfcOpenConnection(
                rfc_params.as_ptr(),
                rfc_params.len() as i32,
                &mut error_info,
            );

            check_rc(error_info.code, &error_info)?;
            // SDK 在异常情况下可能 code==OK 但返回 null 句柄，需防御
            if handle.is_null() {
                return Err(RfcError {
                    code: error_info.code,
                    message: "RfcOpenConnection 返回 null 句柄".into(),
                    key: crate::string_utils::sap_uc_to_string(error_info.key.as_ptr(), 128),
                    ..Default::default()
                });
            }
            Ok(Self { handle })
        }
    }

    /// 获取指定函数的调用对象
    pub fn get_function(&self, func_name: &str) -> Result<RfcFunction, RfcError> {
        unsafe {
            let func_desc = self.get_function_desc_handle(func_name)?;
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let func_handle = RfcCreateFunction(func_desc, &mut error_info);
            check_rc(error_info.code, &error_info)?;
            if func_handle.is_null() {
                return Err(RfcError {
                    code: error_info.code,
                    message: "RfcCreateFunction 返回 null 句柄".into(),
                    ..Default::default()
                });
            }

            Ok(RfcFunction {
                handle: func_handle,
                conn_handle: self.handle,
            })
        }
    }

    /// 拉取函数元数据描述句柄（内部用：get_function 和 get_param_infos 共用）
    unsafe fn get_function_desc_handle(
        &self,
        func_name: &str,
    ) -> Result<RFC_FUNCTION_DESC_HANDLE, RfcError> {
        let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
        let name_uc = str_to_sap_uc(func_name);
        let func_desc = RfcGetFunctionDesc(self.handle, name_uc.as_ptr(), &mut error_info);
        check_rc(error_info.code, &error_info)?;
        if func_desc.is_null() {
            return Err(RfcError {
                code: error_info.code,
                message: format!("RfcGetFunctionDesc 返回 null 句柄 (函数: {})", func_name),
                ..Default::default()
            });
        }
        Ok(func_desc)
    }

    /// 读取函数的所有参数元数据（名称、类型、字符长度、方向、是否表/结构体）。
    /// 用于元数据自动发现，避免调用方手填 max_len。
    pub fn get_param_infos(&self, func_name: &str) -> Result<Vec<ParamInfo>, RfcError> {
        unsafe {
            let func_desc = self.get_function_desc_handle(func_name)?;
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();

            let mut count: u32 = 0;
            let rc = RfcGetParameterCount(func_desc, &mut count, &mut error_info);
            check_rc(rc, &error_info)?;

            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                let mut pdesc: RFC_PARAMETER_DESC = std::mem::zeroed();
                let rc = RfcGetParameterDescByIndex(func_desc, i, &mut pdesc, &mut error_info);
                check_rc(rc, &error_info)?;

                let name = crate::string_utils::sap_uc_to_string(pdesc.name.as_ptr(), 30);
                // ucLength 是 2-byte-per-SAP_CHAR 系统的字节长度，字符长度 = ucLength / 2
                let char_length = pdesc.ucLength / 2;
                let parameter_text =
                    crate::string_utils::sap_uc_to_string(pdesc.parameterText.as_ptr(), 79);
                let default_value =
                    crate::string_utils::sap_uc_to_string(pdesc.defaultValue.as_ptr(), 30);
                out.push(ParamInfo {
                    name,
                    type_: pdesc.type_,
                    char_length: char_length as usize,
                    direction: pdesc.direction,
                    decimals: pdesc.decimals,
                    optional: pdesc.optional != 0,
                    parameter_text,
                    default_value,
                    type_desc_handle: if pdesc.typeDescHandle.handle.is_null() {
                        None
                    } else {
                        Some(pdesc.typeDescHandle)
                    },
                });
            }
            Ok(out)
        }
    }

    /// 按 DDIC 类型名（结构/表/类型，如 MARA、BAPIRETURN）取类型描述符。
    /// 返回的 type_handle 可传给 get_field_infos() 读字段元数据。
    pub fn get_type_desc(
        &self,
        type_name: &str,
    ) -> Result<RFC_TYPE_DESC_HANDLE, RfcError> {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            let name_uc = crate::string_utils::str_to_sap_uc(type_name);
            let type_handle = RfcGetTypeDesc(self.handle, name_uc.as_ptr(), &mut error_info);
            // RfcGetTypeDesc 失败时 code != RFC_OK；成功但返回 null handle 也视为错误
            check_rc(error_info.code, &error_info)?;
            if type_handle.handle.is_null() {
                return Err(RfcError {
                    code: error_info.code,
                    message: format!("DDIC 类型 [{}] 未找到或无字段定义", type_name),
                    key: crate::string_utils::sap_uc_to_string(error_info.key.as_ptr(), 128),
                    ..Default::default()
                });
            }
            Ok(type_handle)
        }
    }

    /// 轻量自检：调用无参标准函数 `RFC_PING`，验证连接对 SAP 仍可达。
    /// 供 readiness 探针（`GET /ready`）使用——无参数读写，开销远小于业务调用。
    /// 失败时若属通信类错误，连接池会自动丢弃该连接并重建。
    pub fn ping(&self) -> Result<(), RfcError> {
        let mut f = self.get_function("RFC_PING")?;
        f.invoke()?;
        Ok(())
    }

    /// 读取对端 SAP 系统信息（sysid / release / host / OS）。
    /// 调用标准远程函数 `RFC_SYSTEM_INFO`（自 R/3 起全系统可用），读其
    /// EXPORT 结构 `RFCSI_EXPORT`（DDIC 结构 RFCSI）。供 `/api/version`
    /// 的 SAP 块使用——结果在网关生命周期内不变，调用方负责缓存。
    pub fn system_info(&self) -> Result<RemoteSystemInfo, RfcError> {
        let mut f = self.get_function("RFC_SYSTEM_INFO")?;
        f.invoke()?;
        let rsci = f.get_structure("RFCSI_EXPORT")?;
        Ok(RemoteSystemInfo {
            sysid: rsci.get_chars("RFCSYSID", 8)?,
            release: rsci.get_chars("RFCSAPRL", 4)?,
            host: rsci.get_chars("RFCHOST", 32)?,
            os: rsci.get_chars("RFCOPSYS", 10)?,
            destination: rsci.get_chars("RFCDEST", 32)?,
        })
    }
}

/// 对端 SAP 系统静态信息（RFCSI 结构的有用子集）。
/// 字段长度为 RFCSI 的 DDIC 长度，仅作初始缓冲；更长值会自适应重读。
#[derive(Debug, Clone)]
pub struct RemoteSystemInfo {
    /// 系统 ID（如 A4H）
    pub sysid: String,
    /// SAP Release（如 "753" / "816"；注意只反映 Basis/S4 内核版本，
    /// 不直接区分 ECC 与 S/4HANA——判定 S/4 可查 CVERS 表的 S4CORE 组件）
    pub release: String,
    /// 应用服务器主机名
    pub host: String,
    /// 操作系统（如 "Linux"）
    pub os: String,
    /// RFC 目标名（如 "vhcala4hci_A4H_00"）
    pub destination: String,
}

/// 单个参数的元数据（Rust 化后的 RFC_PARAMETER_DESC 子集）
#[derive(Clone)]
pub struct ParamInfo {
    pub name: String,
    /// RFCTYPE_* 常量值
    pub type_: i32,
    /// 字符长度（ucLength/2，对 CHAR/NUM/DATE/TIME 等有意义）
    pub char_length: usize,
    /// RFC_DIRECTION 位掩码（import/export/changing/tables）
    pub direction: i32,
    /// 小数位数（对 BCD/FLOAT 有意义）
    pub decimals: u32,
    /// 是否可选（RFC_BYTE，非 0 表示可选）
    pub optional: bool,
    /// 参数描述文本（parameterText，最多 79 字符，常为中文说明）
    pub parameter_text: String,
    /// 参数默认值（defaultValue，最多 30 字符）
    pub default_value: String,
    /// 若为结构体/表，持有其子类型句柄（用于进一步查询字段长度）
    pub type_desc_handle: Option<RFC_TYPE_DESC_HANDLE>,
}

impl std::fmt::Debug for ParamInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParamInfo")
            .field("name", &self.name)
            .field("type_", &self.type_)
            .field("char_length", &self.char_length)
            .field("direction", &self.direction)
            .field("decimals", &self.decimals)
            .field("optional", &self.optional)
            .field("parameter_text", &self.parameter_text)
            .field("default_value", &self.default_value)
            .field("type_desc_handle", &self.type_desc_handle.is_some())
            .finish()
    }
}

/// 读取结构体/表类型的所有字段元数据（用于自动发现表行的字段长度）。
///
/// # Safety
/// `type_handle` 必须是从 ParamInfo 取得的有效句柄。
pub unsafe fn get_field_infos(
    type_handle: RFC_TYPE_DESC_HANDLE,
) -> Result<Vec<ParamInfo>, RfcError> {
    let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
    let mut count: u32 = 0;
    let rc = RfcGetFieldCount(type_handle, &mut count, &mut error_info);
    check_rc(rc, &error_info)?;

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut fdesc: RFC_FIELD_DESC = std::mem::zeroed();
        let rc = RfcGetFieldDescByIndex(type_handle, i, &mut fdesc, &mut error_info);
        check_rc(rc, &error_info)?;
        let name = crate::string_utils::sap_uc_to_string(fdesc.name.as_ptr(), 30);
        let char_length = fdesc.ucLength / 2;
        out.push(ParamInfo {
            name,
            type_: fdesc.type_,
            char_length: char_length as usize,
            direction: 0, // 字段没有方向
            decimals: fdesc.decimals,
            optional: false, // 字段没有 optional 标记
            parameter_text: String::new(), // RFC_FIELD_DESC 无 parameterText
            default_value: String::new(),  // RFC_FIELD_DESC 无 defaultValue
            type_desc_handle: if fdesc.typeDescHandle.handle.is_null() {
                None
            } else {
                Some(fdesc.typeDescHandle)
            },
        });
    }
    Ok(out)
}

/// 析构自动关闭连接，对应 C 中的 RfcCloseConnection
impl Drop for RfcConnection {
    fn drop(&mut self) {
        unsafe {
            let mut error_info = std::mem::zeroed::<RFC_ERROR_INFO>();
            // 忽略关闭错误，避免 drop 中 panic
            let _ = RfcCloseConnection(self.handle, &mut error_info);
        }
    }
}
