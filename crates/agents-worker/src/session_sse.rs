use std::{future::Future, time::Duration};

use futures::{Stream, StreamExt, future::Either};
use worker::{Delay, Result};

const KEEPALIVE: &[u8] = b": keepalive\n\n";
const DONE: &[u8] = b"data: [DONE]\n\n";

pub(super) fn keep_alive<S>(source: S) -> impl Stream<Item = Result<Vec<u8>>>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin,
{
    with_timer(source, || Delay::from(Duration::from_secs(15)))
}

fn with_timer<S, F, T>(source: S, timer: F) -> impl Stream<Item = Result<Vec<u8>>>
where
    S: Stream<Item = Result<Vec<u8>>> + Unpin,
    F: FnMut() -> T,
    T: Future<Output = ()>,
{
    futures::stream::unfold(
        (source, timer, false),
        |(mut source, mut timer, ended)| async move {
            if ended {
                return None;
            }
            // Poll upstream first: EOF or queued data wins over a ready heartbeat.
            let item = match futures::future::select(source.next(), Box::pin(timer())).await {
                Either::Left((item, _)) => item?,
                Either::Right(_) => Ok(KEEPALIVE.to_vec()),
            };
            let ended = item.as_ref().map_or(true, |bytes| bytes == DONE);
            Some((item, (source, timer, ended)))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{channel::mpsc, executor::block_on, stream};

    #[test]
    fn quiet_channel_emits_comment_then_done_closes_even_with_a_live_sender() {
        block_on(async {
            let (tx, rx) = mpsc::unbounded();
            let output = with_timer(rx, || futures::future::ready(()));
            futures::pin_mut!(output);
            assert_eq!(output.next().await.unwrap().unwrap(), KEEPALIVE);
            tx.unbounded_send(Ok(DONE.to_vec())).unwrap();
            assert_eq!(output.next().await.unwrap().unwrap(), DONE);
            assert!(output.next().await.is_none());
            drop(tx);
        });
    }

    #[test]
    fn queued_data_and_eof_win_over_heartbeat_without_changing_data() {
        let chunks = vec![
            b"data: {\"value\":\"[DONE]\"}\n\n".to_vec(),
            b"data: second\n\n".to_vec(),
        ];
        let output = with_timer(stream::iter(chunks.clone().into_iter().map(Ok)), || {
            futures::future::ready(())
        });
        let actual = block_on(output.collect::<Vec<_>>())
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(actual, chunks);
    }

    #[test]
    fn closed_channel_never_becomes_an_infinite_heartbeat_stream() {
        let output = with_timer(stream::empty(), || futures::future::ready(()));
        assert!(block_on(output.collect::<Vec<_>>()).is_empty());
    }

    #[test]
    fn upstream_errors_are_unchanged_and_terminal() {
        let source = stream::iter([
            Err(worker::Error::RustError("failed".into())),
            Ok(b"unreachable".to_vec()),
        ]);
        let output = with_timer(source, || futures::future::ready(()));
        let actual = block_on(output.collect::<Vec<_>>());
        assert_eq!(actual.len(), 1);
        assert!(matches!(&actual[0], Err(worker::Error::RustError(code)) if code == "failed"));
    }
}
