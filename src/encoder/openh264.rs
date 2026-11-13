//! Native encoder using the OpenH264 source and bindings from the same crate.
use super::{Camera, Yuv420, quality_quantizer};
use anyhow::{Context, Result, ensure};
use openh264_sys2::{self as ffi, source::APILoader};
use std::{ptr::NonNull, slice};

pub(super) struct Encoder {
    handle: NonNull<ffi::ISVCEncoder>,
    functions: ffi::ISVCEncoderVtbl,
    initialized: bool,
}

impl Encoder {
    pub(super) fn new() -> Result<Self> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: the bundled source matches these bindings, and the output
        // pointer is valid. Successful creation transfers ownership here.
        check(
            unsafe { APILoader::WelsCreateSVCEncoder(&mut handle) },
            "create encoder",
        )?;
        let handle = NonNull::new(handle).context("OpenH264 returned a null encoder")?;
        // SAFETY: a successfully created encoder contains a valid C vtable.
        let functions = unsafe { **handle.as_ptr() };
        Ok(Self {
            handle,
            functions,
            initialized: false,
        })
    }

    pub(super) fn initialize(&mut self, camera: &Camera) -> Result<()> {
        let get_params = self
            .functions
            .GetDefaultParams
            .context("OpenH264 GetDefaultParams")?;
        let initialize = self
            .functions
            .InitializeExt
            .context("OpenH264 InitializeExt")?;
        let set_option = self.functions.SetOption.context("OpenH264 SetOption")?;
        // Validate cleanup and per-frame functions before initializing native resources.
        self.functions
            .Uninitialize
            .context("OpenH264 Uninitialize")?;
        self.functions.EncodeFrame.context("OpenH264 EncodeFrame")?;
        self.functions
            .ForceIntraFrame
            .context("OpenH264 ForceIntraFrame")?;
        let mut params = ffi::SEncParamExt::default();
        // SAFETY: params uses this library's ABI and is writable for its full size.
        check(
            unsafe { get_params(self.handle.as_ptr(), &mut params) },
            "get encoder defaults",
        )?;
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u16;
        params.iUsageType = ffi::CAMERA_VIDEO_REAL_TIME;
        params.iPicWidth = camera.width as i32;
        params.iPicHeight = camera.height as i32;
        params.fMaxFrameRate = camera.framerate as f32;
        // Constant QP uses the layer QP directly without bitrate-driven changes.
        params.iRCMode = ffi::RC_OFF_MODE;
        params.iSpatialLayerNum = 1;
        params.iTemporalLayerNum = 1;
        params.uiIntraPeriod = camera.framerate;
        params.eSpsPpsIdStrategy = ffi::CONSTANT_ID;
        params.iMultipleThreadIdc = threads;
        params.bEnableFrameSkip = false;
        params.bEnableAdaptiveQuant = false;
        params.bEnableBackgroundDetection = false;
        params.bEnableSceneChangeDetect = false;
        params.bEnableDenoise = false;
        params.bEnableLongTermReference = false;
        params.iComplexityMode = ffi::LOW_COMPLEXITY;
        let layer = &mut params.sSpatialLayers[0];
        layer.iVideoWidth = params.iPicWidth;
        layer.iVideoHeight = params.iPicHeight;
        layer.fFrameRate = params.fMaxFrameRate;
        layer.uiProfileIdc = ffi::PRO_BASELINE;
        layer.uiLevelIdc = ffi::LEVEL_UNKNOWN;
        layer.iDLayerQp = quality_quantizer(camera.quality);
        layer.sSliceArgument.uiSliceMode = ffi::SM_SIZELIMITED_SLICE;
        // Size-limited slices grow the native bitstream allocation and avoid
        // fixed-slice overflow on detailed frames. Scale the limit so large
        // images stay well below OpenH264's maximum slice/NAL count.
        layer.sSliceArgument.uiSliceSizeConstraint =
            (camera.width * camera.height * 3 / 2 / 32).max(1500);
        layer.sSliceArgument.uiSliceNum = u32::from(threads);
        layer.bVideoSignalTypePresent = true;
        layer.bColorDescriptionPresent = true;
        layer.bFullRange = false;
        layer.uiColorPrimaries = 1;
        layer.uiTransferCharacteristics = 1;
        layer.uiColorMatrix = 1;
        let mut log_level = ffi::WELS_LOG_ERROR;
        // SAFETY: the option expects a writable integer; params and its fields
        // are initialized, validated, and copied by InitializeExt.
        unsafe {
            check(
                set_option(
                    self.handle.as_ptr(),
                    ffi::ENCODER_OPTION_TRACE_LEVEL,
                    std::ptr::from_mut(&mut log_level).cast(),
                ),
                "set encoder log level",
            )?;
            check(
                initialize(self.handle.as_ptr(), &params),
                "initialize encoder",
            )?;
        }
        self.initialized = true;
        Ok(())
    }

    pub(super) fn encode(
        &mut self,
        frame: &Yuv420<'_>,
        timestamp_us: u64,
        force_idr: bool,
        output: &mut Vec<u8>,
    ) -> Result<()> {
        let mut input = ffi::SSourcePicture {
            iColorFormat: ffi::videoFormatI420,
            iPicWidth: frame.width as i32,
            iPicHeight: frame.height as i32,
            uiTimeStamp: (timestamp_us / 1000) as i64,
            ..Default::default()
        };
        for index in 0..3 {
            input.iStride[index] = frame.strides[index] as i32;
            input.pData[index] = frame.planes[index].as_ptr().cast_mut();
        }
        let mut info = ffi::SFrameBSInfo::default();
        // SAFETY: initialize validated these function pointers; the caller
        // validated all dimensions, strides, and plane lengths. Input is borrowed
        // through this synchronous call; OpenH264 only reads its pixel buffers.
        unsafe {
            if force_idr {
                check(
                    self.functions.ForceIntraFrame.unwrap()(self.handle.as_ptr(), true),
                    "force keyframe",
                )?;
            }
            check(
                self.functions.EncodeFrame.unwrap()(self.handle.as_ptr(), &input, &mut info),
                "encode frame",
            )?;
        }
        let count = usize::try_from(info.iLayerNum).context("negative OpenH264 layer count")?;
        let layers = info
            .sLayerInfo
            .get(..count)
            .context("invalid OpenH264 layer count")?;
        output.clear();
        for layer in layers {
            let count = usize::try_from(layer.iNalCount).context("negative OpenH264 NAL count")?;
            if count == 0 {
                continue;
            }
            ensure!(
                count <= ffi::MAX_NAL_UNITS_IN_LAYER as usize
                    && !layer.pNalLengthInByte.is_null()
                    && !layer.pBsBuf.is_null(),
                "invalid OpenH264 NAL storage"
            );
            // SAFETY: a successful EncodeFrame supplies count lengths in this
            // layer. Its native storage remains alive until the next encode.
            let lengths = unsafe { slice::from_raw_parts(layer.pNalLengthInByte, count) };
            let size = lengths.iter().try_fold(0usize, |total, &size| {
                let size = usize::try_from(size).context("negative OpenH264 NAL size")?;
                total
                    .checked_add(size)
                    .context("OpenH264 layer size overflow")
            })?;
            ensure!(size <= isize::MAX as usize, "OpenH264 layer is too large");
            // SAFETY: pBsBuf contains the concatenated NALs described by lengths.
            output.extend_from_slice(unsafe { slice::from_raw_parts(layer.pBsBuf, size) });
        }
        ensure!(
            !output.is_empty() && usize::try_from(info.iFrameSizeInBytes)? == output.len(),
            "OpenH264 returned an empty or incomplete frame"
        );
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the statically linked encoder.
        // Uninitialize only after successful initialization.
        unsafe {
            if self.initialized {
                self.functions.Uninitialize.unwrap()(self.handle.as_ptr());
            }
            APILoader::WelsDestroySVCEncoder(self.handle.as_ptr());
        }
    }
}

fn check(status: i32, operation: &str) -> Result<()> {
    ensure!(status == 0, "OpenH264 {operation} failed (status {status})");
    Ok(())
}
