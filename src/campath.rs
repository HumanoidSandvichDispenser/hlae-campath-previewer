//! Camera-path interpolation + HLAE export, ported 1:1 from the web previewer's
//! `campath.ts` / `campathExport.ts` so a path built here renders identically under
//! `mirv_campath`. See those files for the full derivation and the HLAE/qspline
//! license notes. The interpolation design mirrors advancedfx shared/AfxMath and
//! McEnnan's CC0 qspline:
//!  - position & fov cubic = clamped (zero-end-velocity) C2 spline via moments,
//!  - rotation sCubic       = McEnnan quaternion spline (needs >= 4 keyframes),
//!  - rotation sLinear / <4 = shortest-arc slerp.
//!
//! All values are parameterized by keyframe TICK (converted to seconds only on
//! export). Positions/quaternions are stored in Source/Hammer Z-up coords — the
//! same frame the web app used — so `source_angles_from_quaternion` and the XML
//! writer work unchanged. The renderer converts to/from bevy Y-up at the edges.

use bevy::math::{Quat, Vec3};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PositionInterp {
    Linear,
    Cubic,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RotationInterp {
    SLinear,
    SCubic,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FovInterp {
    Linear,
    Cubic,
}

#[derive(Clone, Copy)]
pub struct CampathInterp {
    pub position: PositionInterp,
    pub rotation: RotationInterp,
    pub fov: FovInterp,
}

impl Default for CampathInterp {
    fn default() -> Self {
        // Matches DEFAULT_CAMPATH_INTERP in campath.ts.
        Self {
            position: PositionInterp::Cubic,
            rotation: RotationInterp::SCubic,
            fov: FovInterp::Cubic,
        }
    }
}

/// One keyframe. Position/quaternion are Source Z-up; quaternion is [x,y,z,w].
/// fov is vertical degrees (what the bevy PerspectiveProjection carries).
#[derive(Clone, Copy)]
pub struct Keyframe {
    pub tick: u32,
    pub position: [f32; 3],
    pub quaternion: [f32; 4], // x, y, z, w
    pub fov: f32,
}

pub struct Sample {
    pub position: Vec3,
    pub quaternion: Quat,
    pub fov: f32,
}

// ─── CLAMPED CUBIC SPLINE (position / fov) ───────────────────────────────────────

/// Second derivatives (moments M_i = S''(x_i)) of the clamped cubic spline with
/// zero end slopes, solved via the Thomas algorithm. See campath.ts for the moment
/// relation. Uses f64 internally to match the JS reference numerically.
fn clamped_spline_moments(x: &[f64], y: &[f64]) -> Vec<f64> {
    let n = x.len();
    let mut h = vec![0.0; n - 1];
    for i in 0..n - 1 {
        h[i] = x[i + 1] - x[i];
    }

    let mut sub = vec![0.0; n];
    let mut diag = vec![0.0; n];
    let mut sup = vec![0.0; n];
    let mut rhs = vec![0.0; n];

    diag[0] = 2.0 * h[0];
    sup[0] = h[0];
    rhs[0] = 6.0 * ((y[1] - y[0]) / h[0]);

    for i in 1..=n - 2 {
        sub[i] = h[i - 1];
        diag[i] = 2.0 * (h[i - 1] + h[i]);
        sup[i] = h[i];
        rhs[i] = 6.0 * ((y[i + 1] - y[i]) / h[i] - (y[i] - y[i - 1]) / h[i - 1]);
    }

    sub[n - 1] = h[n - 2];
    diag[n - 1] = 2.0 * h[n - 2];
    rhs[n - 1] = 6.0 * (-(y[n - 1] - y[n - 2]) / h[n - 2]);

    // Thomas algorithm.
    let mut cp = vec![0.0; n];
    let mut dp = vec![0.0; n];
    cp[0] = sup[0] / diag[0];
    dp[0] = rhs[0] / diag[0];
    for i in 1..n {
        let m = diag[i] - sub[i] * cp[i - 1];
        cp[i] = sup[i] / m;
        dp[i] = (rhs[i] - sub[i] * dp[i - 1]) / m;
    }

    let mut mm = vec![0.0; n];
    mm[n - 1] = dp[n - 1];
    for i in (0..n - 1).rev() {
        mm[i] = dp[i] - cp[i] * mm[i + 1];
    }
    mm
}

fn eval_cubic_spline(xa: &[f64], ya: &[f64], m: &[f64], t: f64) -> f64 {
    let n = xa.len();
    let (mut klo, mut khi) = (0usize, n - 1);
    while khi - klo > 1 {
        let k = (khi + klo) >> 1;
        if xa[k] > t {
            khi = k;
        } else {
            klo = k;
        }
    }
    let h = xa[khi] - xa[klo];
    let a = (xa[khi] - t) / h;
    let b = (t - xa[klo]) / h;
    a * ya[klo]
        + b * ya[khi]
        + ((a * a * a - a) * m[klo] + (b * b * b - b) * m[khi]) * (h * h / 6.0)
}

fn linear_scalar(xa: &[f64], ya: &[f64], t: f64) -> f64 {
    let n = xa.len();
    if t <= xa[0] {
        return ya[0];
    }
    if t >= xa[n - 1] {
        return ya[n - 1];
    }
    let mut khi = 1;
    while khi < n - 1 && xa[khi] < t {
        khi += 1;
    }
    let klo = khi - 1;
    let h = xa[khi] - xa[klo];
    let f = if h == 0.0 { 0.0 } else { (t - xa[klo]) / h };
    ya[klo] + f * (ya[khi] - ya[klo])
}

// ─── QUATERNION SPLINE (sCubic) ──────────────────────────────────────────────────
// McEnnan qspline, port of CSCubicQuaternionInterpolation. Quaternions are [x,y,z,w].

const QSPLINE_EPS: f64 = 1.0e-6;
type V3 = [f64; 3];

fn v3cross(b: V3, c: V3) -> V3 {
    [
        b[1] * c[2] - b[2] * c[1],
        b[2] * c[0] - b[0] * c[2],
        b[0] * c[1] - b[1] * c[0],
    ]
}

fn unvec(a: V3, out: &mut V3) -> f64 {
    let amag = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
    if amag > 0.0 {
        out[0] = a[0] / amag;
        out[1] = a[1] / amag;
        out[2] = a[2] / amag;
    } else {
        *out = [0.0, 0.0, 0.0];
    }
    amag
}

fn getang(qi: &[f64; 4], qf: &[f64; 4], e: &mut V3) -> f64 {
    let temp: V3 = [
        qi[3] * qf[0] - qi[0] * qf[3] - qi[1] * qf[2] + qi[2] * qf[1],
        qi[3] * qf[1] - qi[1] * qf[3] - qi[2] * qf[0] + qi[0] * qf[2],
        qi[3] * qf[2] - qi[2] * qf[3] - qi[0] * qf[1] + qi[1] * qf[0],
    ];
    let ca = qi[0] * qf[0] + qi[1] * qf[1] + qi[2] * qf[2] + qi[3] * qf[3];
    let sa = unvec(temp, e);
    2.0 * sa.atan2(ca)
}

fn bd(e: V3, dtheta: f64, flag: i32, xin: V3, xout: &mut V3) {
    if dtheta > QSPLINE_EPS {
        let ca = dtheta.cos();
        let sa = dtheta.sin();
        let (b1, b2) = if flag == 0 {
            (0.5 * dtheta * sa / (1.0 - ca), 0.5 * dtheta)
        } else {
            (sa / dtheta, (ca - 1.0) / dtheta)
        };
        let b0 = xin[0] * e[0] + xin[1] * e[1] + xin[2] * e[2];
        let temp2 = v3cross(e, xin);
        let temp1 = v3cross(temp2, e);
        for i in 0..3 {
            xout[i] = b0 * e[i] + b1 * temp1[i] + b2 * temp2[i];
        }
    } else {
        *xout = xin;
    }
}

fn rf(e: V3, dtheta: f64, win: V3, rhs: &mut V3) {
    if dtheta > QSPLINE_EPS {
        let ca = dtheta.cos();
        let sa = dtheta.sin();
        let temp2 = v3cross(e, win);
        let temp1 = v3cross(temp2, e);
        let dot = win[0] * e[0] + win[1] * e[1] + win[2] * e[2];
        let mag = win[0] * win[0] + win[1] * win[1] + win[2] * win[2];
        let c1 = 1.0 - ca;
        let r0 = 0.5 * (mag - dot * dot) * (dtheta - sa) / c1;
        let r1 = dot * (dtheta * sa - 2.0 * c1) / (dtheta * c1);
        for i in 0..3 {
            rhs[i] = r0 * e[i] + r1 * temp1[i];
        }
    } else {
        *rhs = [0.0, 0.0, 0.0];
    }
}

#[allow(clippy::too_many_arguments)]
fn rates(
    n: usize,
    maxit: usize,
    tol: f64,
    wi: V3,
    wf: V3,
    h: &[f64],
    dtheta: &[f64],
    e: &[V3],
    w: &mut [V3],
) {
    let mut a = vec![0.0; n];
    let mut b = vec![0.0; n];
    let mut c = vec![0.0; n];
    let mut wprev = vec![[0.0; 3]; n];
    let mut temp1: V3 = [0.0; 3];
    let mut temp2: V3 = [0.0; 3];

    let mut iter = 0usize;
    let mut dw;
    loop {
        for i in 1..n - 1 {
            wprev[i] = w[i];
        }

        for i in 1..n - 1 {
            a[i] = 2.0 / h[i - 1];
            b[i] = 4.0 / h[i - 1] + 4.0 / h[i];
            c[i] = 2.0 / h[i];

            rf(e[i - 1], dtheta[i - 1], wprev[i], &mut temp1);

            for j in 0..3 {
                w[i][j] = 6.0
                    * (dtheta[i - 1] * e[i - 1][j] / (h[i - 1] * h[i - 1])
                        + dtheta[i] * e[i][j] / (h[i] * h[i]))
                    - temp1[j];
            }
        }

        bd(e[0], dtheta[0], 1, wi, &mut temp1);
        bd(e[n - 2], dtheta[n - 2], 0, wf, &mut temp2);

        for j in 0..3 {
            w[1][j] -= a[1] * temp1[j];
            w[n - 2][j] -= c[n - 2] * temp2[j];
        }

        // Reduce to upper triangular form.
        for i in 1..n - 2 {
            b[i + 1] -= c[i] * a[i + 1] / b[i];
            bd(e[i], dtheta[i], 1, w[i], &mut temp1);
            for j in 0..3 {
                w[i + 1][j] -= temp1[j] * a[i + 1] / b[i];
            }
        }

        // Back substitution.
        for j in 0..3 {
            w[n - 2][j] /= b[n - 2];
        }
        for i in (1..n - 2).rev() {
            bd(e[i], dtheta[i], 0, w[i + 1], &mut temp1);
            for j in 0..3 {
                w[i][j] = (w[i][j] - c[i] * temp1[j]) / b[i];
            }
        }

        dw = 0.0;
        for i in 1..n - 1 {
            for j in 0..3 {
                dw += (w[i][j] - wprev[i][j]) * (w[i][j] - wprev[i][j]);
            }
        }
        dw = dw.sqrt();

        iter += 1;
        if iter >= maxit || dw <= tol {
            break;
        }
    }

    w[0] = wi;
    w[n - 1] = wf;
}

fn slew3_coeffs(dtheta: f64, e: V3, wi_seg: V3, wf_seg: V3) -> (V3, V3, V3) {
    // Note: `dt` (segment duration) cancels out of the evaluated quaternion here
    // because the reference multiplies a0/a1 by dt and slew3_quat re-divides by dt.
    // We fold it out: pass dt=1 semantics (a0=wi_seg, a1=bvec-3*a2).
    let sa = dtheta.sin();
    let ca = dtheta.cos();

    let bvec: V3 = if dtheta > QSPLINE_EPS {
        let c1 = 0.5 * sa * dtheta / (1.0 - ca);
        let c2 = 0.5 * dtheta;
        let b0 = e[0] * wf_seg[0] + e[1] * wf_seg[1] + e[2] * wf_seg[2];
        let bvec2 = v3cross(e, wf_seg);
        let bvec1 = v3cross(bvec2, e);
        [
            b0 * e[0] + c1 * bvec1[0] + c2 * bvec2[0],
            b0 * e[1] + c1 * bvec1[1] + c2 * bvec2[1],
            b0 * e[2] + c1 * bvec1[2] + c2 * bvec2[2],
        ]
    } else {
        wf_seg
    };

    let mut a0: V3 = [0.0; 3];
    let mut a1: V3 = [0.0; 3];
    let mut a2: V3 = [0.0; 3];
    for i in 0..3 {
        a2[i] = e[i] * dtheta;
        a0[i] = wi_seg[i];
        a1[i] = bvec[i] - 3.0 * a2[i];
    }
    (a0, a1, a2)
}

fn slew3_quat(t: f64, dt: f64, qi: &[f64; 4], a0: V3, a1: V3, a2: V3) -> [f64; 4] {
    let x = t / dt;
    let x10 = x - 1.0;
    let x11 = x10 * x10;
    let mut th0: V3 = [0.0; 3];
    for i in 0..3 {
        th0[i] = ((x * a2[i] + x10 * a1[i]) * x + x11 * a0[i]) * x;
    }

    let mut u: V3 = [0.0; 3];
    let ang = unvec(th0, &mut u);
    let ca = (0.5 * ang).cos();
    let sa = (0.5 * ang).sin();

    [
        ca * qi[0] + sa * (u[2] * qi[1] - u[1] * qi[2] + u[0] * qi[3]),
        ca * qi[1] + sa * (-u[2] * qi[0] + u[0] * qi[2] + u[1] * qi[3]),
        ca * qi[2] + sa * (u[1] * qi[0] - u[0] * qi[1] + u[2] * qi[3]),
        ca * qi[3] + sa * (-u[0] * qi[0] - u[1] * qi[1] - u[2] * qi[2]),
    ]
}

struct QSpline {
    x: Vec<f64>,
    y: Vec<[f64; 4]>,
    h: Vec<f64>,
    dtheta: Vec<f64>,
    e: Vec<V3>,
    w: Vec<V3>,
    n: usize,
}

impl QSpline {
    fn new(times: &[f64], quats: &[[f64; 4]]) -> Self {
        let n = times.len();
        let x = times.to_vec();

        // Copy quats, flipping sign to always travel the short way.
        let mut y: Vec<[f64; 4]> = Vec::with_capacity(n);
        for i in 0..n {
            let mut q = quats[i];
            if i > 0 {
                let prev = y[i - 1];
                let dot = q[0] * prev[0] + q[1] * prev[1] + q[2] * prev[2] + q[3] * prev[3];
                if dot < 0.0 {
                    q = [-q[0], -q[1], -q[2], -q[3]];
                }
            }
            y.push(q);
        }

        let mut h = vec![0.0; n - 1];
        let mut dtheta = vec![0.0; n - 1];
        let mut e = vec![[0.0; 3]; n - 1];
        let mut w = vec![[0.0; 3]; n];

        for i in 0..n - 1 {
            h[i] = x[i + 1] - x[i];
        }
        for i in 0..n - 1 {
            let mut ei: V3 = [0.0; 3];
            dtheta[i] = getang(&y[i], &y[i + 1], &mut ei);
            e[i] = ei;
        }

        // HLAE clamps end rates to zero; maxit=2, tol=EPS.
        rates(
            n,
            2,
            QSPLINE_EPS,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            &h,
            &dtheta,
            &e,
            &mut w,
        );

        Self {
            x,
            y,
            h,
            dtheta,
            e,
            w,
            n,
        }
    }

    fn interp(&self, xi: f64) -> [f64; 4] {
        let n = self.n;
        let (mut klo, mut khi) = (0usize, n - 1);
        while khi - klo > 1 {
            let k = (khi + klo) >> 1;
            if self.x[k] > xi {
                khi = k;
            } else {
                klo = k;
            }
        }
        let (a0, a1, a2) = slew3_coeffs(self.dtheta[klo], self.e[klo], self.w[klo], self.w[klo + 1]);
        slew3_quat(xi - self.x[klo], self.h[klo], &self.y[klo], a0, a1, a2)
    }
}

// ─── COMPILED PATH ───────────────────────────────────────────────────────────────

pub struct CompiledCampath {
    interp: CampathInterp,
    t: Vec<f64>,
    px: Vec<f64>,
    py: Vec<f64>,
    pz: Vec<f64>,
    fov: Vec<f64>,
    quats: Vec<[f64; 4]>,
    px2: Option<Vec<f64>>,
    py2: Option<Vec<f64>>,
    pz2: Option<Vec<f64>>,
    fov2: Option<Vec<f64>>,
    cubic_ok: bool,
    qspline: Option<QSpline>,
    count: usize,
}

impl CompiledCampath {
    /// Compile from keyframes (>= 2). Returns None otherwise. Keyframes are sorted
    /// by tick; duplicate ticks must not occur (caller dedupes on capture).
    pub fn compile(keyframes: &[Keyframe], interp: CampathInterp) -> Option<Self> {
        if keyframes.len() < 2 {
            return None;
        }
        let mut sorted = keyframes.to_vec();
        sorted.sort_by_key(|k| k.tick);

        let t: Vec<f64> = sorted.iter().map(|k| k.tick as f64).collect();
        let px: Vec<f64> = sorted.iter().map(|k| k.position[0] as f64).collect();
        let py: Vec<f64> = sorted.iter().map(|k| k.position[1] as f64).collect();
        let pz: Vec<f64> = sorted.iter().map(|k| k.position[2] as f64).collect();
        let fov: Vec<f64> = sorted.iter().map(|k| k.fov as f64).collect();
        let quats: Vec<[f64; 4]> = sorted
            .iter()
            .map(|k| {
                [
                    k.quaternion[0] as f64,
                    k.quaternion[1] as f64,
                    k.quaternion[2] as f64,
                    k.quaternion[3] as f64,
                ]
            })
            .collect();

        let cubic_ok = sorted.len() >= 4;
        let (px2, py2, pz2, fov2) = if cubic_ok {
            (
                Some(clamped_spline_moments(&t, &px)),
                Some(clamped_spline_moments(&t, &py)),
                Some(clamped_spline_moments(&t, &pz)),
                Some(clamped_spline_moments(&t, &fov)),
            )
        } else {
            (None, None, None, None)
        };

        let qspline = if interp.rotation == RotationInterp::SCubic && sorted.len() >= 4 {
            Some(QSpline::new(&t, &quats))
        } else {
            None
        };

        Some(Self {
            interp,
            count: sorted.len(),
            t,
            px,
            py,
            pz,
            fov,
            quats,
            px2,
            py2,
            pz2,
            fov2,
            cubic_ok,
            qspline,
        })
    }

    pub fn tick_range(&self) -> (f64, f64) {
        (self.t[0], self.t[self.count - 1])
    }

    fn eval_scalar(&self, method_cubic: bool, ya: &[f64], y2: &Option<Vec<f64>>, t: f64) -> f64 {
        if method_cubic && self.cubic_ok {
            if let Some(m) = y2 {
                if t <= self.t[0] {
                    return ya[0];
                }
                if t >= self.t[self.count - 1] {
                    return ya[self.count - 1];
                }
                return eval_cubic_spline(&self.t, ya, m, t);
            }
        }
        linear_scalar(&self.t, ya, t)
    }

    fn eval_rotation(&self, t: f64) -> Quat {
        let n = self.count;
        let q = |i: usize| Quat::from_xyzw(
            self.quats[i][0] as f32,
            self.quats[i][1] as f32,
            self.quats[i][2] as f32,
            self.quats[i][3] as f32,
        );
        if t <= self.t[0] {
            return q(0);
        }
        if t >= self.t[n - 1] {
            return q(n - 1);
        }
        if self.interp.rotation == RotationInterp::SCubic {
            if let Some(qs) = &self.qspline {
                let r = qs.interp(t);
                return Quat::from_xyzw(r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32)
                    .normalize();
            }
        }
        // sLinear (and sCubic fallback): shortest-arc slerp.
        let mut khi = 1;
        while khi < n - 1 && self.t[khi] < t {
            khi += 1;
        }
        let klo = khi - 1;
        let h = self.t[khi] - self.t[klo];
        let f = if h == 0.0 { 0.0 } else { (t - self.t[klo]) / h };
        q(klo).slerp(q(khi), f as f32)
    }

    pub fn eval(&self, t: f64) -> Sample {
        let pos_cubic = self.interp.position == PositionInterp::Cubic;
        let fov_cubic = self.interp.fov == FovInterp::Cubic;
        Sample {
            position: Vec3::new(
                self.eval_scalar(pos_cubic, &self.px, &self.px2, t) as f32,
                self.eval_scalar(pos_cubic, &self.py, &self.py2, t) as f32,
                self.eval_scalar(pos_cubic, &self.pz, &self.pz2, t) as f32,
            ),
            quaternion: self.eval_rotation(t),
            fov: self.eval_scalar(fov_cubic, &self.fov, &self.fov2, t) as f32,
        }
    }
}

// ─── HLAE EXPORT ─────────────────────────────────────────────────────────────────

const RAD2DEG: f32 = 180.0 / std::f32::consts::PI;

/// Source QAngle (pitch, yaw, roll degrees) from a Z-up-world camera quaternion
/// (camera looks down local -Z, +Y up). Port of sourceAnglesFromQuaternion.
fn source_angles_from_quaternion(q: Quat) -> (f32, f32, f32) {
    let forward = q * Vec3::new(0.0, 0.0, -1.0);
    let up = q * Vec3::new(0.0, 1.0, 0.0);
    let mut left = up.cross(forward);
    if left.length_squared() > 0.0 {
        left = left.normalize();
    }
    let xy_dist = forward.x.hypot(forward.y);

    let (pitch, yaw, roll);
    if xy_dist > 0.001 {
        yaw = forward.y.atan2(forward.x);
        pitch = (-forward.z).atan2(xy_dist);
        let up_z = left.y * forward.x - left.x * forward.y;
        roll = left.z.atan2(up_z);
    } else {
        yaw = (-left.x).atan2(left.y);
        pitch = (-forward.z).atan2(xy_dist);
        roll = 0.0;
    }
    (pitch * RAD2DEG, yaw * RAD2DEG, roll * RAD2DEG)
}

fn interp_attrs(interp: &CampathInterp) -> String {
    let mut attrs: Vec<&str> = Vec::new();
    match interp.position {
        PositionInterp::Linear => attrs.push("positionInterp=\"linear\""),
        PositionInterp::Cubic => attrs.push("positionInterp=\"cubic\""),
    }
    match interp.rotation {
        RotationInterp::SLinear => attrs.push("rotationInterp=\"sLinear\""),
        RotationInterp::SCubic => attrs.push("rotationInterp=\"sCubic\""),
    }
    match interp.fov {
        FovInterp::Linear => attrs.push("fovInterp=\"linear\""),
        FovInterp::Cubic => attrs.push("fovInterp=\"cubic\""),
    }
    attrs.join(" ")
}

/// `mirv_campath` XML. `interval_per_tick` converts keyframe tick -> seconds.
pub fn to_hlae_campath_xml(
    keyframes: &[Keyframe],
    interp: &CampathInterp,
    interval_per_tick: f32,
) -> String {
    let mut sorted = keyframes.to_vec();
    sorted.sort_by_key(|k| k.tick);

    let attrs = interp_attrs(interp);
    let cam_open = if attrs.is_empty() {
        "<campath>".to_string()
    } else {
        format!("<campath {attrs}>")
    };

    let comment = "<!--Points are in Quake coordinates, meaning x=forward, y=left, z=up and rotation order is first rx, then ry and lastly rz.\n\
Rotation direction follows the right-hand grip rule.\n\
rx (roll), ry (pitch), rz(yaw) are the Euler angles in degrees.\n\
qw, qx, qy, qz are the quaternion values.\n\
When read it is sufficient that either rx, ry, rz OR qw, qx, qy, qz are present.\n\
If both are present then qw, qx, qy, qz take precedence.-->";

    let points: Vec<String> = sorted
        .iter()
        .map(|kf| {
            let q = Quat::from_xyzw(
                kf.quaternion[0],
                kf.quaternion[1],
                kf.quaternion[2],
                kf.quaternion[3],
            );
            let (pitch, yaw, roll) = source_angles_from_quaternion(q);
            let t = kf.tick as f32 * interval_per_tick;
            format!(
                "\t\t<p t=\"{:.6}\" x=\"{:.6}\" y=\"{:.6}\" z=\"{:.6}\" fov=\"{:.6}\" \
                 rx=\"{:.6}\" ry=\"{:.6}\" rz=\"{:.6}\" \
                 qw=\"{:.6}\" qx=\"{:.6}\" qy=\"{:.6}\" qz=\"{:.6}\"/>",
                t,
                kf.position[0],
                kf.position[1],
                kf.position[2],
                kf.fov,
                roll,
                pitch,
                yaw,
                q.w,
                q.x,
                q.y,
                q.z,
            )
        })
        .collect();

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{cam_open}\n\t<points>\n\t\t{comment}\n{}\n\t</points>\n</campath>\n",
        points.join("\n")
    )
}

/// Minimal VDM that loads + enables the campath at the first keyframe's tick.
pub fn to_vdm(keyframes: &[Keyframe], campath_file_name: &str) -> String {
    let start_tick = keyframes.iter().map(|k| k.tick).min().unwrap_or(0);
    let bare: String = campath_file_name
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"')
        .collect();
    let commands = format!("mirv_campath clear; mirv_campath load {bare}; mirv_campath enabled 1");
    format!(
        "demoactions\n{{\n\t\"1\"\n\t{{\n\t\tfactory \"PlayCommands\"\n\t\tname \"Load campath\"\n\t\tstarttick \"{start_tick}\"\n\t\tcommands \"{commands}\"\n\t}}\n}}\n"
    )
}
