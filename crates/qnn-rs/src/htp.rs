//! HTP power/perf-infrastructure FFI, hand-declared byte-exact from the QAIRT
//! headers `HTP/QnnHtpPerfInfrastructure.h` and `HTP/QnnHtpDevice.h`. These use
//! C++/`bool` constructs excluded from `wrapper.h`, so bindgen never emitted
//! them; the layout is reproduced here and guarded by `size_of`/`offset_of`
//! asserts. All members are 4-byte (`uint32_t` or LP64 `-fno-short-enums` C
//! `enum`); function pointers are 8-byte on the LP64 targets.
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code)]

use crate::bindings::Qnn_ErrorHandle_t;
use std::ffi::c_void;

// QnnHtpPerfInfrastructure_PowerConfigOption_t
pub const QNN_HTP_PERF_INFRASTRUCTURE_POWER_CONFIGOPTION_DCVS_V3: u32 = 1;

// QnnHtpPerfInfrastructure_PowerMode_t
pub const QNN_HTP_PERF_INFRASTRUCTURE_POWERMODE_ADJUST_UP_DOWN: u32 = 0x1;
pub const QNN_HTP_PERF_INFRASTRUCTURE_POWERMODE_PERFORMANCE_MODE: u32 = 0x10;

// QnnHtpPerfInfrastructure_VoltageCorner_t
pub const DCVS_VOLTAGE_CORNER_DISABLE: u32 = 0x10;
pub const DCVS_VOLTAGE_VCORNER_NOM: u32 = 0x60;
pub const DCVS_VOLTAGE_VCORNER_TURBO: u32 = 0x80;
pub const DCVS_VOLTAGE_VCORNER_MAX_VOLTAGE_CORNER: u32 = 0xA0;

// QnnHtpDevice_InfrastructureType_t
pub const QNN_HTP_DEVICE_INFRASTRUCTURE_TYPE_PERF: u32 = 0;

pub const QNN_HTP_PERF_INFRASTRUCTURE_POWER_CONFIGOPTION_RPC_CONTROL_LATENCY: u32 = 2;
pub const QNN_HTP_PERF_INFRASTRUCTURE_POWER_CONFIGOPTION_RPC_POLLING_TIME: u32 = 3;

// QnnHtpContext_ConfigOption_t
pub const QNN_HTP_CONTEXT_CONFIG_OPTION_REGISTER_MULTI_CONTEXTS: u32 = 2;
pub const QNN_HTP_CONTEXT_CONFIG_OPTION_USE_EXTENDED_UDMA: u32 = 11;

// QnnHtpContext_GetPropertyOption_t
pub const QNN_HTP_CONTEXT_GET_PROP_MAX_SPILLFILL_BUFFER_SIZE: u32 = 2;

/// DcvsV3 config; 16 × 4-byte members = 64 bytes (the union ceiling).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QnnHtpPerfInfrastructure_DcvsV3_t {
    pub contextId: u32,
    pub setDcvsEnable: u32,
    pub dcvsEnable: u32,
    pub powerMode: u32,
    pub setSleepLatency: u32,
    pub sleepLatency: u32,
    pub setSleepDisable: u32,
    pub sleepDisable: u32,
    pub setBusParams: u32,
    pub busVoltageCornerMin: u32,
    pub busVoltageCornerTarget: u32,
    pub busVoltageCornerMax: u32,
    pub setCoreParams: u32,
    pub coreVoltageCornerMin: u32,
    pub coreVoltageCornerTarget: u32,
    pub coreVoltageCornerMax: u32,
}

/// Single-member view of the C `PowerConfig_t` union; `dcvsV3Config` is the
/// largest arm (64 bytes) so this fixes the union size byte-exactly.
#[repr(C)]
#[derive(Copy, Clone)]
pub union QnnHtpPerfInfrastructure_PowerConfig_union {
    pub dcvsV3Config: QnnHtpPerfInfrastructure_DcvsV3_t,
    pub rpcControlLatencyConfig: u32,
    pub rpcPollingTimeConfig: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct QnnHtpPerfInfrastructure_PowerConfig_t {
    pub option: u32,
    pub config: QnnHtpPerfInfrastructure_PowerConfig_union,
}

pub type QnnHtpPerfInfrastructure_CreatePowerConfigIdFn_t = Option<
    unsafe extern "C" fn(deviceId: u32, coreId: u32, powerConfigId: *mut u32) -> Qnn_ErrorHandle_t,
>;
pub type QnnHtpPerfInfrastructure_DestroyPowerConfigIdFn_t =
    Option<unsafe extern "C" fn(powerConfigId: u32) -> Qnn_ErrorHandle_t>;
pub type QnnHtpPerfInfrastructure_SetPowerConfigFn_t = Option<
    unsafe extern "C" fn(
        powerConfigId: u32,
        config: *const *const QnnHtpPerfInfrastructure_PowerConfig_t,
    ) -> Qnn_ErrorHandle_t,
>;
pub type QnnHtpPerfInfrastructure_SetMemoryConfigFn_t = Option<
    unsafe extern "C" fn(deviceId: u32, coreId: u32, config: *const *const c_void) -> Qnn_ErrorHandle_t,
>;

/// Function-pointer table returned via `deviceGetInfrastructure` for HTP.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QnnHtpDevice_PerfInfrastructure_t {
    pub createPowerConfigId: QnnHtpPerfInfrastructure_CreatePowerConfigIdFn_t,
    pub destroyPowerConfigId: QnnHtpPerfInfrastructure_DestroyPowerConfigIdFn_t,
    pub setPowerConfig: QnnHtpPerfInfrastructure_SetPowerConfigFn_t,
    pub setMemoryConfig: QnnHtpPerfInfrastructure_SetMemoryConfigFn_t,
}

/// `struct _QnnDevice_Infrastructure_t`; the single-member C union is flattened
/// to `perfInfra`, which repr(C) places at offset 8 (4 bytes pad after enum).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QnnHtpDevice_Infrastructure_t {
    pub infraType: u32,
    pub perfInfra: QnnHtpDevice_PerfInfrastructure_t,
}

/// `QnnHtpContext_GroupRegistration_t`: head handle (0 = start a new group)
/// plus the group's shared spill-fill scratch size in bytes.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QnnHtpContext_GroupRegistration_t {
    pub firstGroupHandle: *mut c_void,
    pub maxSpillFillBuffer: u64,
}

/// The arms of `QnnHtpContext_CustomConfig_t`'s union this crate sets; the
/// 16-byte `groupRegistration` is the largest and forces 8-byte alignment.
#[repr(C)]
#[derive(Copy, Clone)]
pub union QnnHtpContext_CustomConfig_union {
    pub weightSharingEnabled: bool,
    pub groupRegistration: QnnHtpContext_GroupRegistration_t,
    pub useExtendedUdma: bool,
}

/// `QnnHtpContext_CustomConfig_t`; the 4-byte enum is followed by 4 bytes of
/// padding before the 8-byte-aligned union.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QnnHtpContext_CustomConfig_t {
    pub option: u32,
    pub config: QnnHtpContext_CustomConfig_union,
}

/// The `uint64_t` arms of `QnnHtpContext_CustomProperty_t`'s union.
#[repr(C)]
#[derive(Copy, Clone)]
pub union QnnHtpContext_CustomProperty_union {
    pub bufferStartAlignment: u64,
    pub spillfillBufferSize: u64,
}

/// `QnnHtpContext_CustomProperty_t` for `contextGetProperty`; same 4-byte enum
/// plus padding then union layout as the custom config.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QnnHtpContext_CustomProperty_t {
    pub option: u32,
    pub prop: QnnHtpContext_CustomProperty_union,
}

const _: () = assert!(std::mem::size_of::<QnnHtpContext_GroupRegistration_t>() == 16);
const _: () = assert!(std::mem::size_of::<QnnHtpContext_CustomConfig_t>() == 24);
const _: () = assert!(std::mem::offset_of!(QnnHtpContext_CustomConfig_t, config) == 8);
const _: () = assert!(std::mem::size_of::<QnnHtpContext_CustomProperty_t>() == 16);
const _: () = assert!(std::mem::offset_of!(QnnHtpContext_CustomProperty_t, prop) == 8);

const _: () = assert!(std::mem::size_of::<QnnHtpPerfInfrastructure_DcvsV3_t>() == 64);
const _: () = assert!(std::mem::size_of::<QnnHtpPerfInfrastructure_PowerConfig_t>() == 68);
const _: () = assert!(std::mem::size_of::<QnnHtpDevice_PerfInfrastructure_t>() == 32);
const _: () = assert!(std::mem::size_of::<QnnHtpDevice_Infrastructure_t>() == 40);
const _: () = assert!(std::mem::offset_of!(QnnHtpDevice_Infrastructure_t, perfInfra) == 8);
