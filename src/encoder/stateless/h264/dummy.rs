// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::EncoderConfig;
use super::StatelessEncoder;
use crate::backend::dummy::encoder::Backend;
use crate::codec::h264::parser::Sei;
use crate::codec::h264::parser::SeiMessage;
use crate::codec::h264::synthesizer::Synthesizer;
use crate::encoder::stateless::h264::BackendRequest;
use crate::encoder::stateless::h264::StatelessH264EncoderBackend;
use crate::encoder::stateless::EncodeResult;
use crate::encoder::stateless::ReadyPromise;
use crate::encoder::stateless::StatelessBackendError;
use crate::encoder::stateless::StatelessBackendResult;
use crate::BlockingMode;

pub const DUMMY_TS_SEI_UUID: &[u8; 16] = b"cros-codecs-time";

impl StatelessH264EncoderBackend<()> for Backend {
    type Reference = ();
    type CodedPromise = ReadyPromise<Vec<u8>>;
    type ReconPromise = ReadyPromise<()>;

    fn encode_slice(
        &mut self,
        request: BackendRequest<Self::Picture, Self::Reference>,
    ) -> StatelessBackendResult<(Self::ReconPromise, Self::CodedPromise)> {
        let mut coded_output = request.coded_output;

        let mut sei_payload = Vec::<u8>::with_capacity(DUMMY_TS_SEI_UUID.len() + 8usize);
        sei_payload.extend(DUMMY_TS_SEI_UUID);
        sei_payload.extend(request.input_meta.timestamp.to_le_bytes());

        // TODO move somewhere nicer and add tests
        let sei = Sei {
            messages: vec![SeiMessage {
                payload_type: 0x5,
                payload: sei_payload,
            }],
        };

        Synthesizer::<'_, Sei, Vec<u8>>::synthesize(0, &sei, &mut coded_output, false)
            .map_err(|err| StatelessBackendError::Other(anyhow::anyhow!(err)))?;

        let ref_promise = ReadyPromise::from(());
        let coded_promise = ReadyPromise::from(coded_output);

        Ok((ref_promise, coded_promise))
    }
}

impl StatelessEncoder<(), Backend> {
    pub(crate) fn new_dummy(
        config: EncoderConfig,
        blocking_mode: BlockingMode,
    ) -> EncodeResult<Self> {
        Self::new(Backend, config, blocking_mode)
    }
}
