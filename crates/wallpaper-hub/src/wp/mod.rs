// SPDX-License-Identifier: MIT
//
//! 壁纸访问层。
//!
//! 三条路：`local`（自己扫工坊目录）、`workshop`（工坊元数据）、`native`（自研内核控制面）。
//!
//! `proto` 是数据模型（由 `proto/control.proto` 生成），只当"条目/类型"这些纯结构用；
//! 协议调用已全部移除，统一 `allow(dead_code)`。

pub mod local;
pub mod native;
pub mod workshop;

/// 遗留下来的**数据模型**（由 `proto/control.proto` 生成）。
/// 现在只当"条目/类型"这些纯结构用（本地扫描器产出的也是它）；协议请求/响应类型已成死代码，
/// 统一 `allow(dead_code)` —— 别再往里加协议调用，那些路已经没了。
#[allow(dead_code)]
pub mod proto {
    // 生成文件按 proto 的 package 命名：waywallen.control.v1
    include!(concat!(env!("OUT_DIR"), "/waywallen.control.v1.rs"));
}
