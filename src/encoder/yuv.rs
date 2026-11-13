//! Borrowed planar 8-bit YUV420; libcamera mappings already account for FD offsets.
use anyhow::{Context, Result, bail, ensure};

pub struct Yuv420<'a> {
    pub planes: [&'a [u8]; 3],
    pub strides: [usize; 3],
    pub width: usize,
    pub height: usize,
}

impl<'a> Yuv420<'a> {
    pub fn new(planes: &[&'a [u8]], width: usize, height: usize, stride: usize) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2),
            "YUV420 dimensions must be positive and even"
        );
        ensure!(
            stride >= width && stride.is_multiple_of(2),
            "invalid YUV420 stride"
        );
        let y_size = stride.checked_mul(height).context("YUV420 size overflow")?;
        let c_size = y_size / 4;
        let take = |plane: &'a [u8], size| plane.get(..size).context("YUV420 plane is too short");
        let data = match planes {
            [all] => {
                let size = y_size
                    .checked_add(c_size.checked_mul(2).context("YUV420 size overflow")?)
                    .context("YUV420 size overflow")?;
                let all = take(all, size)?;
                [
                    &all[..y_size],
                    &all[y_size..y_size + c_size],
                    &all[y_size + c_size..],
                ]
            }
            [y, uv] => {
                let uv = take(uv, 2 * c_size)?;
                [take(y, y_size)?, &uv[..c_size], &uv[c_size..]]
            }
            [y, u, v] => [take(y, y_size)?, take(u, c_size)?, take(v, c_size)?],
            _ => bail!("YUV420 requires one, two or three planes"),
        };
        Ok(Self {
            planes: data,
            strides: [stride, stride / 2, stride / 2],
            width,
            height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_planes_and_offsets_are_borrowed_without_copying() {
        let (w, h, s) = (64, 66, 80);
        let y = vec![40; s * h];
        let u = vec![90; s * h / 4];
        let v = vec![170; s * h / 4];
        let all = [y.as_slice(), u.as_slice(), v.as_slice()].concat();
        for parts in [
            vec![all.as_slice()],
            vec![y.as_slice(), &all[y.len()..]],
            vec![y.as_slice(), u.as_slice(), v.as_slice()],
        ] {
            let frame = Yuv420::new(&parts, w, h, s).unwrap();
            assert_eq!(
                (frame.width, frame.height, frame.strides),
                (w, h, [s, s / 2, s / 2])
            );
            assert_eq!(frame.planes, [y.as_slice(), u.as_slice(), v.as_slice()]);
            assert_eq!(frame.planes[0].as_ptr(), parts[0].as_ptr());
        }
        let mut offset = vec![0; 17];
        offset.extend_from_slice(&all);
        let frame = Yuv420::new(&[&offset[17..]], w, h, s).unwrap();
        assert_eq!(frame.planes[0], y);
        assert!(Yuv420::new(&[], w, h, s).is_err());
        assert!(Yuv420::new(&[&all[..all.len() - 1]], w, h, s).is_err());
        assert!(Yuv420::new(&[&y, &u, &v[..v.len() - 1]], w, h, s).is_err());
        assert!(Yuv420::new(&[&all], w, h, s - 1).is_err());
        assert!(Yuv420::new(&[&all], w + 1, h, s).is_err());
        assert!(Yuv420::new(&[&all], w, h, usize::MAX - 1).is_err());
    }
}
