// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::VecDeque;
use std::rc::Rc;

use log::trace;

use crate::codec::h264::parser::Level;
use crate::codec::h264::parser::Pps;
use crate::codec::h264::parser::PpsBuilder;
use crate::codec::h264::parser::Profile;
use crate::codec::h264::parser::SliceHeaderBuilder;
use crate::codec::h264::parser::SliceType;
use crate::codec::h264::parser::Sps;
use crate::codec::h264::parser::SpsBuilder;
use crate::codec::h264::synthesizer::Synthesizer;
use crate::encoder::stateless::h264::BackendRequest;
use crate::encoder::stateless::h264::DpbEntry;
use crate::encoder::stateless::h264::DpbEntryMeta;
use crate::encoder::stateless::h264::EncoderConfig;
use crate::encoder::stateless::h264::IsReference;
use crate::encoder::stateless::EncodeError;
use crate::encoder::stateless::EncodeResult;
use crate::encoder::stateless::FrameMetadata;

/// Available predictors and initialization parameters
#[derive(Clone)]
pub enum PredictionStructure {
    /// Simplest prediction structure, suitable eg. for RTC. IDR is produced at the start of
    /// the stream and every time when [`limit`] frames are reached. IDR is built with SPS, PPS
    /// and frame with single I slice. Following IDR frames are single P slice frames referencing
    /// maximum [`tail`] previous frames.
    LowDelay {
        tail: u16,
        limit: u16,
    },

    GroupOfPictures {
        size: u16,
        limit: u16,
    },
}

/// The result of the predictor operations.
#[allow(clippy::large_enum_variant)]
pub(super) enum PredictorVerdict<P, R> {
    /// The backend/encoder shall do nothing.
    NoOperation,
    /// The [`BackendRequest`] shall be submitted to the backend
    Request { requests: Vec<BackendRequest<P, R>> },
}

/// Predictor is responsible for yielding stream parameter sets and creating requests to backend.
/// It accepts the frames and reconstructed frames and returns [`PredictorVerdict`] what operation
/// encoder shall perfom. For example [`Predictor`] may hold frames from processing until enough
/// is supplied to create a specific prediction structure. [`Predictor::drain`] may be called to
/// force predictor to yield requests.
pub(super) trait Predictor<P, R> {
    /// Called by encoder when there is new frame to encode. The predictor may return [`NoOperation`]
    /// to postpone processing or [`Request`] to process a frame (it does not have to be a frame
    /// specified in parameters)
    ///
    /// [`NoOperation`]: PredictorVerdict::NoOperation
    /// [`Request`]: PredictorVerdict::Request
    fn new_frame(
        &mut self,
        backend_pic: P,
        meta: FrameMetadata,
    ) -> EncodeResult<PredictorVerdict<P, R>>;

    /// This function is called by the encoder, with reconstructed frame when backend finished
    /// processing the frame. the [`Predictor`] may choose to return a [`Request`] to submit new
    /// request to backend, if reconstructed was required for creating that request.
    ///
    /// [`Request`]: PredictorVerdict::Request
    fn reconstructed(&mut self, recon: DpbEntry<R>) -> EncodeResult<PredictorVerdict<P, R>>;

    /// Force [`Predictor`] to pop frame from internal queue and return a [`BackendRequest`]
    fn drain(&mut self) -> EncodeResult<Vec<BackendRequest<P, R>>>;
}

/// Implementation of [`LowDelay`] prediction structure. See [`LowDelay`] for details.
///
/// [`LowDelay`]: PredictionStructure::LowDelay
pub(super) struct LowDelay<P, R> {
    /// Current frame in the sequence counter
    counter: u16,
    /// Limit of frames in the sequence
    limit: u16,
    /// Target number of reference frames that an interframe should have
    tail: u16,

    /// Queue of pending frames to be encoded
    queue: VecDeque<(P, FrameMetadata)>,

    /// The currently held frames in POC increasing order.
    dpb: VecDeque<Rc<DpbEntry<R>>>,

    /// Current sequence SPS
    sps: Option<Rc<Sps>>,
    /// Current sequence PPS
    pps: Option<Rc<Pps>>,

    /// Encoder config
    config: Rc<EncoderConfig>,
}

impl<P, R> LowDelay<P, R> {
    pub(super) fn new(config: EncoderConfig) -> Self {
        let config = Rc::new(config);
        let (tail, limit) = match config.pred_structure {
            PredictionStructure::LowDelay { tail, limit } => (tail, limit),
            _ => panic!(),
        };

        Self {
            counter: 0,
            limit,
            tail,
            queue: Default::default(),
            dpb: Default::default(),
            sps: None,
            pps: None,
            config,
        }
    }
}

impl<P, R> LowDelay<P, R> {
    fn new_sequence(&mut self) {
        trace!("beginning new sequence");
        let mut sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(self.config.profile);

        // H.264 Table 6-1
        sps = match self.config.profile {
            // 4:2:2 subsampling
            Profile::High422P => sps.chroma_format_idc(2),
            // 4:2:0 subsampling
            _ => sps.chroma_format_idc(1),
        };

        let sps = sps
            .level_idc(self.config.level)
            .max_frame_num(self.limit as u32)
            .pic_order_cnt_type(0)
            .max_pic_order_cnt_lsb(self.limit as u32 * 2)
            .max_num_ref_frames(self.tail as u32 + 1)
            .frame_mbs_only_flag(true)
            // H264 spec Table A-4
            .direct_8x8_inference_flag(self.config.level >= Level::L3)
            .resolution(self.config.resolution.width, self.config.resolution.height)
            .bit_depth_luma(8)
            .bit_depth_chroma(8)
            .aspect_ratio(1, 1)
            .timing_info(1, self.config.framerate * 2, false)
            .build();

        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(self.config.default_qp)
            .deblocking_filter_control_present_flag(true)
            .num_ref_idx_l0_default_active(self.tail as u8)
            // Unused, P frame relies only on list0
            .num_ref_idx_l1_default_active_minus1(0)
            .build();

        self.dpb.clear();
        self.sps = Some(sps);
        self.pps = Some(pps);
    }

    fn request_idr(
        &mut self,
        input: P,
        input_meta: FrameMetadata,
    ) -> EncodeResult<PredictorVerdict<P, R>> {
        // Begin new sequence and start with I frame and no references.
        self.counter = 0;
        self.new_sequence();

        // SAFETY: SPS and PPS were initialized by [`Self::new_sequence()`]
        let sps = self.sps.clone().unwrap();
        let pps = self.pps.clone().unwrap();

        let dpb_meta = DpbEntryMeta {
            poc: self.counter * 2,
            frame_num: self.counter as u32,
            is_reference: IsReference::ShortTerm,
        };

        let header = SliceHeaderBuilder::new(&pps)
            .slice_type(SliceType::I)
            .first_mb_in_slice(0)
            .pic_order_cnt_lsb(dpb_meta.poc)
            .build();

        self.counter += 1;

        let mut headers = vec![];
        Synthesizer::<Sps, Vec<u8>>::synthesize(3, &sps, &mut headers, true)?;
        Synthesizer::<Pps, Vec<u8>>::synthesize(3, &pps, &mut headers, true)?;

        let num_macroblocks =
            ((sps.pic_width_in_mbs_minus1 + 1) * (sps.pic_height_in_map_units_minus1 + 1)) as usize;

        Ok(PredictorVerdict::Request {
            requests: vec![BackendRequest {
                sps,
                pps,
                header,
                input,
                input_meta,
                dpb_meta,
                // This frame is IDR, therefore it has no references
                ref_list_0: vec![],
                ref_list_1: vec![],

                num_macroblocks,

                is_idr: true,
                config: Rc::clone(&self.config),

                coded_output: headers,
            }],
        })
    }

    fn request_interframe(
        &mut self,
        input: P,
        input_meta: FrameMetadata,
    ) -> PredictorVerdict<P, R> {
        let mut ref_list_0 = vec![];

        // Use all avaiable reference frames in DPB. Their number is limited by the parameter
        for reference in self.dpb.iter().rev() {
            ref_list_0.push(Rc::clone(reference));
        }

        // SAFETY: SPS and PPS were initialized during IDR request
        let sps = self.sps.clone().unwrap();
        let pps = self.pps.clone().unwrap();

        let dpb_meta = DpbEntryMeta {
            poc: self.counter * 2,
            frame_num: self.counter as u32,
            is_reference: IsReference::ShortTerm,
        };

        let header = SliceHeaderBuilder::new(&pps)
            .slice_type(SliceType::P)
            .first_mb_in_slice(0)
            .pic_order_cnt_lsb(dpb_meta.poc)
            .build();

        let num_macroblocks =
            ((sps.pic_width_in_mbs_minus1 + 1) * (sps.pic_height_in_map_units_minus1 + 1)) as usize;

        let request = BackendRequest {
            sps,
            pps,
            header,
            input,
            input_meta,
            dpb_meta,
            ref_list_0,
            ref_list_1: vec![], // No future references

            num_macroblocks,

            is_idr: false,
            config: Rc::clone(&self.config),

            coded_output: vec![],
        };

        self.counter += 1;

        // Remove obselete reference frames
        while self.dpb.len() > self.tail as usize - 1 {
            self.dpb.pop_front();
        }

        PredictorVerdict::Request {
            requests: vec![request],
        }
    }

    fn next_request(&mut self) -> EncodeResult<PredictorVerdict<P, R>> {
        self.counter %= self.limit;

        match self.queue.pop_front() {
            // Nothing to do. Quit.
            None => Ok(PredictorVerdict::NoOperation),

            // If first frame in the sequence or forced IDR then create IDR request.
            Some((input, meta)) if self.counter == 0 || meta.force_keyframe => {
                Ok(self.request_idr(input, meta)?)
            }

            // There is no enough frames in the DPB
            Some((input, meta))
                if self.dpb.is_empty()
                    || self.dpb.len() < (self.counter.min(self.tail) as usize) =>
            {
                self.queue.push_front((input, meta));
                Ok(PredictorVerdict::NoOperation)
            }

            Some((input, meta)) => {
                // Make sure that reference frames in DPB is consistent
                assert!(self.dpb.back().unwrap().meta.frame_num == self.counter as u32 - 1);
                Ok(self.request_interframe(input, meta))
            }
        }
    }
}

impl<P, R> Predictor<P, R> for LowDelay<P, R> {
    fn new_frame(
        &mut self,
        input: P,
        frame_metadata: FrameMetadata,
    ) -> EncodeResult<PredictorVerdict<P, R>> {
        // Add new frame in the request queue and request new encoding if possible
        self.queue.push_back((input, frame_metadata));
        self.next_request()
    }

    fn reconstructed(&mut self, recon: DpbEntry<R>) -> EncodeResult<PredictorVerdict<P, R>> {
        // Add new reconstructed surface and request next encoding if possible
        self.dpb.push_back(Rc::new(recon));
        self.next_request()
    }

    fn drain(&mut self) -> EncodeResult<Vec<BackendRequest<P, R>>> {
        // [`LowDelay`] will not hold any frames, therefore the drain function shall never be called.
        Err(EncodeError::InvalidInternalState)
    }
}

pub(super) struct GroupOfPictures<P, R> {
    /// Current frame in the sequence counter
    poc_counter: u16,
    // frame_num counter
    frame_counter: u32,

    /// Limit of frames in the sequence
    limit: u16,
    /// The number of B frames in GOP
    size: u16,

    /// Queue of pending frames to be encoded
    pending: VecDeque<(P, FrameMetadata)>,

    /// Buffer of future B frames. The contents of this buffers will be drained
    /// after both l0 and l1 references are reconstructed.
    future_b_frames: VecDeque<(P, FrameMetadata)>,

    // The left frame of the GOP, that will be l0 reference for b frames
    l0_ref: Option<Rc<DpbEntry<R>>>,
    // Metadata of the future l0 reference frame
    idr_ref_pending: Option<DpbEntryMeta>,
    // Metadata of the future l1 reference frame
    l1_ref_pending: Option<DpbEntryMeta>,

    /// Current sequence SPS
    sps: Option<Rc<Sps>>,
    /// Current sequence PPS
    pps: Option<Rc<Pps>>,

    /// Encoder config
    config: Rc<EncoderConfig>,
}

impl<P, R> GroupOfPictures<P, R> {
    pub(super) fn new(config: EncoderConfig) -> Self {
        let config = Rc::new(config);
        let (size, limit) = match config.pred_structure {
            PredictionStructure::GroupOfPictures { size, limit } => (size, limit),
            _ => panic!(),
        };

        Self {
            poc_counter: 0,
            frame_counter: 0,
            limit,
            size,

            pending: Default::default(),
            future_b_frames: Default::default(),

            l0_ref: None,
            idr_ref_pending: None,
            l1_ref_pending: None,

            sps: None,
            pps: None,
            config,
        }
    }
}

impl<P, R> GroupOfPictures<P, R> {
    fn new_sequence(&mut self) {
        trace!("beginning new sequence");
        let mut sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(self.config.profile);

        // H.264 Table 6-1
        sps = match self.config.profile {
            // 4:2:2 subsampling
            Profile::High422P => sps.chroma_format_idc(2),
            // 4:2:0 subsampling
            _ => sps.chroma_format_idc(1),
        };

        let sps = sps
            .level_idc(self.config.level)
            .max_frame_num(self.limit as u32)
            .pic_order_cnt_type(0)
            .max_pic_order_cnt_lsb(self.limit as u32 * 2)
            .max_num_ref_frames(self.size as u32 + 1)
            .frame_mbs_only_flag(true)
            // H264 spec Table A-4
            .direct_8x8_inference_flag(self.config.level >= Level::L3)
            .resolution(self.config.resolution.width, self.config.resolution.height)
            .bit_depth_luma(8)
            .bit_depth_chroma(8)
            .aspect_ratio(1, 1)
            .timing_info(1, self.config.framerate * 2, false)
            .build();

        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(self.config.default_qp)
            .deblocking_filter_control_present_flag(true)
            .num_ref_idx_l0_default_active(1)
            .num_ref_idx_l1_default_active(1)
            .build();

        self.frame_counter = 0;
        self.poc_counter = 0;
        self.l0_ref = None;
        self.idr_ref_pending = None;
        self.l1_ref_pending = None;
        self.sps = Some(sps);
        self.pps = Some(pps);
    }

    fn request_idr(
        &mut self,
        input: P,
        input_meta: FrameMetadata,
    ) -> EncodeResult<BackendRequest<P, R>> {
        // Begin new sequence and start with I frame and no references.
        self.new_sequence();

        // SAFETY: SPS and PPS were initialized by [`Self::new_sequence()`]
        let sps = self.sps.clone().unwrap();
        let pps = self.pps.clone().unwrap();

        let dpb_meta = DpbEntryMeta {
            poc: self.poc_counter * 2,
            frame_num: self.frame_counter,
            is_reference: IsReference::ShortTerm,
        };

        let header = SliceHeaderBuilder::new(&pps)
            .slice_type(SliceType::I)
            .first_mb_in_slice(0)
            .pic_order_cnt_lsb(dpb_meta.poc)
            .build();

        self.poc_counter += 1;
        self.frame_counter += 1;

        let mut headers = vec![];
        Synthesizer::<Sps, Vec<u8>>::synthesize(3, &sps, &mut headers, true)?;
        Synthesizer::<Pps, Vec<u8>>::synthesize(3, &pps, &mut headers, true)?;

        let num_macroblocks =
            ((sps.pic_width_in_mbs_minus1 + 1) * (sps.pic_height_in_map_units_minus1 + 1)) as usize;

        self.idr_ref_pending = Some(dpb_meta.clone());

        Ok(BackendRequest {
            sps,
            pps,
            header,
            input,
            input_meta,
            dpb_meta,
            // This frame is IDR, therefore it has no references
            ref_list_0: vec![],
            ref_list_1: vec![],

            num_macroblocks,

            is_idr: true,
            config: Rc::clone(&self.config),

            coded_output: headers,
        })
    }

    fn request_p(&mut self, input: P, input_meta: FrameMetadata) -> BackendRequest<P, R> {
        // SAFETY: SPS and PPS were initialized during IDR request
        let sps = self.sps.clone().unwrap();
        let pps = self.pps.clone().unwrap();

        let dpb_meta = DpbEntryMeta {
            poc: (self.poc_counter + self.size) * 2,
            frame_num: self.frame_counter,
            is_reference: IsReference::ShortTerm,
        };

        let header = SliceHeaderBuilder::new(&pps)
            .slice_type(SliceType::P)
            .first_mb_in_slice(0)
            .pic_order_cnt_lsb(dpb_meta.poc)
            .build();

        let num_macroblocks =
            ((sps.pic_width_in_mbs_minus1 + 1) * (sps.pic_height_in_map_units_minus1 + 1)) as usize;

        self.l1_ref_pending = Some(dpb_meta.clone());

        let request = BackendRequest {
            sps,
            pps,
            header,
            input,
            input_meta,
            dpb_meta,
            ref_list_0: vec![Rc::clone(self.l0_ref.as_ref().unwrap())],
            ref_list_1: vec![], // No future references

            num_macroblocks,

            is_idr: false,
            config: Rc::clone(&self.config),

            coded_output: vec![],
        };

        self.poc_counter += 1;
        self.frame_counter += 1;

        request
    }

    fn request_b(
        &mut self,
        input: P,
        input_meta: FrameMetadata,
        l1_ref: &Rc<DpbEntry<R>>,
    ) -> BackendRequest<P, R> {
        // SAFETY: SPS and PPS were initialized during IDR request
        let sps = self.sps.clone().unwrap();
        let pps = self.pps.clone().unwrap();

        let dpb_meta = DpbEntryMeta {
            poc: (self.poc_counter - 1) * 2,
            frame_num: self.frame_counter,
            is_reference: IsReference::No,
        };

        let header = SliceHeaderBuilder::new(&pps)
            .slice_type(SliceType::B)
            .first_mb_in_slice(0)
            .pic_order_cnt_lsb(dpb_meta.poc)
            .build();

        let num_macroblocks =
            ((sps.pic_width_in_mbs_minus1 + 1) * (sps.pic_height_in_map_units_minus1 + 1)) as usize;

        let request = BackendRequest {
            sps,
            pps,
            header,
            input,
            input_meta,
            dpb_meta,
            ref_list_0: vec![Rc::clone(self.l0_ref.as_ref().unwrap())],
            ref_list_1: vec![Rc::clone(l1_ref)],

            num_macroblocks,

            is_idr: false,
            config: Rc::clone(&self.config),

            coded_output: vec![],
        };

        self.poc_counter += 1;

        request
    }

    fn next_i_p_frames(&mut self, requests: &mut Vec<BackendRequest<P, R>>) -> EncodeResult<()> {
        while let Some((input, frame_metadata)) = self.pending.pop_front() {
            if self.l0_ref.is_none() && self.idr_ref_pending.is_none() {
                requests.push(self.request_idr(input, frame_metadata)?);
            } else if self.future_b_frames.len() < self.size as usize {
                self.future_b_frames.push_back((input, frame_metadata));
            } else if self.l1_ref_pending.is_none() && self.l0_ref.is_some() {
                requests.push(self.request_p(input, frame_metadata));
            } else {
                self.pending.push_front((input, frame_metadata));
                break;
            }
        }

        Ok(())
    }
}

impl<P, R> Predictor<P, R> for GroupOfPictures<P, R> {
    fn new_frame(
        &mut self,
        input: P,
        frame_metadata: FrameMetadata,
    ) -> EncodeResult<PredictorVerdict<P, R>> {
        self.pending.push_back((input, frame_metadata));

        let mut requests = vec![];
        self.next_i_p_frames(&mut requests)?;

        if requests.is_empty() {
            return Ok(PredictorVerdict::NoOperation);
        }

        Ok(PredictorVerdict::Request { requests })
    }

    fn reconstructed(&mut self, recon: DpbEntry<R>) -> EncodeResult<PredictorVerdict<P, R>> {
        let mut requests = vec![];

        if self.idr_ref_pending.as_ref() == Some(&recon.meta) {
            // It is the first reconstructed picture in the sequence (I),
            // therefore using as l0 reference.
            self.l0_ref = Some(Rc::new(recon));
        } else if self.l1_ref_pending.as_ref() == Some(&recon.meta) {
            let recon = Rc::new(recon);

            while let Some((input, meta)) = self.future_b_frames.pop_front() {
                requests.push(self.request_b(input, meta, &recon));
            }

            self.l0_ref = Some(recon);
            self.l1_ref_pending = None;
        }

        self.next_i_p_frames(&mut requests)?;

        if requests.is_empty() {
            return Ok(PredictorVerdict::NoOperation);
        }

        Ok(PredictorVerdict::Request { requests })
    }

    fn drain(&mut self) -> EncodeResult<Vec<BackendRequest<P, R>>> {
        if self.l1_ref_pending.is_some() {
            return Err(EncodeError::InvalidInternalState);
        }

        let Some((input, meta)) = self.future_b_frames.pop_back() else {
            return Err(EncodeError::InvalidInternalState);
        };

        let req = self.request_p(input, meta);
        self.l1_ref_pending = Some(req.dpb_meta.clone());

        Ok(vec![req])
    }
}
