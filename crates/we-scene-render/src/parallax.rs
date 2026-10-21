// SPDX-License-Identifier: MIT
//
//! 相机视差：`general.cameraparallax*` + 对象 `parallaxDepth`。
//! 本模块是这一条的**唯一落地点**（以前 `parallax_disp` 只是个没接线的桩）。
//! 公式与规则**照 OWE**（`Scene/Pkg/Parse/Uniform/UniformSource.cpp` +
//! `SceneObj/SceneDocument.cppm:19-45`，逐行出处见 `docs/reference-implementation-notes.md` §5）：
//!
//! ```text
//! mouse = diag(1,-1)·(0.5 − pointer) ⊙ sceneOrtho · mouse_influence // OWE:380-381
//! offset(scene) = (node_xy − camera_xy + mouse) ⊙ depth · amount // OWE:383-385
//! ```
//! 三条语义（OWE 独有、LWE 没有，我们取 OWE —— 文档 §5.3/§6.6 的结论）：
//!
//! * **缺省 + 正交场景 ⇒ depth = {1,1}**（`kImplicitOrthographicParallaxDepth`）；
//!
//! * **显式 {0,0} ⇒ 冻结该层/子树**（`IsZeroParallaxDepth`）；
//!
//! * **父链传播**：自己没有 `parallaxDepth` 时向上找最近的\"作者化祖先\"，
//! 中间任何一层 `disablepropagation` 就停（`LogicalParallaxState`）。
//! 透视场景 + 无作者深度 ⇒ 该层**无视差**（我们用 `ortho_implicit=false` 表达）。
//! `delay` 用 OWE 的**速率**模型（`Advance`，`UniformSource.cpp:417-438`）：它不是时间常数
//! 而是响应速率 —— 0 = 立刻吸附、1 ≈ 6.67/s、2 ≈ 3.33/s、≥3 = 不跟随（冻结）。
//! LWE 的 `delay*dt` 逐帧插值会随帧率漂移，不用。
//!
//! 开关：`WE_PARALLAX=1`（**默认关** —— 它会影响所有声明了 `cameraparallax` 的场景，
//! 而本机只有 4 条带参照、且都在别的子代理正在动的文件之外）。

/// 场景声明的视差参数（`general.cameraparallax*`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cfg {
    pub enable: bool,
    /// `cameraparallaxamount`
    pub amount: f32,
    /// `cameraparallaxdelay`（速率模型里的 \"delay\"，不是秒）
    pub delay: f32,
    /// `cameraparallaxmouseinfluence`
    pub mouse_influence: f32,
}

impl Default for Cfg {
    fn default() -> Self {
        // OWE `UniformCameraParallax` 的结构体默认值（`UniformSource.cppm:148-153`）
        Cfg {
            enable: false,
            amount: 0.5,
            delay: 0.1,
            mouse_influence: 0.0,
        }
    }
}

/// 场景级视差状态（每帧推进；`pointer` 是**平滑后**的鼠标位）。
#[derive(Debug, Clone, Copy)]
pub struct State {
    pub cfg: Cfg,
    /// 平滑后的鼠标位（初始 = 中性 0.5,0.5）
    pub pointer: [f32; 2],
    /// 上一帧的平滑鼠标位（OWE 的 `pointer_last`；给跟手速度类 shader）
    pub pointer_last: [f32; 2],
    /// 场景正交尺寸（= 画布宽高；`Ortho`）
    pub ortho: [f32; 2],
    /// 相机在场景里的位置（我们用可见区中心）
    pub camera: [f32; 2],
    /// 透视图层的 implicit depth 规则：非正交场景 ⇒ false
    pub ortho_implicit: bool,
}

impl State {
    pub fn new(cfg: Cfg, ortho: [f32; 2], camera: [f32; 2], ortho_implicit: bool) -> Self {
        State {
            cfg,
            pointer: [0.5, 0.5],
            pointer_last: [0.5, 0.5],
            ortho,
            camera,
            ortho_implicit,
        }
    }

    /// 每帧推进 `pointer`（OWE `Advance` 的速率模型）。
    pub fn advance(&mut self, target: [f32; 2], dt: f32) {
        self.pointer_last = self.pointer;
        self.pointer = advance(self.pointer, target, self.cfg.delay, dt);
    }

    /// 鼠标项（场景单位，含 `diag(1,-1)` 与 `influence`）。
    pub fn mouse_term(&self) -> [f32; 2] {
        mouse_term(self.pointer, self.ortho, self.cfg.mouse_influence)
    }

    /// 供 uniform `g_ParallaxPosition`（[0,1] 空间，y 取反、受 influence 缩放）。
    pub fn position_uniform(&self) -> [f32; 2] {
        position_uniform(self.pointer, self.cfg.enable, self.cfg.mouse_influence)
    }

    /// 某个对象的场景位移：`(node − cam + mouse) ⊙ depth · amount`。
    pub fn offset(&self, node_xy: [f32; 2], depth: [f32; 2]) -> [f32; 2] {
        offset(
            node_xy,
            self.camera,
            self.mouse_term(),
            depth,
            self.cfg.amount,
        )
    }

    /// 把场景位移换成 NDC 位移（正交投影 ⇒ 线性；`vis` = 可见区宽高）。
    pub fn ndc_shift(&self, scene_offset: [f32; 2], vis: [f32; 2]) -> [f32; 2] {
        [
            2.0 * scene_offset[0] / vis[0].max(1e-6),
            2.0 * scene_offset[1] / vis[1].max(1e-6),
        ]
    }
}

/// `pointer` 的平滑推进（OWE `UniformSource.cpp:417-438`）。
///
/// `delay ≤ 0` ⇒ 立刻吸附；`delay ≥ 3` ⇒ 完全不跟随（速率 0）；中间线性：
/// `rate = 10·max(1 − delay/3, 0)`（1/s），`t = min(rate·dt, 1)`。
pub fn advance(cur: [f32; 2], target: [f32; 2], delay: f32, dt: f32) -> [f32; 2] {
    let delay = delay.max(0.0);
    if delay <= 0.0 {
        return target;
    }
    const RANGE: f32 = 3.0;
    const RATE: f32 = 10.0;
    let rate = RATE * (1.0 - delay / RANGE).max(0.0);
    if rate <= 0.0 {
        // delay ≥ 3：不跟随 —— 停在当前位置
        return cur;
    }
    let t = (rate * dt.max(0.0)).min(1.0);
    [
        cur[0] + (target[0] - cur[0]) * t,
        cur[1] + (target[1] - cur[1]) * t,
    ]
}

/// 鼠标项：`diag(1,-1)·(0.5 − pointer) ⊙ ortho · influence`（场景像素）。
pub fn mouse_term(pointer: [f32; 2], ortho: [f32; 2], influence: f32) -> [f32; 2] {
    let cx = 0.5 - pointer[0];
    let cy = 0.5 - pointer[1];
    // diag(1,-1)：y 取反（与 \"pointer 的 y 向下\" 配套）
    [cx * ortho[0] * influence, -cy * ortho[1] * influence]
}

/// 场景位移：`(node − cam + mouse) ⊙ depth · amount`。
pub fn offset(
    node_xy: [f32; 2],
    camera_xy: [f32; 2],
    mouse: [f32; 2],
    depth: [f32; 2],
    amount: f32,
) -> [f32; 2] {
    [
        (node_xy[0] - camera_xy[0] + mouse[0]) * depth[0] * amount,
        (node_xy[1] - camera_xy[1] + mouse[1]) * depth[1] * amount,
    ]
}

/// `g_ParallaxPosition`（`UniformSource.cpp:738-745`）：`0.5 + diag(1,-1)·(pointer−0.5)·influence`。
pub fn position_uniform(pointer: [f32; 2], enable: bool, influence: f32) -> [f32; 2] {
    if !enable {
        return [0.5, 0.5];
    }
    let cx = pointer[0] - 0.5;
    let cy = pointer[1] - 0.5;
    [
        0.5 + cx * influence,
        0.5 + (-cy) * influence,
    ]
}

/// 对象 `parallaxDepth` 的**有效值**（OWE `SceneDocument.cppm:19-45` 的规则集）。
///
/// * `authored = Some(v)`：作者值；`{0,0}` ⇒ 冻结（原样返回 `{0,0}`）。
///
/// * `authored = None`：正交场景 ⇒ `{1,1}`（implicit）；透视场景 ⇒ `{0,0}`（该层无视差）。
pub fn depth_effective(authored: Option<[f32; 2]>, ortho_implicit: bool) -> [f32; 2] {
    match authored {
        Some(d) => d,
        None => {
            if ortho_implicit {
                [1.0, 1.0]
            } else {
                [0.0, 0.0]
            }
        }
    }
}

/// 父链传播：自己没有 `parallaxDepth` 时向上找最近的作者化祖先
/// （OWE `LogicalParallaxState`：中间任一层 `disablepropagation` ⇒ 停）。
///
/// `chain` = 由内到外的 `(depth 字符串, disable_propagation)`；返回值同时告知
/// 是否找到作者值（`None` = 整条链都没有 ⇒ 走 implicit 规则）。
pub fn depth_from_chain(chain: &[(String, bool)]) -> Option<[f32; 2]> {
    for (s, stop) in chain {
        let t = s.trim();
        if !t.is_empty() {
            if let Some(d) = we_scene::scene::parse_vec2(t) {
                return Some(d);
            }
            // 写坏的值：当作没有作者值（不猜）
        }
        if *stop {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
    }

    #[test]
    fn mouse_term_flips_y_and_scales_by_influence() {
        // 鼠标在左上角 ⇒ (0.5-0)=+0.5 的 x 项、y 取反后 = -0.5
        let m = mouse_term([0.0, 0.0], [4000.0, 2000.0], 1.0);
        assert!(close(m, [2000.0, -1000.0]), "{m:?}");
        // influence=0 ⇒ 没有鼠标项（OWE 的 `m_mouse_parallax ? mouse_influence : 0`）
        assert!(close(mouse_term([0.0, 0.0], [4000.0, 2000.0], 0.0), [0.0, 0.0]));
        // 中位 = 中性
        assert!(close(mouse_term([0.5, 0.5], [4000.0, 2000.0], 1.0), [0.0, 0.0]));
        // 夏日影的声明值（influence = -0.62）⇒ 方向反过来：
        // 指针 y=0.25 在中心之上 ⇒ 未取反的偏移 = -0.25·H，乘负 influence ⇒ **变正**
        let m2 = mouse_term([0.5, 0.25], [5476.0, 3080.0], -0.62);
        assert!(close(m2, [0.0, 0.25 * 3080.0 * 0.62]), "{m2:?}");
    }

    #[test]
    fn offset_is_multiplicative_in_depth_and_amount() {
        // depth = 0 ⇒ 位移 0（**乘法**语义；LWE 的 `depth + amount` 在 depth=0 时仍有位移）
        let o = offset([100.0, 200.0], [50.0, 50.0], [10.0, -10.0], [0.0, 0.0], 0.5);
        assert!(close(o, [0.0, 0.0]), "{o:?}");
        // amount = 0 ⇒ 位移 0
        let o = offset([100.0, 200.0], [50.0, 50.0], [10.0, -10.0], [1.0, 1.0], 0.0);
        assert!(close(o, [0.0, 0.0]), "{o:?}");
        // 完整式：((100-50)+10)·1·0.5 = 30
        let o = offset([100.0, 200.0], [50.0, 50.0], [10.0, -10.0], [1.0, 1.0], 0.5);
        assert!(close(o, [30.0, (150.0 - 10.0) * 0.5]), "{o:?}");
    }

    #[test]
    fn depth_rules_follow_owe() {
        // 显式值原样（含 {0,0} = 冻结）
        assert_eq!(depth_effective(Some([0.0, 0.0]), true), [0.0, 0.0]);
        assert_eq!(depth_effective(Some([0.7, 0.7]), true), [0.7, 0.7]);
        // 缺省 + 正交 ⇒ {1,1}
        assert_eq!(depth_effective(None, true), [1.0, 1.0]);
        // 缺省 + 透视 ⇒ {0,0}
        assert_eq!(depth_effective(None, false), [0.0, 0.0]);
    }

    #[test]
    fn depth_propagates_up_the_chain_until_a_barrier() {
        let v = |s: &str| (s.to_string(), false);
        // 自己没有 ⇒ 用父的
        assert_eq!(
            depth_from_chain(&[v(""), v("0.25 0.25")]),
            Some([0.25, 0.25])
        );
        // 显示 0 0 也是\"作者值\"（冻结），不该被父覆盖
        assert_eq!(depth_from_chain(&[v("0 0"), v("1 1")]), Some([0.0, 0.0]));
        // disablepropagation 的层挡住传播
        let barrier = (String::new(), true);
        assert_eq!(depth_from_chain(&[v(""), barrier, v("1 1")]), None);
        // 全空 ⇒ None（交给 implicit 规则）
        assert_eq!(depth_from_chain(&[v(""), v("  ")]), None);
    }

    #[test]
    fn advance_uses_owe_rate_model() {
        // delay = 0 ⇒ 立刻吸附
        assert_eq!(advance([0.0, 0.0], [1.0, 1.0], 0.0, 0.5), [1.0, 1.0]);
        // delay ≥ 3 ⇒ 不跟随（冻结）
        assert_eq!(advance([0.1, 0.2], [1.0, 1.0], 3.0, 5.0), [0.1, 0.2]);
        assert_eq!(advance([0.1, 0.2], [1.0, 1.0], 9.0, 5.0), [0.1, 0.2]);
        // delay = 1.07（夏日影）⇒ rate = 10·(1−1.07/3) = 6.4333/s
        let dt = 1.0 / 30.0;
        let t = 10.0 * (1.0 - 1.07 / 3.0) * dt;
        let got = advance([0.0, 0.0], [1.0, 1.0], 1.07, dt);
        assert!((got[0] - t).abs() < 1e-5, "{got:?} 期望 {t}");
        // 一步到位不会过冲（t 夹在 1）
        let got = advance([0.0, 0.0], [1.0, 1.0], 0.1, 10.0);
        assert_eq!(got, [1.0, 1.0]);
    }

    #[test]
    fn position_uniform_matches_owe() {
        // 关 ⇒ 中性
        assert_eq!(position_uniform([0.2, 0.8], false, 1.0), [0.5, 0.5]);
        // 开：0.5 + diag(1,-1)·(p-0.5)·influence
        let p = position_uniform([0.75, 0.25], true, 0.5);
        assert!(close(p, [0.5 + 0.25 * 0.5, 0.5 + 0.25 * 0.5]), "{p:?}");
        // 负 influence（夏日影 -0.62）⇒ 反向
        let p = position_uniform([1.0, 0.5], true, -0.62);
        assert!(close(p, [0.5 - 0.31, 0.5]), "{p:?}");
    }

    #[test]
    fn ndc_shift_matches_ortho_scale() {
        let st = State::new(Cfg::default(), [4000.0, 2000.0], [2000.0, 1000.0], true);
        let s = st.ndc_shift([200.0, -100.0], [4000.0, 2000.0]);
        assert!(close(s, [0.1, -0.1]), "{s:?}");
    }
}
