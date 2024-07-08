#![warn(unused_crate_dependencies)]

use reth_exex::{ExExContext, ExExNotification};
use reth_node_api::FullNodeComponents;
use reth_node_ethereum::EthereumNode;
use reth_tracing::tracing::info;
use std::{
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
};
pub fn unix_epoch_ms() -> u64 {
    use std::time::SystemTime;
    let now = SystemTime::now();
    now.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_else(|err| panic!("Current time {now:?} is invalid: {err:?}"))
        .as_millis() as u64
}

#[derive(Default)]
struct BlockTimeData {
    /// Wall time of last block
    wall_time_ms: u64,
    /// Timestamp of last block (chain time)
    block_timestamp: u64,
}

struct WallTimeExEx<Node: FullNodeComponents> {
    /// The context of the ExEx
    ctx: ExExContext<Node>,
    /// Time data of last block
    last_block_timedata: BlockTimeData,
}

impl<Node: FullNodeComponents> WallTimeExEx<Node> {
    fn new(ctx: ExExContext<Node>) -> Self {
        Self {
            ctx,
            last_block_timedata: BlockTimeData::default(),
        }
    }
}

impl<Node: FullNodeComponents + Unpin> Future for WallTimeExEx<Node> {
    type Output = eyre::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        while let Some(notification) = ready!(this.ctx.notifications.poll_recv(cx)) {
            match &notification {
                ExExNotification::ChainCommitted { new } => {
                    info!(committed_chain = ?new.range(), "Received commit");
                }
                ExExNotification::ChainReorged { old, new } => {
                    info!(from_chain = ?old.range(), to_chain = ?new.range(), "Received reorg");
                }
                ExExNotification::ChainReverted { old } => {
                    info!(reverted_chain = ?old.range(), "Received revert");
                }
            };

            if let Some(committed_chain) = notification.committed_chain() {
                this.last_block_timedata.block_timestamp = committed_chain.tip().timestamp;
                this.last_block_timedata.wall_time_ms = unix_epoch_ms();
            }
        }

        Poll::Ready(Ok(()))
    }
}

fn main() -> eyre::Result<()> {
    reth::cli::Cli::parse_args().run(|builder, _| async move {
        let handle = builder
            .node(EthereumNode::default())
            .install_exex("walltime", |ctx| async move { Ok(WallTimeExEx::new(ctx)) })
            .launch()
            .await?;

        handle.wait_for_node_exit().await
    })
}

#[cfg(test)]
mod tests {
    use crate::unix_epoch_ms;
    use reth::providers::{Chain, ExecutionOutcome};
    use reth::revm::db::BundleState;
    use reth_exex_test_utils::{test_exex_context, PollOnce};
    use reth_testing_utils::generators::{self, random_block, random_receipt};
    use std::pin::pin;

    #[tokio::test]
    async fn test_exex() -> eyre::Result<()> {
        let mut rng = &mut generators::rng();

        let (ctx, handle) = test_exex_context().await?;
        let mut exex = pin!(super::WallTimeExEx::new(ctx));

        // Generate first block and its state
        let block_1 = random_block(&mut rng, 0, None, Some(1), None)
            .seal_with_senders()
            .ok_or(eyre::eyre!("failed to recover senders"))?;
        let block_1_timestamp = block_1.timestamp;
        let execution_outcome1 = ExecutionOutcome::new(
            BundleState::default(),
            vec![random_receipt(&mut rng, &block_1.body[0], None)].into(),
            block_1.number,
            vec![],
        );

        // Send a notification to the Execution Extension that the chain with the first block has
        // been committed
        handle
            .send_notification_chain_committed(Chain::new(vec![block_1], execution_outcome1, None))
            .await?;
        exex.poll_once().await?;

        // Sleep for a second.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Assert that the wall time is now 1000 ms before the unix epoch, ~10ms tolerance.
        let time_difference =
            if exex.as_mut().last_block_timedata.wall_time_ms > (unix_epoch_ms() - 1000) {
                &exex.last_block_timedata.wall_time_ms - (unix_epoch_ms() - 1000)
            } else {
                (unix_epoch_ms() - 1000) - &exex.last_block_timedata.wall_time_ms
            };
        assert!(time_difference <= 5);

        // Assert that the block timestamp is correct.
        assert_eq!(exex.last_block_timedata.block_timestamp, block_1_timestamp);

        Ok(())
    }
}
