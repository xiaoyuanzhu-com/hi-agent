//! InsightFace face vendor — local ONNX face detection and embedding, no network.
//!
//! One vendor owns the whole face pipeline so the capability surface stays
//! "image in → faces out" with no cross-capability reference. It runs the
//! InsightFace `buffalo_l` pair (the same models Immich uses):
//!
//!   decode image → SCRFD detect (bbox + 5 landmarks) → 5-point align to
//!   112×112 → ArcFace embed → L2-normalized 512-d vector, per face
//!
//! Preprocessing follows InsightFace exactly, because the models were trained on
//! it: SCRFD takes a letter-boxed 640×640 RGB image normalized `(x−127.5)/128`,
//! emits per-stride (8/16/32) score/bbox/keypoint maps with 2 anchors per cell,
//! decoded via distance-to-box/point and merged with NMS. The ArcFace recognizer
//! takes the aligned crop normalized `(x−127.5)/127.5` and emits a 512-d
//! embedding. The recognizer is swappable for any InsightFace 112×112→512 model
//! (e.g. `w600k_r50` for buffalo_l, `w600k_mbf` for buffalo_s) via env — the
//! pipeline is model-agnostic.
//!
//! Both [`ort::session::Session`]s sit behind a [`Mutex`] (run needs `&mut`);
//! inference is synchronous CPU work the capability layer runs on a blocking
//! thread.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use image::RgbImage;
use ort::session::Session;
use ort::value::Tensor;

use crate::body::capabilities::face::DetectedFace;

/// SCRFD square input edge. InsightFace's standard detector input.
const DET_SIZE: usize = 640;
/// Feature-map strides SCRFD emits, in output order.
const STRIDES: [usize; 3] = [8, 16, 32];
/// Anchors per feature-map cell (SCRFD `_bnkps` models).
const NUM_ANCHORS: usize = 2;
/// ArcFace recognizer input edge.
const REC_SIZE: usize = 112;
/// Default detection score floor and NMS IoU.
const SCORE_THRESH: f32 = 0.5;
const NMS_THRESH: f32 = 0.4;

/// ArcFace reference 5-point landmark template for a 112×112 crop (left eye,
/// right eye, nose, left mouth, right mouth) — what the recognizer expects.
const REF_LANDMARKS: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

pub struct Config {
    scrfd: Mutex<Session>,
    arcface: Mutex<Session>,
    #[allow(dead_code)]
    scrfd_path: PathBuf,
    #[allow(dead_code)]
    arcface_path: PathBuf,
}

impl Config {
    /// Load both ONNX models auto-provisioned by [`crate::models`]: `scrfd_path`
    /// (detector) and `arcface_path` (recognizer). Fails if either file can't be
    /// opened as a model (a real pin/corruption bug), so it surfaces at startup.
    pub fn load(scrfd_path: &Path, arcface_path: &Path) -> anyhow::Result<Self> {
        let scrfd = Session::builder()
            .context("creating ORT session builder")?
            .commit_from_file(scrfd_path)
            .with_context(|| format!("loading SCRFD model from {}", scrfd_path.display()))?;
        let arcface = Session::builder()
            .context("creating ORT session builder")?
            .commit_from_file(arcface_path)
            .with_context(|| format!("loading ArcFace recognizer from {}", arcface_path.display()))?;
        Ok(Self {
            scrfd: Mutex::new(scrfd),
            arcface: Mutex::new(arcface),
            scrfd_path: scrfd_path.to_path_buf(),
            arcface_path: arcface_path.to_path_buf(),
        })
    }
}

/// Detect every face in `image_bytes` and embed each one. Returns one
/// [`DetectedFace`] per kept detection (bbox + landmarks in original-image
/// pixels, score, and an L2-normalized 512-d embedding). Synchronous CPU work.
pub fn detect_and_embed(cfg: &Config, image_bytes: &[u8]) -> anyhow::Result<Vec<DetectedFace>> {
    let img = image::load_from_memory(image_bytes)
        .context("decoding image")?
        .to_rgb8();

    let dets = detect(cfg, &img)?;
    let mut faces = Vec::with_capacity(dets.len());
    for d in dets {
        let crop = align_to_crop(&img, &d.landmarks);
        let embedding = embed_crop(cfg, &crop)?;
        faces.push(DetectedFace {
            bbox: d.bbox,
            landmarks: d.landmarks,
            score: d.score,
            embedding,
        });
    }
    Ok(faces)
}

/// A raw detection before embedding: pixel-space bbox `[x1,y1,x2,y2]`,
/// 5 landmarks, and score — all already mapped back to original-image scale.
struct Detection {
    bbox: [f32; 4],
    landmarks: [[f32; 2]; 5],
    score: f32,
}

fn detect(cfg: &Config, img: &RgbImage) -> anyhow::Result<Vec<Detection>> {
    let (input, det_scale) = preprocess_det(img);
    let tensor = Tensor::from_array((
        vec![1_i64, 3, DET_SIZE as i64, DET_SIZE as i64],
        input,
    ))
    .context("building SCRFD input tensor")?;

    // Collect every output's flat data up front: the SessionOutputs borrow ends
    // when the guard drops, so we copy out before decoding.
    let outs: Vec<Vec<f32>> = {
        let mut session = cfg.scrfd.lock().expect("SCRFD session mutex poisoned");
        let outputs = session.run(ort::inputs![tensor]).context("SCRFD inference")?;
        let n = outputs.len();
        if n != STRIDES.len() * 3 {
            anyhow::bail!(
                "SCRFD model has {n} outputs; expected {} (a keypoint model: score/bbox/kps × 3 strides)",
                STRIDES.len() * 3
            );
        }
        (0..n)
            .map(|i| {
                outputs[i]
                    .try_extract_tensor::<f32>()
                    .map(|(_s, d)| d.to_vec())
                    .context("extracting SCRFD output")
            })
            .collect::<anyhow::Result<_>>()?
    };

    let fmc = STRIDES.len();
    let mut dets = Vec::new();
    for (idx, &stride) in STRIDES.iter().enumerate() {
        let scores = &outs[idx];
        let bbox_preds = &outs[idx + fmc];
        let kps_preds = &outs[idx + fmc * 2];

        let centers = anchor_centers(stride);
        if scores.len() != centers.len() {
            anyhow::bail!(
                "SCRFD stride {stride}: {} scores for {} anchors (output order/model mismatch)",
                scores.len(),
                centers.len()
            );
        }

        for (i, &c) in centers.iter().enumerate() {
            let score = scores[i];
            if score < SCORE_THRESH {
                continue;
            }
            let bbox = distance2bbox(c, &bbox_preds[i * 4..i * 4 + 4], stride as f32);
            let landmarks = distance2kps(c, &kps_preds[i * 10..i * 10 + 10], stride as f32);
            dets.push(Detection { bbox, landmarks, score });
        }
    }

    let keep = nms(&dets, NMS_THRESH);
    Ok(keep
        .into_iter()
        .map(|i| {
            let mut d = Detection {
                bbox: dets[i].bbox,
                landmarks: dets[i].landmarks,
                score: dets[i].score,
            };
            // Map detections from the letter-boxed 640×640 back to original pixels.
            for v in d.bbox.iter_mut() {
                *v /= det_scale;
            }
            for p in d.landmarks.iter_mut() {
                p[0] /= det_scale;
                p[1] /= det_scale;
            }
            d
        })
        .collect())
}

/// Letter-box `img` into a 640×640 RGB canvas (top-left, aspect preserved) and
/// pack it as NCHW f32 normalized `(x−127.5)/128`. Returns the tensor data and
/// the scale factor mapping original → resized.
fn preprocess_det(img: &RgbImage) -> (Vec<f32>, f32) {
    let (w, h) = img.dimensions();
    let scale = (DET_SIZE as f32 / w as f32).min(DET_SIZE as f32 / h as f32);
    let new_w = (w as f32 * scale).round() as u32;
    let new_h = (h as f32 * scale).round() as u32;
    let resized = image::imageops::resize(img, new_w, new_h, image::imageops::FilterType::Triangle);

    // NCHW, RGB, zero-padded to DET_SIZE².
    let plane = DET_SIZE * DET_SIZE;
    let mut data = vec![-127.5 / 128.0; 3 * plane]; // padding pixels = (0−127.5)/128
    for y in 0..new_h as usize {
        for x in 0..new_w as usize {
            let px = resized.get_pixel(x as u32, y as u32);
            let off = y * DET_SIZE + x;
            for c in 0..3 {
                data[c * plane + off] = (px[c] as f32 - 127.5) / 128.0;
            }
        }
    }
    (data, scale)
}

/// Anchor centers for one stride: a `(DET_SIZE/stride)²` grid of cell centers,
/// each repeated `NUM_ANCHORS` times, in row-major (y, x, anchor) order — the
/// order SCRFD's outputs are flattened in.
fn anchor_centers(stride: usize) -> Vec<[f32; 2]> {
    let n = DET_SIZE / stride;
    let mut centers = Vec::with_capacity(n * n * NUM_ANCHORS);
    for y in 0..n {
        for x in 0..n {
            let c = [(x * stride) as f32, (y * stride) as f32];
            for _ in 0..NUM_ANCHORS {
                centers.push(c);
            }
        }
    }
    centers
}

/// Decode a left/top/right/bottom distance prediction (in stride units) into a
/// pixel-space `[x1,y1,x2,y2]` box around the anchor center.
fn distance2bbox(c: [f32; 2], d: &[f32], stride: f32) -> [f32; 4] {
    [
        c[0] - d[0] * stride,
        c[1] - d[1] * stride,
        c[0] + d[2] * stride,
        c[1] + d[3] * stride,
    ]
}

/// Decode 5 landmark offset pairs (in stride units) into pixel-space points.
fn distance2kps(c: [f32; 2], d: &[f32], stride: f32) -> [[f32; 2]; 5] {
    let mut out = [[0.0_f32; 2]; 5];
    for k in 0..5 {
        out[k] = [c[0] + d[2 * k] * stride, c[1] + d[2 * k + 1] * stride];
    }
    out
}

/// Greedy IoU NMS. Returns indices into `dets` to keep, highest score first.
fn nms(dets: &[Detection], iou_thresh: f32) -> Vec<usize> {
    let mut order: Vec<usize> = (0..dets.len()).collect();
    order.sort_by(|&a, &b| dets[b].score.partial_cmp(&dets[a].score).unwrap_or(std::cmp::Ordering::Equal));

    let mut keep = Vec::new();
    let mut suppressed = vec![false; dets.len()];
    for i in 0..order.len() {
        let a = order[i];
        if suppressed[a] {
            continue;
        }
        keep.push(a);
        for &b in order.iter().skip(i + 1) {
            if !suppressed[b] && iou(&dets[a].bbox, &dets[b].bbox) > iou_thresh {
                suppressed[b] = true;
            }
        }
    }
    keep
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let x1 = a[0].max(b[0]);
    let y1 = a[1].max(b[1]);
    let x2 = a[2].min(b[2]);
    let y2 = a[3].min(b[3]);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let area_a = (a[2] - a[0]).max(0.0) * (a[3] - a[1]).max(0.0);
    let area_b = (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0);
    let union = area_a + area_b - inter;
    if union > 0.0 {
        inter / union
    } else {
        0.0
    }
}

/// Warp the face into a 112×112 RGB crop using the similarity transform that
/// best maps its 5 landmarks onto the ArcFace reference template. Returns the
/// crop as NCHW f32 normalized `(x−127.5)/127.5`, ready for the recognizer.
fn align_to_crop(img: &RgbImage, landmarks: &[[f32; 2]; 5]) -> Vec<f32> {
    // Forward maps source landmarks → reference; invert it to sample the source
    // for each destination pixel.
    let m = umeyama(landmarks, &REF_LANDMARKS);
    let inv = invert_affine(&m);

    let (w, h) = img.dimensions();
    let plane = REC_SIZE * REC_SIZE;
    let mut data = vec![0.0_f32; 3 * plane];
    for dy in 0..REC_SIZE {
        for dx in 0..REC_SIZE {
            let sx = inv[0][0] * dx as f32 + inv[0][1] * dy as f32 + inv[0][2];
            let sy = inv[1][0] * dx as f32 + inv[1][1] * dy as f32 + inv[1][2];
            let rgb = sample_bilinear(img, sx, sy, w, h);
            let off = dy * REC_SIZE + dx;
            for c in 0..3 {
                data[c * plane + off] = (rgb[c] - 127.5) / 127.5;
            }
        }
    }
    data
}

fn embed_crop(cfg: &Config, crop_nchw: &[f32]) -> anyhow::Result<Vec<f32>> {
    let tensor = Tensor::from_array((
        vec![1_i64, 3, REC_SIZE as i64, REC_SIZE as i64],
        crop_nchw.to_vec(),
    ))
    .context("building recognizer input tensor")?;
    let mut session = cfg.arcface.lock().expect("ArcFace session mutex poisoned");
    let outputs = session.run(ort::inputs![tensor]).context("ArcFace inference")?;
    let (_shape, emb) = outputs[0]
        .try_extract_tensor::<f32>()
        .context("extracting face embedding")?;
    Ok(l2_normalize(emb))
}

/// Bilinearly sample an RGB pixel at fractional `(x, y)`, clamping to the image
/// edge. Out-of-range coordinates (from the inverse warp) read the border.
fn sample_bilinear(img: &RgbImage, x: f32, y: f32, w: u32, h: u32) -> [f32; 3] {
    let x = x.clamp(0.0, (w - 1) as f32);
    let y = y.clamp(0.0, (h - 1) as f32);
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;

    let p = |px: u32, py: u32| img.get_pixel(px, py);
    let mut out = [0.0_f32; 3];
    for c in 0..3 {
        let top = p(x0, y0)[c] as f32 * (1.0 - fx) + p(x1, y0)[c] as f32 * fx;
        let bot = p(x0, y1)[c] as f32 * (1.0 - fx) + p(x1, y1)[c] as f32 * fx;
        out[c] = top * (1.0 - fy) + bot * fy;
    }
    out
}

/// Estimate the 2D similarity transform (uniform scale + rotation + translation,
/// no reflection) mapping `src` onto `dst`, returned as a 2×3 affine
/// `[[a,b,tx],[c,d,ty]]`. Umeyama's closed form.
fn umeyama(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> [[f32; 3]; 2] {
    use nalgebra::{Matrix2, Vector2};

    let n = src.len() as f32;
    let mean = |pts: &[[f32; 2]; 5]| {
        let mut m = Vector2::zeros();
        for p in pts {
            m += Vector2::new(p[0], p[1]);
        }
        m / n
    };
    let mu_src = mean(src);
    let mu_dst = mean(dst);

    let mut cov = Matrix2::zeros();
    let mut var_src = 0.0_f32;
    for k in 0..src.len() {
        let xs = Vector2::new(src[k][0], src[k][1]) - mu_src;
        let xd = Vector2::new(dst[k][0], dst[k][1]) - mu_dst;
        cov += xd * xs.transpose();
        var_src += xs.norm_squared();
    }
    cov /= n;
    var_src /= n;

    let svd = cov.svd(true, true);
    let u = svd.u.unwrap();
    let v_t = svd.v_t.unwrap();
    let mut s = Matrix2::identity();
    if u.determinant() * v_t.determinant() < 0.0 {
        s[(1, 1)] = -1.0;
    }
    let r = u * s * v_t;

    let d = Vector2::new(svd.singular_values[0], svd.singular_values[1]);
    let scale = if var_src > 0.0 {
        (d[0] * s[(0, 0)] + d[1] * s[(1, 1)]) / var_src
    } else {
        1.0
    };

    let t = mu_dst - r * mu_src * scale;
    let rs = r * scale;
    [[rs[(0, 0)], rs[(0, 1)], t[0]], [rs[(1, 0)], rs[(1, 1)], t[1]]]
}

/// Invert a 2×3 affine transform.
fn invert_affine(m: &[[f32; 3]; 2]) -> [[f32; 3]; 2] {
    let (a, b, tx) = (m[0][0], m[0][1], m[0][2]);
    let (c, d, ty) = (m[1][0], m[1][1], m[1][2]);
    let det = a * d - b * c;
    let inv_det = if det != 0.0 { 1.0 / det } else { 0.0 };
    let (ia, ib) = (d * inv_det, -b * inv_det);
    let (ic, id) = (-c * inv_det, a * inv_det);
    [[ia, ib, -(ia * tx + ib * ty)], [ic, id, -(ic * tx + id * ty)]]
}

fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance2bbox_expands_around_the_center() {
        let b = distance2bbox([10.0, 20.0], &[1.0, 1.0, 2.0, 3.0], 1.0);
        assert_eq!(b, [9.0, 19.0, 12.0, 23.0]);
    }

    #[test]
    fn distance2kps_offsets_each_point_by_stride() {
        let k = distance2kps([0.0, 0.0], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0], 2.0);
        assert_eq!(k[0], [2.0, 4.0]);
        assert_eq!(k[4], [18.0, 20.0]);
    }

    #[test]
    fn anchor_centers_have_the_expected_count_and_first_cells() {
        let c = anchor_centers(32); // 20×20 grid × 2 anchors
        assert_eq!(c.len(), 20 * 20 * NUM_ANCHORS);
        assert_eq!(c[0], [0.0, 0.0]);
        assert_eq!(c[1], [0.0, 0.0]); // second anchor, same cell
        assert_eq!(c[2], [32.0, 0.0]); // next cell over in x
    }

    #[test]
    fn nms_suppresses_the_overlapping_lower_score_box() {
        let mk = |bbox: [f32; 4], score: f32| Detection { bbox, landmarks: [[0.0; 2]; 5], score };
        let dets = vec![
            mk([0.0, 0.0, 10.0, 10.0], 0.9),
            mk([1.0, 1.0, 11.0, 11.0], 0.8), // heavily overlaps #0
            mk([100.0, 100.0, 110.0, 110.0], 0.7), // disjoint
        ];
        let mut keep = nms(&dets, 0.4);
        keep.sort();
        assert_eq!(keep, vec![0, 2]);
    }

    #[test]
    fn umeyama_recovers_a_known_similarity() {
        // Apply a known similarity (scale 2, 90° rotation, translate) to the
        // reference points to make `src`; the estimate must map src → ref.
        let (s, tx, ty) = (2.0_f32, 5.0_f32, -3.0_f32);
        let mut src = [[0.0_f32; 2]; 5];
        for i in 0..5 {
            let (x, y) = (REF_LANDMARKS[i][0], REF_LANDMARKS[i][1]);
            src[i] = [-s * y + tx, s * x + ty]; // rotate 90°, scale, translate
        }
        let m = umeyama(&src, &REF_LANDMARKS);
        for i in 0..5 {
            let mx = m[0][0] * src[i][0] + m[0][1] * src[i][1] + m[0][2];
            let my = m[1][0] * src[i][0] + m[1][1] * src[i][1] + m[1][2];
            assert!((mx - REF_LANDMARKS[i][0]).abs() < 1e-2, "x {mx} vs {}", REF_LANDMARKS[i][0]);
            assert!((my - REF_LANDMARKS[i][1]).abs() < 1e-2, "y {my} vs {}", REF_LANDMARKS[i][1]);
        }
    }

    #[test]
    fn invert_affine_round_trips() {
        let m = [[2.0_f32, 0.5, 3.0], [-0.5, 2.0, -1.0]];
        let inv = invert_affine(&m);
        // m followed by inv should return a point to itself.
        let (x, y) = (7.0_f32, 11.0);
        let fx = m[0][0] * x + m[0][1] * y + m[0][2];
        let fy = m[1][0] * x + m[1][1] * y + m[1][2];
        let bx = inv[0][0] * fx + inv[0][1] * fy + inv[0][2];
        let by = inv[1][0] * fx + inv[1][1] * fy + inv[1][2];
        assert!((bx - x).abs() < 1e-3 && (by - y).abs() < 1e-3);
    }
}
