use crate::encoder::stateless::BackendPromise;
use crate::encoder::stateless::StatelessBackendResult;
use crate::encoder::stateless::StatelessVideoEncoderBackend;
use crate::encoder::FrameMetadata;

pub(crate) struct Backend;

impl StatelessVideoEncoderBackend<()> for Backend {
    type Picture = ();

    fn import_picture(
        &mut self,
        _metadata: &FrameMetadata,
        _handle: (),
    ) -> StatelessBackendResult<Self::Picture> {
        Ok(())
    }
}
