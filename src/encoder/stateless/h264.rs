// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::codec::h264::parser::Level;
use crate::codec::h264::parser::Pps;
use crate::codec::h264::parser::Profile;
use crate::codec::h264::parser::SliceHeader;
use crate::codec::h264::parser::Sps;
use crate::encoder::stateless::h264::predictor::LowDelay;
use crate::encoder::stateless::h264::predictor::PredictionStructure;
use crate::encoder::stateless::h264::predictor::Predictor;
use crate::encoder::stateless::h264::predictor::PredictorVerdict;
use crate::encoder::stateless::BackendPromise;
use crate::encoder::stateless::EncodeResult;
use crate::encoder::stateless::FrameMetadata;
use crate::encoder::stateless::OutputQueue;
use crate::encoder::stateless::StatelessBackendResult;
use crate::encoder::stateless::StatelessVideoEncoder;
use crate::encoder::stateless::StatelessVideoEncoderBackend;
use crate::encoder::CodedBitstreamBuffer;
use crate::BlockingMode;
use crate::Resolution;

mod predictor;

#[cfg(test)]
pub(crate) mod dummy;
#[cfg(feature = "vaapi")]
pub mod vaapi;

#[derive(Clone)]
pub enum Bitrate {
    Constant(u64),
}

impl Bitrate {
    fn target(&self) -> u64 {
        match self {
            Bitrate::Constant(target) => *target,
        }
    }
}

#[derive(Clone)]
pub struct EncoderConfig {
    pub bitrate: Bitrate,
    pub framerate: u32,
    pub resolution: Resolution,
    pub profile: Profile,
    pub level: Level,
    pub pred_structure: PredictionStructure,
    pub default_qp: u8,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        // Artificially encoder configuration with intent to be widely supported.
        Self {
            bitrate: Bitrate::Constant(30_000_000),
            framerate: 30,
            resolution: Resolution {
                width: 320,
                height: 240,
            },
            profile: Profile::Baseline,
            level: Level::L4,
            pred_structure: PredictionStructure::LowDelay {
                tail: 1,
                limit: 2048,
            },
            default_qp: 26,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IsReference {
    No,
    ShortTerm,
    LongTerm,
}

#[derive(Clone, Debug)]
pub(crate) struct DpbEntryMeta {
    /// Picture order count
    poc: u16,
    frame_num: u32,
    is_reference: IsReference,
}

/// Frame structure used in the backend representing currently encoded frame or references used
/// for its encoding.
pub(crate) struct DpbEntry<R> {
    /// Reconstructed picture
    recon_pic: R,
    /// Decoded picture buffer entry metadata
    meta: DpbEntryMeta,
}

/// Stateless H.264 encoder backend input.
pub struct BackendRequest<P, R> {
    sps: Rc<Sps>,
    pps: Rc<Pps>,
    header: SliceHeader,

    /// Input frame to be encoded
    input: P,

    /// Input frame metadata
    input_meta: FrameMetadata,

    /// DPB entry metadata
    dpb_meta: DpbEntryMeta,

    /// Reference lists
    ref_list_0: Vec<Rc<DpbEntry<R>>>,
    ref_list_1: Vec<Rc<DpbEntry<R>>>,

    /// Number of macroblock to be encoded in slice
    num_macroblocks: usize,

    /// True whenever the result is IDR
    is_idr: bool,

    /// Current encoder config. The backend may peek into config to find bitrate and framerate
    /// settings.
    config: Rc<EncoderConfig>,

    /// Container for the request output. [`StatelessH264EncoderBackend`] impl shall move it and
    /// append the slice data to it. This prevents unnecessary copying of bitstream around.
    coded_output: Vec<u8>,
}

/// Wrapper type for [`BackendPromise<Output = Vec<u8>>`], with additional
/// metadata.
struct SlicePromise<P>
where
    P: BackendPromise<Output = Vec<u8>>,
{
    /// Slice data and reconstructed surface promise
    bitstream: P,

    /// Input frame metadata, for [`CodedBitstreamBuffer`]
    meta: FrameMetadata,
}

impl<P> BackendPromise for SlicePromise<P>
where
    P: BackendPromise<Output = Vec<u8>>,
{
    type Output = CodedBitstreamBuffer;

    fn is_ready(&self) -> bool {
        self.bitstream.is_ready()
    }

    fn sync(self) -> StatelessBackendResult<Self::Output> {
        let coded_data = self.bitstream.sync()?;

        log::trace!("synced bitstream size={}", coded_data.len());

        Ok(CodedBitstreamBuffer::new(self.meta, coded_data))
    }
}

/// Wrapper type for [`BackendPromise<Output = R>`], with additional
/// metadata.
struct ReferencePromise<P>
where
    P: BackendPromise,
{
    /// Slice data and reconstructed surface promise
    recon: P,

    /// [`DpbEntryMeta`] of reconstructed surface
    dpb_meta: DpbEntryMeta,
}

impl<P> BackendPromise for ReferencePromise<P>
where
    P: BackendPromise,
{
    type Output = DpbEntry<P::Output>;

    fn is_ready(&self) -> bool {
        self.recon.is_ready()
    }

    fn sync(self) -> StatelessBackendResult<Self::Output> {
        let recon_pic = self.recon.sync()?;

        log::trace!("synced recon picture frame_num={}", self.dpb_meta.frame_num);

        Ok(DpbEntry {
            recon_pic,
            meta: self.dpb_meta,
        })
    }
}

/// Trait for stateless encoder backend for H.264
pub trait StatelessH264EncoderBackend<H>: StatelessVideoEncoderBackend<H> {
    type Reference;
    type CodedPromise: BackendPromise<Output = Vec<u8>>;
    type ReconPromise: BackendPromise<Output = Self::Reference>;

    /// Submit a [`BackendRequest`] to the backend. This operation returns both a
    /// [`Self::CodedPromise`] and a [`Self::ReconPromise`] with resulting slice data.
    fn encode_slice(
        &mut self,
        request: BackendRequest<Self::Picture, Self::Reference>,
    ) -> StatelessBackendResult<(Self::ReconPromise, Self::CodedPromise)>;
}

pub struct StatelessEncoder<H, B>
where
    B: StatelessH264EncoderBackend<H>,
    B::Picture: 'static,
    B::Reference: 'static,
{
    /// Pending slice output promise queue
    output_queue: OutputQueue<SlicePromise<B::CodedPromise>>,

    /// Pending reconstructed pictures promise queue
    recon_queue: OutputQueue<ReferencePromise<B::ReconPromise>>,

    /// [`Predictor`] instance responsible for the encoder decision making
    predictor: Box<dyn Predictor<B::Picture, B::Reference>>,

    /// Pending [`CodedBitstreamBuffer`]s to be polled by the user
    coded_queue: VecDeque<CodedBitstreamBuffer>,

    /// Number of the currently held frames by the predictor
    predictor_frame_count: usize,

    /// [`StatelessH264EncoderBackend`] instance to delegate [`BackendRequest`] to
    backend: B,
}

impl<H, B> StatelessEncoder<H, B>
where
    B: StatelessH264EncoderBackend<H>,
    B::Picture: 'static,
    B::Reference: 'static,
{
    fn new(backend: B, config: EncoderConfig, mode: BlockingMode) -> EncodeResult<Self> {
        let predictor: Box<dyn Predictor<_, _>> = match config.pred_structure {
            PredictionStructure::LowDelay { .. } => Box::new(LowDelay::new(config)),
        };

        Ok(Self {
            backend,
            predictor,
            predictor_frame_count: 0,
            coded_queue: Default::default(),
            output_queue: OutputQueue::new(mode),
            recon_queue: OutputQueue::new(mode),
        })
    }

    fn request(&mut self, request: BackendRequest<B::Picture, B::Reference>) -> EncodeResult<()> {
        let meta = request.input_meta.clone();
        let dpb_meta = request.dpb_meta.clone();

        log::trace!("submitting new request");
        let (recon, bitstream) = self.backend.encode_slice(request)?;

        // Wrap promise from backend with headers and metadata
        let slice_promise = SlicePromise { bitstream, meta };

        self.output_queue.add_promise(slice_promise);

        let ref_promise = ReferencePromise { recon, dpb_meta };

        self.recon_queue.add_promise(ref_promise);

        Ok(())
    }

    fn execute(
        &mut self,
        verdict: PredictorVerdict<B::Picture, B::Reference>,
    ) -> EncodeResult<bool> {
        let requests = match verdict {
            PredictorVerdict::NoOperation => return Ok(false),
            PredictorVerdict::Request { requests } => requests,
        };

        for request in requests {
            self.request(request)?;
            self.predictor_frame_count -= 1;
        }
        Ok(true)
    }

    fn poll_pending(&mut self, mode: BlockingMode) -> EncodeResult<()> {
        // Poll the output queue once and then continue polling while new promise is submitted
        while let Some(coded) = self.output_queue.poll(mode)? {
            self.coded_queue.push_back(coded);
        }

        while let Some(recon) = self.recon_queue.poll(mode)? {
            let verdict = self.predictor.reconstructed(recon)?;
            if !self.execute(verdict)? {
                // No promise was submitted, therefore break
                break;
            }
        }

        Ok(())
    }
}

impl<H, B> StatelessVideoEncoder<H> for StatelessEncoder<H, B>
where
    B: StatelessH264EncoderBackend<H>,
{
    fn encode(&mut self, metadata: FrameMetadata, handle: H) -> EncodeResult<()> {
        log::trace!(
            "encode: timestamp={} layout={:?}",
            metadata.timestamp,
            metadata.layout
        );

        // Import `handle` to backends representation
        let backend_pic = self.backend.import_picture(&metadata, handle)?;

        // Increase the number of frames that predictor holds, before handing one to it
        self.predictor_frame_count += 1;

        // Ask predictor to decide on the next move and execute it
        let verdict = self.predictor.new_frame(backend_pic, metadata)?;
        self.execute(verdict)?;

        Ok(())
    }

    fn drain(&mut self) -> EncodeResult<()> {
        log::trace!("currently predictor holds {}", self.predictor_frame_count);

        // Drain the predictor
        while self.predictor_frame_count > 0 || !self.recon_queue.is_empty() {
            if self.output_queue.is_empty() && self.recon_queue.is_empty() {
                // The OutputQueue is empty and predictor holds frames, force it to yield a request
                // to empty it's internal queue.
                let requests = self.predictor.drain()?;
                self.predictor_frame_count -= requests.len();

                for request in requests {
                    self.request(request)?;
                }
            }

            self.poll_pending(BlockingMode::Blocking)?;
        }

        // There are still some requests being processed. Continue on polling them.
        while !self.output_queue.is_empty() {
            self.poll_pending(BlockingMode::Blocking)?;
        }

        Ok(())
    }

    fn poll(&mut self) -> EncodeResult<Option<CodedBitstreamBuffer>> {
        // Poll on output queue without blocking and try to dueue from coded queue
        self.poll_pending(BlockingMode::NonBlocking)?;
        Ok(self.coded_queue.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::codec::h264::nalu::Nalu;
    use crate::codec::h264::parser::NaluHeader;
    use crate::codec::h264::parser::NaluType;
    use crate::codec::h264::parser::Parser;
    use crate::encoder::stateless::h264::dummy::DUMMY_TS_SEI_UUID;
    use crate::encoder::stateless::simple_encode_loop;
    use crate::encoder::stateless::tests::DummyFrameProducer;
    use crate::FrameLayout;
    use crate::PlaneLayout;

    #[test]
    fn test_low_delay_dummy() {
        const WIDTH: usize = 1;
        const HEIGHT: usize = 1;
        const FRAME_COUNT: u64 = 10000;

        let _ = env_logger::try_init();

        let config = EncoderConfig {
            profile: Profile::Main,
            framerate: 30,
            resolution: Resolution {
                width: WIDTH as u32,
                height: HEIGHT as u32,
            },
            ..Default::default()
        };

        let frame_layout = FrameLayout {
            format: (b"NV12".into(), 0),
            size: Resolution {
                width: WIDTH as u32,
                height: HEIGHT as u32,
            },
            planes: vec![
                PlaneLayout {
                    buffer_index: 0,
                    offset: 0,
                    stride: WIDTH,
                },
                PlaneLayout {
                    buffer_index: 0,
                    offset: WIDTH * HEIGHT,
                    stride: WIDTH,
                },
            ],
        };

        let mut encoder = StatelessEncoder::new_dummy(config, BlockingMode::Blocking).unwrap();

        let mut producer = DummyFrameProducer::new(FRAME_COUNT, frame_layout);
        let bitstream = simple_encode_loop(&mut encoder, &mut producer).unwrap();
        let mut cursor = Cursor::new(&bitstream[..]);

        let mut frame_counter: u64 = 0;

        while let Ok(nalu) = Nalu::<NaluHeader>::next(&mut cursor) {
            match nalu.header.type_ {
                NaluType::Sei => {
                    let sei = Parser::parse_sei(&nalu).unwrap();

                    for message in sei.messages {
                        if message.payload_type != 0x05 {
                            continue;
                        }

                        let uuid = &message.payload[..16];
                        assert_eq!(DUMMY_TS_SEI_UUID, uuid);

                        let expected_payload = frame_counter.to_le_bytes();
                        assert_eq!(&expected_payload, &message.payload[16..]);

                        frame_counter += 1;
                    }
                }
                type_ => {
                    assert!(matches!(type_, NaluType::Sps | NaluType::Pps))
                }
            }
        }

        assert_eq!(frame_counter, FRAME_COUNT);

        const WRITE_TO_FILE: bool = true;
        if WRITE_TO_FILE {
            use std::io::Write;
            let mut out = std::fs::File::create("test_low_delay_dummy.264").unwrap();
            out.write_all(&bitstream).unwrap();
            out.flush().unwrap();
        }
    }
}
