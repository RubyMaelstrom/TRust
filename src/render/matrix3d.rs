//! 4x4 transform matrices: CSS Transforms 2 #mathematical-description,
//! #decomposing-a-3d-matrix, #interpolation-of-decomposed-3d-matrix-values
//! and #recomposing-to-a-3d-matrix (local CSSWG snapshot 81c27f686901).
//!
//! Matrices are column-major, as `matrix3d()` lists them: element `[c * 4 +
//! r]` is row `r` of column `c`, so `m[12..15]` is the translation. A box
//! whose `transform-style` is `flat` (the only style TRust offers) is
//! flattened into its parent's plane, which for an affine 3D matrix is the
//! orthographic projection `matrix(m[0], m[1], m[4], m[5], m[12], m[13])`.

use super::Affine2d;
use crate::core::CssPoint;

pub(crate) type Matrix3d = [f32; 16];

pub(crate) const IDENTITY: Matrix3d = [
    1., 0., 0., 0., //
    0., 1., 0., 0., //
    0., 0., 1., 0., //
    0., 0., 0., 1.,
];

/// `a` post-multiplied by `b` (a · b).
pub(crate) fn multiply(a: &Matrix3d, b: &Matrix3d) -> Matrix3d {
    std::array::from_fn(|index| {
        let (column, row) = (index / 4, index % 4);
        (0..4).map(|k| a[k * 4 + row] * b[column * 4 + k]).sum()
    })
}

pub(crate) fn from_affine(m: Affine2d) -> Matrix3d {
    let [a, b, c, d, e, f] = m.0;
    [
        a, b, 0., 0., //
        c, d, 0., 0., //
        0., 0., 1., 0., //
        e, f, 0., 1.,
    ]
}

pub(crate) fn translate(x: f32, y: f32, z: f32) -> Matrix3d {
    let mut m = IDENTITY;
    [m[12], m[13], m[14]] = [x, y, z];
    m
}

pub(crate) fn scale(x: f32, y: f32, z: f32) -> Matrix3d {
    let mut m = IDENTITY;
    [m[0], m[5], m[10]] = [x, y, z];
    m
}

/// Transforms 2 #funcdef-perspective: a depth below 1px is treated as 1px;
/// an infinite depth (`none`) is the identity.
pub(crate) fn perspective(depth: f32) -> Matrix3d {
    let mut m = IDENTITY;
    if depth.is_finite() {
        m[11] = -1. / depth.max(1.);
    }
    m
}

/// Transforms 2 #Rotate3dDefined: `radians` about the normalized
/// `[x, y, z]`; a vector that cannot be normalized applies no rotation.
pub(crate) fn rotate(x: f32, y: f32, z: f32, radians: f32) -> Matrix3d {
    let length = (x * x + y * y + z * z).sqrt();
    if length == 0. || !length.is_finite() {
        return IDENTITY;
    }
    let (x, y, z) = (x / length, y / length, z / length);
    let (s, c) = radians.sin_cos();
    let t = 1. - c;
    [
        x * x * t + c,
        x * y * t + z * s,
        x * z * t - y * s,
        0.,
        x * y * t - z * s,
        y * y * t + c,
        y * z * t + x * s,
        0.,
        x * z * t + y * s,
        y * z * t - x * s,
        z * z * t + c,
        0.,
        0.,
        0.,
        0.,
        1.,
    ]
}

/// Whether `m` is a 2D matrix: one `matrix()` could express it.
pub(crate) fn is_2d(m: &Matrix3d) -> bool {
    m[2] == 0.
        && m[3] == 0.
        && m[6] == 0.
        && m[7] == 0.
        && m[8] == 0.
        && m[9] == 0.
        && m[10] == 1.
        && m[11] == 0.
        && m[14] == 0.
        && m[15] == 1.
}

/// The plane `z = 0` under `m`, flattened into the parent's plane. An affine
/// matrix projects orthographically, which is exact. A perspective matrix
/// maps the plane by a projective homography, which a 2D affine renderer
/// cannot represent; it takes the homography's first-order (tangent)
/// approximation at `about`, exact at that point and its local scale.
pub(crate) fn flatten(m: &Matrix3d, about: CssPoint) -> Affine2d {
    let (w0, wx, wy) = (m[15], m[3], m[7]);
    if wx == 0. && wy == 0. {
        let w = if w0 != 0. && w0.is_finite() { w0 } else { 1. };
        return Affine2d([m[0] / w, m[1] / w, m[4] / w, m[5] / w, m[12] / w, m[13] / w]);
    }
    let (x, y) = (about.x, about.y);
    let w = wx * x + wy * y + w0;
    if w.abs() < 1e-6 || !w.is_finite() {
        // The point is projected to infinity: nothing renders.
        return Affine2d([0., 0., 0., 0., 0., 0.]);
    }
    let px = m[0] * x + m[4] * y + m[12];
    let py = m[1] * x + m[5] * y + m[13];
    let (hx, hy) = (px / w, py / w);
    // ∂(p/w)/∂x = (∂p/∂x · w − p · ∂w/∂x) / w².
    let w2 = w * w;
    let a = (m[0] * w - px * wx) / w2;
    let b = (m[1] * w - py * wx) / w2;
    let c = (m[4] * w - px * wy) / w2;
    let d = (m[5] * w - py * wy) / w2;
    Affine2d([a, b, c, d, hx - a * x - c * y, hy - b * x - d * y])
}

/// Transforms 2 #decomposing-a-3d-matrix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Decomposed {
    pub translate: [f32; 3],
    pub scale: [f32; 3],
    pub skew: [f32; 3],
    pub perspective: [f32; 4],
    pub quaternion: [f32; 4],
}

type Row = [f32; 3];

fn dot(a: Row, b: Row) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn length(a: Row) -> f32 {
    dot(a, a).sqrt()
}

fn normalize(a: Row) -> Row {
    let l = length(a);
    if l == 0. {
        a
    } else {
        [a[0] / l, a[1] / l, a[2] / l]
    }
}

fn combine(a: Row, b: Row, ascl: f32, bscl: f32) -> Row {
    std::array::from_fn(|i| a[i] * ascl + b[i] * bscl)
}

fn cross(a: Row, b: Row) -> Row {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// The pseudo-code's `matrix[i][j]`: row `i` of its row-major convention,
/// which is our column `i`, row `j`.
fn at(m: &Matrix3d, i: usize, j: usize) -> f32 {
    m[i * 4 + j]
}

fn determinant(m: &Matrix3d) -> f64 {
    let m: [f64; 16] = m.map(f64::from);
    let sub = |r0: usize, r1: usize, c0: usize, c1: usize| {
        m[c0 * 4 + r0] * m[c1 * 4 + r1] - m[c1 * 4 + r0] * m[c0 * 4 + r1]
    };
    // Laplace expansion by 2x2 minors of the first two and last two rows.
    sub(0, 1, 0, 1) * sub(2, 3, 2, 3) - sub(0, 1, 0, 2) * sub(2, 3, 1, 3)
        + sub(0, 1, 0, 3) * sub(2, 3, 1, 2)
        + sub(0, 1, 1, 2) * sub(2, 3, 0, 3)
        - sub(0, 1, 1, 3) * sub(2, 3, 0, 2)
        + sub(0, 1, 2, 3) * sub(2, 3, 0, 1)
}

fn inverse(m: &Matrix3d) -> Option<Matrix3d> {
    let det = determinant(m);
    if det == 0. || !det.is_finite() {
        return None;
    }
    // Adjugate by cofactors, in f64.
    let m64: [f64; 16] = m.map(f64::from);
    let get = |row: usize, column: usize| m64[column * 4 + row];
    let minor = |row: usize, column: usize| {
        let rows: Vec<usize> = (0..4).filter(|&r| r != row).collect();
        let columns: Vec<usize> = (0..4).filter(|&c| c != column).collect();
        let e = |r: usize, c: usize| get(rows[r], columns[c]);
        e(0, 0) * (e(1, 1) * e(2, 2) - e(1, 2) * e(2, 1))
            - e(0, 1) * (e(1, 0) * e(2, 2) - e(1, 2) * e(2, 0))
            + e(0, 2) * (e(1, 0) * e(2, 1) - e(1, 1) * e(2, 0))
    };
    Some(std::array::from_fn(|index| {
        let (column, row) = (index / 4, index % 4);
        // inverse[row][column] = cofactor[column][row] / det.
        let sign = if (row + column) % 2 == 0 { 1. } else { -1. };
        (sign * minor(column, row) / det) as f32
    }))
}

/// Transforms 2 #decomposing-a-3d-matrix; `None` when it cannot be
/// decomposed.
pub(crate) fn decompose(matrix: &Matrix3d) -> Option<Decomposed> {
    let mut matrix = *matrix;
    if matrix[15] == 0. {
        return None;
    }
    let w = matrix[15];
    for value in &mut matrix {
        *value /= w;
    }
    let mut perspective_matrix = matrix;
    for i in 0..3 {
        perspective_matrix[i * 4 + 3] = 0.;
    }
    perspective_matrix[15] = 1.;
    if determinant(&perspective_matrix) == 0. {
        return None;
    }
    let perspective =
        if at(&matrix, 0, 3) != 0. || at(&matrix, 1, 3) != 0. || at(&matrix, 2, 3) != 0. {
            let rhs = [
                at(&matrix, 0, 3),
                at(&matrix, 1, 3),
                at(&matrix, 2, 3),
                at(&matrix, 3, 3),
            ];
            let inverse = inverse(&perspective_matrix)?;
            // multVecMatrix(rhs, transpose(inverse)): result[j] = Σ_i rhs[i] ·
            // transpose[i][j] = Σ_i rhs[i] · inverse[j][i].
            std::array::from_fn(|j| (0..4).map(|i| rhs[i] * at(&inverse, j, i)).sum())
        } else {
            [0., 0., 0., 1.]
        };
    let translate = [at(&matrix, 3, 0), at(&matrix, 3, 1), at(&matrix, 3, 2)];
    let mut row: [Row; 3] =
        std::array::from_fn(|i| [at(&matrix, i, 0), at(&matrix, i, 1), at(&matrix, i, 2)]);
    let mut scale = [0.; 3];
    let mut skew = [0.; 3];
    scale[0] = length(row[0]);
    row[0] = normalize(row[0]);
    skew[0] = dot(row[0], row[1]);
    row[1] = combine(row[1], row[0], 1., -skew[0]);
    scale[1] = length(row[1]);
    row[1] = normalize(row[1]);
    if scale[1] != 0. {
        skew[0] /= scale[1];
    }
    skew[1] = dot(row[0], row[2]);
    row[2] = combine(row[2], row[0], 1., -skew[1]);
    skew[2] = dot(row[1], row[2]);
    row[2] = combine(row[2], row[1], 1., -skew[2]);
    scale[2] = length(row[2]);
    row[2] = normalize(row[2]);
    if scale[2] != 0. {
        skew[1] /= scale[2];
        skew[2] /= scale[2];
    }
    if dot(row[0], cross(row[1], row[2])) < 0. {
        for i in 0..3 {
            scale[i] *= -1.;
            for value in &mut row[i] {
                *value *= -1.;
            }
        }
    }
    let mut quaternion = [
        0.5 * (1. + row[0][0] - row[1][1] - row[2][2]).max(0.).sqrt(),
        0.5 * (1. - row[0][0] + row[1][1] - row[2][2]).max(0.).sqrt(),
        0.5 * (1. - row[0][0] - row[1][1] + row[2][2]).max(0.).sqrt(),
        0.5 * (1. + row[0][0] + row[1][1] + row[2][2]).max(0.).sqrt(),
    ];
    if row[2][1] > row[1][2] {
        quaternion[0] = -quaternion[0];
    }
    if row[0][2] > row[2][0] {
        quaternion[1] = -quaternion[1];
    }
    if row[1][0] > row[0][1] {
        quaternion[2] = -quaternion[2];
    }
    Some(Decomposed {
        translate,
        scale,
        skew,
        perspective,
        quaternion,
    })
}

/// Transforms 2 #interpolation-of-decomposed-3d-matrix-values.
pub(crate) fn interpolate_decomposed(a: &Decomposed, b: &Decomposed, t: f32) -> Decomposed {
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    let mix3 = |x: [f32; 3], y: [f32; 3]| std::array::from_fn(|i| lerp(x[i], y[i]));
    let mut product = (0..4)
        .map(|i| a.quaternion[i] * b.quaternion[i])
        .sum::<f32>();
    product = product.clamp(-1., 1.);
    let quaternion = if (product.abs() - 1.).abs() < f32::EPSILON {
        a.quaternion
    } else {
        let theta = product.acos();
        let w = (t * theta).sin() / (1. - product * product).sqrt();
        let scale_a = (t * theta).cos() - product * w;
        std::array::from_fn(|i| a.quaternion[i] * scale_a + b.quaternion[i] * w)
    };
    Decomposed {
        translate: mix3(a.translate, b.translate),
        scale: mix3(a.scale, b.scale),
        skew: mix3(a.skew, b.skew),
        perspective: std::array::from_fn(|i| lerp(a.perspective[i], b.perspective[i])),
        quaternion,
    }
}

/// Transforms 2 #recomposing-to-a-3d-matrix. The pseudo-code's arrays hold
/// the transposes of these column-vector matrices (its decomposition reads
/// the basis images as rows); in column-vector terms the decomposition
/// factors `M = P · T · R · Kᵀ · S`: the perspective row `P`, translation,
/// the quaternion's rotation, the XY/XZ/YZ skews above the diagonal, and
/// scale. Recompose that product.
pub(crate) fn recompose(d: &Decomposed) -> Matrix3d {
    let mut perspective = IDENTITY;
    [
        perspective[3],
        perspective[7],
        perspective[11],
        perspective[15],
    ] = d.perspective;
    let [x, y, z, w] = d.quaternion;
    let mut rotation = IDENTITY;
    // Storage index `column * 4 + row` of R[row][column].
    rotation[0] = 1. - 2. * (y * y + z * z);
    rotation[4] = 2. * (x * y - z * w);
    rotation[8] = 2. * (x * z + y * w);
    rotation[1] = 2. * (x * y + z * w);
    rotation[5] = 1. - 2. * (x * x + z * z);
    rotation[9] = 2. * (y * z - x * w);
    rotation[2] = 2. * (x * z - y * w);
    rotation[6] = 2. * (y * z + x * w);
    rotation[10] = 1. - 2. * (x * x + y * y);
    let mut skew = IDENTITY;
    skew[4] = d.skew[0];
    skew[8] = d.skew[1];
    skew[9] = d.skew[2];
    let [tx, ty, tz] = d.translate;
    let [sx, sy, sz] = d.scale;
    [&translate(tx, ty, tz), &rotation, &skew, &scale(sx, sy, sz)]
        .into_iter()
        .fold(perspective, |matrix, factor| multiply(&matrix, factor))
}

/// CSS Transforms 2 #matrix-interpolation of two matrices, or `None` when
/// either cannot be decomposed (the element is not rendered then).
pub(crate) fn interpolate(a: &Matrix3d, b: &Matrix3d, t: f32) -> Option<Matrix3d> {
    let (a, b) = (decompose(a)?, decompose(b)?);
    Some(recompose(&interpolate_decomposed(&a, &b, t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &Matrix3d, b: &Matrix3d) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn decomposition_recomposes_the_matrix() {
        let m = multiply(
            &multiply(&translate(10., -4., 3.), &rotate(1., 2., 3., 0.7)),
            &multiply(&scale(2., 0.5, 1.5), &perspective(400.)),
        );
        let recomposed = recompose(&decompose(&m).unwrap());
        assert!(close(&m, &recomposed), "{m:?}\n{recomposed:?}");
        // Halfway between two rotations about one axis is the half angle.
        let half = interpolate(&rotate(0., 1., 0., 0.), &rotate(0., 1., 0., 2.), 0.5).unwrap();
        assert!(close(&half, &rotate(0., 1., 0., 1.)), "{half:?}");
    }

    #[test]
    fn flat_boxes_project_orthographically() {
        // rotateY(60deg) halves the width; rotateX(60deg) the height.
        let flat = flatten(&rotate(0., 1., 0., 60f32.to_radians()), CssPoint::default());
        assert!((flat.0[0] - 0.5).abs() < 1e-6 && (flat.0[3] - 1.).abs() < 1e-6);
        let flat = flatten(&rotate(1., 0., 0., 60f32.to_radians()), CssPoint::default());
        assert!((flat.0[0] - 1.).abs() < 1e-6 && (flat.0[3] - 0.5).abs() < 1e-6);
        // Translation in z has no effect without perspective.
        assert!(flatten(&translate(0., 0., 50.), CssPoint::default()).is_identity());
        // A projected box is exact at the tangent point.
        let m = multiply(&perspective(100.), &translate(0., 0., -100.));
        let flat = flatten(&m, CssPoint::new(10., 0.));
        let p = flat.map_point(CssPoint::new(10., 0.));
        assert!((p.x - 5.).abs() < 1e-4 && p.y.abs() < 1e-4, "{p:?}");
    }
}
