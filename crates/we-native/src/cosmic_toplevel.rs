//! COSMIC 自有的窗口信息协议绑定（`zcosmic_toplevel_info_v1`）。
//!
//! 为什么要它：`ext_foreign_toplevel_list_v1` 只给身份（title/app_id/identifier），
//! **没有 state** —— 拿不到"这个窗口是不是全屏"。全屏/最大化状态只在 COSMIC 这个协议里，
//! 而 COSMIC 1.9 不提供 `zwlr_foreign_toplevel_manager_v1`。
//!
//! 用法要点（协议 v2+ 的流程）：
//!
//! 1. 从 `ext_foreign_toplevel_list_v1` 拿到每个窗口的 `ext_foreign_toplevel_handle_v1`
//!
//! 2. 对每个句柄调 `zcosmic_toplevel_info_v1.get_cosmic_toplevel(handle)` 取扩展对象
//!
//! 3. 扩展对象补发完整属性：`state`（**array = 完整状态集**，不是增量）与 `geometry`
//! XML 来自 `pop-os/cosmic-protocols`（MIT）。我们只绑定到 **v2**：`get_cosmic_toplevel`、
//! `state`、`geometry` 在 v2 就全有了；v3 只多了 workspace 相关事件（用不到，且会引入
//! `zcosmic_workspace_handle_v1` / `ext_workspace_handle_v1` 两个额外依赖）。
//! 因此这里的 `cosmic-toplevel-info-v2.xml` 是上游 v1 文件**裁掉 workspace 事件、版本号降为 2**
//! 的副本 —— 绑定 v2 时合成器不会发送 v3 事件，语义上与被裁剪部分完全等价。

pub mod cosmic_toplevel {
    // 生成代码把这些当作用域内的路径使用
    pub use wayland_client;
    pub use wayland_client::protocol::*;

    pub mod __interfaces {
        // 工具生成的低层接口表示：它按 `wayland_backend::…` 找 backend，故需要这个别名
        pub use wayland_client;
        pub use wayland_client::backend as wayland_backend;
        pub use wayland_client::protocol::__interfaces::*;
        // XML 引用的外部接口常量（EXT_FOREIGN_TOPLEVEL_HANDLE_V1_INTERFACE 等）在这里
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::__interfaces::*;
        use wayland_protocols::ext::workspace::v1::client::__interfaces::*;
        // generate_interfaces! 一个模块只能用一次（会定义 types_null/SyncWrapper 等公共项），
        // 所以两个协议先合并成一个 XML：protocols/cosmic-combined.xml（脚本生成，接口名不变）
        wayland_scanner::generate_interfaces!("./protocols/cosmic-combined.xml");
    }
    use self::__interfaces::*;

    // XML 里引用的外部接口：模块本身（生成代码写作 super::ext_…::…）与内容（接口常量）都要可
    pub use wayland_client::protocol::wl_output;
    pub use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1;
    pub use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::*;
    pub use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1;
    pub use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::*;

    wayland_scanner::generate_client_code!("./protocols/cosmic-combined.xml");
}
