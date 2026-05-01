// 注意：本文件已被 elfRadio 项目修改。
// 原版用了 inline_const_pat unstable feature 在 ATT/HCI/L2C/SM_US/SM_PEER/HW 错误描述的 macro 里，
// ESP rustc 1.92-nightly 不再识别该 feature。简化处理：保留 BLEError struct + 基础 BLE_HS_E* 错误码描述，
// 移除所有 BASE+offset 类描述（功能不影响，只是日志描述更简略）。

use core::num::NonZeroI32;
use esp_idf_svc::sys;

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct BLEError(NonZeroI32);

impl BLEError {
  pub fn fail() -> Result<(), Self> {
    Self::convert(0xFFFF)
  }

  pub const fn from_non_zero(error: NonZeroI32) -> Self {
    Self(error)
  }

  pub fn check_and_return<T>(error: u32, value: T) -> Result<T, Self> {
    match error {
      0 | sys::BLE_HS_EALREADY | sys::BLE_HS_EDONE => Ok(value),
      error => Err(Self(unsafe { NonZeroI32::new_unchecked(error as _) })),
    }
  }

  pub const fn convert(error: u32) -> Result<(), Self> {
    match error {
      0 | sys::BLE_HS_EALREADY | sys::BLE_HS_EDONE => Ok(()),
      error => Err(Self(unsafe { NonZeroI32::new_unchecked(error as _) })),
    }
  }

  pub fn code(&self) -> u32 {
    self.0.get() as _
  }
}

impl core::fmt::Display for BLEError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match return_code_to_string(self.0.get()) {
      Some(text) => write!(f, "{text}")?,
      None => write!(f, "0x{:X}", self.0)?,
    };

    Ok(())
  }
}

impl core::fmt::Debug for BLEError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match return_code_to_string(self.0.get()) {
      Some(text) => write!(f, "{text}")?,
      None => write!(f, "0x{:X}", self.0)?,
    };

    Ok(())
  }
}

#[cfg(feature = "std")]
impl std::error::Error for BLEError {}

pub fn return_code_to_string(rc: i32) -> Option<&'static str> {
  match rc as u32 {
    sys::BLE_HS_EALREADY => Some("Operation already in progress or completed."),
    sys::BLE_HS_EINVAL => Some("One or more arguments are invalid."),
    sys::BLE_HS_EMSGSIZE => Some("The provided buffer is too small."),
    sys::BLE_HS_ENOENT => Some("No entry matching the specified criteria."),
    sys::BLE_HS_ENOMEM => Some("Operation failed due to resource exhaustion."),
    sys::BLE_HS_ENOTCONN => Some("No open connection with the specified handle."),
    sys::BLE_HS_ENOTSUP => Some("Operation disabled at compile time."),
    sys::BLE_HS_EAPP => Some("Application callback behaved unexpectedly."),
    sys::BLE_HS_EBADDATA => Some("Command from peer is invalid."),
    sys::BLE_HS_EOS => Some("Mynewt OS error."),
    sys::BLE_HS_ECONTROLLER => Some("Event from controller is invalid."),
    sys::BLE_HS_ETIMEOUT => Some("Operation timed out."),
    sys::BLE_HS_EDONE => Some("Operation completed successfully."),
    sys::BLE_HS_EBUSY => Some("Operation cannot be performed until procedure completes."),
    sys::BLE_HS_EREJECT => Some("Peer rejected a connection parameter update request."),
    sys::BLE_HS_EUNKNOWN => Some("Unexpected failure; catch all."),
    sys::BLE_HS_EROLE => Some("Operation requires different role (e.g., central vs. peripheral)."),
    sys::BLE_HS_ETIMEOUT_HCI => Some("HCI request timed out; controller unresponsive."),
    sys::BLE_HS_ENOMEM_EVT => Some(
      "Controller failed to send event due to memory exhaustion (combined host-controller only).",
    ),
    sys::BLE_HS_ENOADDR => Some("Operation requires an identity address but none configured."),
    sys::BLE_HS_ENOTSYNCED => {
      Some("Attempt to use the host before it is synced with controller.")
    }
    sys::BLE_HS_EAUTHEN => Some("Insufficient authentication."),
    sys::BLE_HS_EAUTHOR => Some("Insufficient authorization."),
    sys::BLE_HS_EENCRYPT => Some("Insufficient encryption level."),
    sys::BLE_HS_EENCRYPT_KEY_SZ => Some("Insufficient key size"),
    sys::BLE_HS_ESTORE_CAP => Some("Storage at capacity."),
    sys::BLE_HS_ESTORE_FAIL => Some("Storage IO error."),

    // 原版还包含 ATT_ERR / HCI_ERR / L2C_ERR / SM_US_ERR 等子系统错误描述，
    // 但这些用了 inline const pattern 在 ESP rustc 上无法编译。功能不受影响（只是日志描述更简略）。
    _ => None,
  }
}

#[cfg(not(feature = "debug"))]
macro_rules! ble {
  ($err:expr) => {{
    $crate::BLEError::convert($err as _)
  }};
}
#[cfg(feature = "debug")]
macro_rules! ble {
  ($err:expr) => {{
    let rc = $crate::BLEError::convert($err as _);
    if let Err(err) = rc {
      ::log::warn!(target: "esp32_nimble", "{}[{}]: {:?}", file!(), line!(), err);
    }
    rc
  }};
}

pub(crate) use ble;
