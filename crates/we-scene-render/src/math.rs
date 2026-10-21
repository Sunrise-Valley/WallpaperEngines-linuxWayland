// SPDX-License-Identifier: MIT
//
//! 最小 4×4 矩阵（不引第三方数学库）。存储是 **OpenGL 的列主序**：`m[c*4 + r]`。
//!
//! 为什么上传时不需要转置（容易搞错，写清楚）：
//! WE 的 shader 里是 `gl_Position = mul(vec4(pos,1), MVP)` —— **行向量 × 矩阵**（HLSL 约定）。
//! GLSL 的 `p * M` 算的是 `result_j = Σ_i p_i * M[col=i][row=j]`；
//! 而我们要的是标准列向量乘积 `A · p`，即 `result_j = Σ_i A[j][i] * p_i`。
//! 两者相等要求 `M[col=i][row=j] = A[j][i]`，而列主序数组正好是 `out[i*4+j] = A[j][i]`
//! ⇒ **把本结构体的 `m` 原样交给 `glUniformMatrix4fv(transpose = false)` 就是对的**。

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4 {
    /// 列主序：`m[c * 4 + r]`
    pub m: [f32; 16],
}

impl Mat4 {
    pub fn identity() -> Self {
        let mut m = [0.0f32; 16];
        m[0] = 1.0;
        m[5] = 1.0;
        m[10] = 1.0;
        m[15] = 1.0;
        Mat4 { m }
    }

    /// 经典 glOrtho（列向量约定）
    pub fn ortho(l: f32, r: f32, b: f32, t: f32, n: f32, f: f32) -> Self {
        let mut m = [0.0f32; 16];
        m[0] = 2.0 / (r - l);
        m[5] = 2.0 / (t - b);
        m[10] = -2.0 / (f - n);
        m[12] = -(r + l) / (r - l);
        m[13] = -(t + b) / (t - b);
        m[14] = -(f + n) / (f - n);
        m[15] = 1.0;
        Mat4 { m }
    }

    /// 经典 gluLookAt
    pub fn look_at(eye: [f32; 3], center: [f32; 3], up: [f32; 3]) -> Self {
        let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let cross = |a: [f32; 3], b: [f32; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let norm = |v: [f32; 3]| {
            let l = dot(v, v).sqrt();
            if l == 0.0 {
                v
            } else {
                [v[0] / l, v[1] / l, v[2] / l]
            }
        };
        let f = norm(sub(center, eye));
        let s = norm(cross(f, up));
        let u = cross(s, f);
        let mut m = [0.0f32; 16];
        m[0] = s[0];
        m[1] = u[0];
        m[2] = -f[0];
        m[4] = s[1];
        m[5] = u[1];
        m[6] = -f[1];
        m[8] = s[2];
        m[9] = u[2];
        m[10] = -f[2];
        m[12] = -dot(s, eye);
        m[13] = -dot(u, eye);
        m[14] = dot(f, eye);
        m[15] = 1.0;
        Mat4 { m }
    }

    pub fn translate(v: [f32; 3]) -> Self {
        let mut m = Self::identity().m;
        m[12] = v[0];
        m[13] = v[1];
        m[14] = v[2];
        Mat4 { m }
    }

    pub fn scale(x: f32, y: f32, z: f32) -> Self {
        let mut m = [0.0f32; 16];
        m[0] = x;
        m[5] = y;
        m[10] = z;
        m[15] = 1.0;
        Mat4 { m }
    }

    /// 绕 Z 轴旋转（弧度）。**场景的 `angles` 就是弧度**，和参考实现 OWE 的注释一致
    /// （`GetLocalTrans`：静态 scene.json 的 angles 已是 rad，脚本 API 才用度）。
    pub fn rotate_z(a: f32) -> Self {
        let (s, c) = a.sin_cos();
        let mut m = [0.0f32; 16];
        m[0] = c;
        m[1] = s;
        m[4] = -s;
        m[5] = c;
        m[10] = 1.0;
        m[15] = 1.0;
        Mat4 { m }
    }

    /// 绕 Y 轴旋转（弧度）
    pub fn rotate_y(a: f32) -> Self {
        let (s, c) = a.sin_cos();
        let mut m = [0.0f32; 16];
        m[0] = c;
        m[2] = -s;
        // ⚠ 这一项不能漏：Y 轴旋转要**保留 y 分量**（漏了就是 m[5]=0 ⇒ 整层被压成零高 ⇒ 全黑）
        m[5] = 1.0;
        m[8] = s;
        m[10] = c;
        m[15] = 1.0;
        Mat4 { m }
    }

    /// 绕 X 轴旋转（弧度）
    pub fn rotate_x(a: f32) -> Self {
        let (s, c) = a.sin_cos();
        let mut m = [0.0f32; 16];
        m[0] = 1.0;
        m[5] = c;
        m[6] = s;
        m[9] = -s;
        m[10] = c;
        m[15] = 1.0;
        Mat4 { m }
    }

    /// `self · other`（数学意义上的乘积，不是逐元素）
    pub fn mul(&self, o: &Mat4) -> Mat4 {
        let mut m = [0.0f32; 16];
        for c in 0..4 {
            for r in 0..4 {
                let mut acc = 0.0;
                for k in 0..4 {
                    acc += self.m[k * 4 + r] * o.m[c * 4 + k];
                }
                m[c * 4 + r] = acc;
            }
        }
        Mat4 { m }
    }

    /// 直接喂给 `glUniformMatrix4fv(..., false, ...)`
    pub fn column_major(&self) -> [f32; 16] {
        self.m
    }

    /// 转置。实测：WE 的 `mul(vec4, mat4)` 在这个 GL 驱动上要求**上传转置后**的矩阵，
    /// 否则非等比缩放会被放到错的槽位、画面出现剪切/倾斜（第一帧就是这么斜的）。
    pub fn transposed(&self) -> Mat4 {
        let mut m = [0.0f32; 16];
        for c in 0..4 {
            for r in 0..4 {
                m[c * 4 + r] = self.m[r * 4 + c];
            }
        }
        Mat4 { m }
    }

    /// 通用 4×4 求逆（伴随矩阵 / 余子式）。奇异（|det| < 1e-12）⇒ `None`。
    /// 为 `g_EffectTextureProjectionMatrixInverse` 的真实投影链加的：
    /// WE 的 `effects/xray`（以及 `cursorripple`/`fluidsimulation`/`depthparallax`）
    /// 用这条矩阵把**屏幕 NDC 反投影回图层纹理空间**；参照实现 OWE 走的是
    /// 「每图层一条真实投影链」（`effect_model = layer_model · Scaling(size/2)`，
    /// `SpecNames.cppm:73-79` + `UniformSource.cpp:641-660`），
    /// LWE 那边是硬编码单位阵（`CPass.cpp:881-882`）。见 `docs/reference-implementation-notes.md` §4.3。
    pub fn invert(&self) -> Option<Mat4> {
        let m = &self.m;
        // 数学矩阵的第 r 行第 c 列（我们的 `m` 是列主序：`m[c*4+r]`）
        let a = |r: usize, c: usize| m[c * 4 + r];
        // 去掉第 i 行、第 j 列后的 3×3 行列式
        let minor = |i: usize, j: usize| -> f32 {
            let rows: Vec<usize> = (0..4).filter(|&x| x != i).collect();
            let cols: Vec<usize> = (0..4).filter(|&x| x != j).collect();
            let (r0, r1, r2) = (rows[0], rows[1], rows[2]);
            let (c0, c1, c2) = (cols[0], cols[1], cols[2]);
            a(r0, c0) * (a(r1, c1) * a(r2, c2) - a(r1, c2) * a(r2, c1))
                - a(r0, c1) * (a(r1, c0) * a(r2, c2) - a(r1, c2) * a(r2, c0))
                + a(r0, c2) * (a(r1, c0) * a(r2, c1) - a(r1, c1) * a(r2, c0))
        };
        let mut det = 0.0f32;
        for j in 0..4 {
            det += if j % 2 == 0 { 1.0 } else { -1.0 } * a(0, j) * minor(0, j);
        }
        if det.abs() < 1e-12 {
            return None;
        }
        let inv_det = 1.0 / det;
        let mut inv = [0.0f32; 16];
        for r in 0..4 {
            for c in 0..4 {
                // adj(M)[r][c] = (-1)^(r+c) · minor(c, r) ⇒ M⁻¹ = adj/det
                let s = if (r + c) % 2 == 0 { 1.0 } else { -1.0 };
                inv[c * 4 + r] = s * minor(c, r) * inv_det;
            }
        }
        Some(Mat4 { m: inv })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: [f32; 16], b: [f32; 16]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn identity_is_neutral() {
        let i = Mat4::identity();
        let t = Mat4::translate([3.0, -2.0, 5.0]);
        assert!(approx(i.mul(&t).m, t.m));
        assert!(approx(t.mul(&i).m, t.m));
    }

    #[test]
    fn transpose_is_an_involution_and_swaps_entries() {
        let mut a = Mat4::identity();
        a.m[1] = 7.0; // 第 0 列第 1 行
        let t = a.transposed();
        assert_eq!(t.m[4], 7.0); // 跑到第 1 列第 0 行
        assert_eq!(t.transposed().m, a.m);
    }

    #[test]
    fn rotations_match_the_standard_convention() {
        let apply = |m: Mat4, v: [f32; 3]| {
            let mut out = [0.0f32; 3];
            for r in 0..3 {
                out[r] = (0..4).map(|c| m.m[c * 4 + r] * if c == 3 { 1.0 } else { v[c] }).sum();
            }
            out
        };
        // 绕 Z 转 90°：(1,0) → (0,1)
        let z = apply(Mat4::rotate_z(std::f32::consts::FRAC_PI_2), [1.0, 0.0, 0.0]);
        assert!((z[0]).abs() < 1e-5 && (z[1] - 1.0).abs() < 1e-5, "{z:?}");
        // 绕 X 转 90°：(0,1,0) → (0,0,1)
        let x = apply(Mat4::rotate_x(std::f32::consts::FRAC_PI_2), [0.0, 1.0, 0.0]);
        assert!(x[1].abs() < 1e-5 && (x[2] - 1.0).abs() < 1e-5, "{x:?}");
        // 绕 Y 转 90°：(0,0,1) → (1,0,0)
        let y = apply(Mat4::rotate_y(std::f32::consts::FRAC_PI_2), [0.0, 0.0, 1.0]);
        assert!((y[0] - 1.0).abs() < 1e-5 && y[2].abs() < 1e-5, "{y:?}");
        // 每种旋转都必须**保留自己的轴**（漏一项就会把整层压成零宽/零高 —— 实测 Sona 全黑就是这个）
        let keep = |m: Mat4, axis: usize| {
            let mut v = [0.0f32; 3];
            v[axis] = 1.0;
            let o = apply(m, v);
            assert!((o[axis] - 1.0).abs() < 1e-5, "轴 {axis} 没被保留：{o:?}");
        };
        keep(Mat4::rotate_x(0.7), 0);
        keep(Mat4::rotate_y(0.7), 1);
        keep(Mat4::rotate_z(0.7), 2);
        // 零角 = 单位阵
        let id = Mat4::rotate_z(0.0);
        assert!((id.m[0] - 1.0).abs() < 1e-6 && id.m[1].abs() < 1e-6);
    }

    #[test]
    fn hierarchy_matches_parent_times_local() {
        // world = parent · local（和 OWE 的 SceneNode 层级一致）
        let parent = Mat4::translate([10.0, 0.0, 0.0])
            .mul(&Mat4::scale(2.0, 2.0, 1.0));
        let local = Mat4::translate([5.0, 0.0, 0.0]);
        let world = parent.mul(&local);
        // 父层缩放 2 ⇒ 子层的 +5 被放大成 +10；再加父层平移 10 ⇒ 20
        let mut out = [0.0f32; 4];
        for r in 0..4 {
            out[r] = (0..4).map(|c| world.m[c * 4 + r] * [0.0, 0.0, 0.0, 1.0][c]).sum();
        }
        assert!((out[0] - 20.0).abs() < 1e-5, "{out:?}");
    }

    #[test]
    fn ortho_maps_corners_to_clip_space() {
        // 正交投影把 (±w/2, ±h/2, -1) 映到 NDC 的 (±1, ±1)
        let p = Mat4::ortho(-2.0, 2.0, -1.0, 1.0, 0.01, 100.0);
        let apply = |v: [f32; 4]| {
            let mut out = [0.0f32; 4];
            for r in 0..4 {
                out[r] = (0..4).map(|c| p.m[c * 4 + r] * v[c]).sum();
            }
            out
        };
        let o = apply([2.0, 1.0, -0.5, 1.0]);
        assert!((o[0] - 1.0).abs() < 1e-5, "{o:?}");
        assert!((o[1] - 1.0).abs() < 1e-5, "{o:?}");
    }

    #[test]
    fn look_at_puts_center_on_negative_z() {
        let v = Mat4::look_at([0.0, 0.0, 5.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        // 目标点 (0,0,0) 经视图变换后应在 (0,0,-5)
        let p = [0.0f32, 0.0, 0.0, 1.0];
        let mut out = [0.0f32; 4];
        for r in 0..4 {
            out[r] = (0..4).map(|c| v.m[c * 4 + r] * p[c]).sum();
        }
        assert!((out[2] + 5.0).abs() < 1e-5, "{out:?}");
    }

    #[test]
    fn invert_is_a_two_sided_inverse() {
        let apply = |m: &Mat4, v: [f32; 4]| -> [f32; 4] {
            let mut o = [0.0f32; 4];
            for r in 0..4 {
                o[r] = (0..4).map(|c| m.m[c * 4 + r] * v[c]).sum();
            }
            o
        };
        let cases = [
            Mat4::identity(),
            Mat4::ortho(-2.0, 2.0, -1.0, 1.0, 0.01, 100.0),
            Mat4::translate([3.0, -7.0, 2.0]).mul(&Mat4::scale(2.0, -4.0, 1.0)),
            Mat4::rotate_z(0.7).mul(&Mat4::translate([5.0, 1.0, 0.0])),
            // 带透视的（4×4 非仿射）：仿射逆的实现会在这里露馅
            Mat4 {
                m: [
                    2.0, 0.0, 0.0, -1.0,
                    0.0, 2.0, 0.0, 0.5,
                    0.0, 0.0, 1.0, 0.0,
                    0.0, 0.0, 0.0, 1.0,
                ],
            },
        ];
        for (i, m) in cases.iter().enumerate() {
            let inv = m.invert().unwrap_or_else(|| panic!("用例 {i} 不可逆"));
            for v in [
                [0.0f32, 0.0, 0.0, 1.0],
                [1.0, 2.0, 0.0, 1.0],
                [-3.0, 0.5, 0.0, 1.0],
            ] {
                let back = apply(&inv, apply(m, v));
                // 逐位（含 w）回到 v，容差按数值误差给
                for k in 0..4 {
                    assert!(
                        (back[k] - v[k]).abs() < 1e-3,
                        "用例 {i}: v={v:?} 往返得到 {back:?}"
                    );
                }
            }
        }
        // 奇异矩阵 ⇒ None（而不是 inf/NaN）
        let mut zero = Mat4::identity();
        for k in 0..16 {
            zero.m[k] = 0.0;
        }
        assert!(zero.invert().is_none());
    }
}
