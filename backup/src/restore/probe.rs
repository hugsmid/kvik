//! The bucket probe a full restore makes before it claims (format A.17 Probe; Q-162, Q-214).

use kvik_sink_api::{BackupSink, SinkError};
use sha2::{Digest, Sha256};

use super::RestoreError;
use crate::envelope::Envelope;
use crate::keys::Keys;
use crate::upload::Refused;

fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

fn random16(random: &mut (dyn FnMut() -> u64 + Send)) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&random().to_le_bytes());
    b[8..].copy_from_slice(&random().to_le_bytes());
    b
}

fn refused(r: Refused) -> Result<(), RestoreError> {
    Err(RestoreError::Refused(r))
}

fn refusal(e: &SinkError) -> bool {
    matches!(e, SinkError::Exists { .. } | SinkError::HashMismatch { .. })
}

/// Refuses a bucket without conditional writes or checksum checks; a sink fault is `RestoreError::Sink`.
pub(super) async fn probe<S: BackupSink, E: Envelope>(
    sink: &S,
    env: &E,
    keys: &Keys,
    random: &mut (dyn FnMut() -> u64 + Send),
) -> Result<(), RestoreError> {
    let key = keys.probe();
    let fresh =
        |random: &mut (dyn FnMut() -> u64 + Send), key: &str| env.seal(key, &random16(random));
    let before = sink.stat(&key).await?;
    let first = fresh(random, &key)?;
    let expected = match (
        before,
        sink.put(&key, first.clone().into(), sha256(&first)).await,
    ) {
        (None, Ok(_)) => {
            let second = fresh(random, &key)?;
            match sink.put(&key, second.clone().into(), sha256(&second)).await {
                Ok(_) => return refused(Refused::NoConditionalWrites),
                Err(e) if refusal(&e) => sha256(&first),
                Err(e) => return Err(e.into()),
            }
        }
        (Some(_), Ok(_)) => return refused(Refused::NoConditionalWrites),
        (Some(earlier), Err(e)) if refusal(&e) => earlier.sha256,
        (_, Err(e)) => return Err(e.into()),
    };
    if sink.stat(&key).await?.map(|s| s.sha256) != Some(expected) {
        return refused(Refused::NoConditionalWrites);
    }
    let key = keys.checksum_probe(random16(random));
    let bytes = fresh(random, &key)?;
    let mut wrong = sha256(&bytes);
    wrong[0] = !wrong[0];
    match sink.put(&key, bytes.into(), wrong).await {
        Ok(_) => return refused(Refused::NoChecksums),
        Err(SinkError::HashMismatch { .. }) => {}
        Err(e) => return Err(e.into()),
    }
    if sink.stat(&key).await?.is_some() {
        return refused(Refused::NoChecksums);
    }
    Ok(())
}
