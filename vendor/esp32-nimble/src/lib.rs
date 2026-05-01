#![no_std]
#![allow(clippy::new_without_default)]
#![allow(clippy::single_match)]
#![allow(static_mut_refs)]
#![allow(dangerous_implicit_autorefs)]    // ESP rustc 1.92-nightly 升级为 error，esp32-nimble 大量 raw pointer autoref，禁用
#![allow(unsafe_op_in_unsafe_fn)]
#![feature(decl_macro)]
#![feature(get_mut_unchecked)]
// inline_const_pat 已被 ESP rustc 1.92-nightly 移除/重命名，本 vendor 删除该 feature
// 同时 ble_error.rs 中所有 const block pattern 已改为 wildcard 简化处理
#![doc = include_str!("../README.md")]

#[cfg(feature = "std")]
#[allow(unused_imports)]
#[macro_use]
extern crate std;

extern crate alloc;

#[doc(hidden)]
pub use uuid::uuid as uuid_macro;

mod ble_address;
pub use self::ble_address::*;

pub(crate) type Signal<T> =
  embassy_sync::signal::Signal<esp_idf_svc::hal::task::embassy_sync::EspRawMutex, T>;
#[allow(dead_code)]
pub(crate) type Channel<T, const N: usize> =
  embassy_sync::channel::Channel<esp_idf_svc::hal::task::embassy_sync::EspRawMutex, T, N>;

mod ble_device;
pub use self::ble_device::BLEDevice;

mod ble_error;
pub(crate) use self::ble_error::ble;
pub use self::ble_error::BLEError;

mod ble_security;
pub use self::ble_security::BLESecurity;

pub mod enums;

// elfRadio 项目仅用 GATT Server（手机 DTrac 连 ESP32），不用 Client（Central 端扫描连其他设备）
// client 模块在 ESP rustc 1.92-nightly 上有 type inference 错误，禁用以避免编译失败
// mod client;
// pub use self::client::*;

mod server;
pub use self::server::*;

pub mod l2cap;

pub mod utilities;

#[allow(unused)]
macro_rules! dbg {
  ($val:expr) => {
    match $val {
      tmp => {
        ::log::info!("{} = {:#?}", stringify!($val), &tmp);
        tmp
      }
    }
  };
}

#[allow(unused)]
pub(crate) use dbg;
