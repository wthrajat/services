//! This module is responsible for updating the database, for each settlement
//! event that is emitted by the settlement contract.
//
// When we put settlement transactions on chain there is no reliable way to
// know the transaction hash because we can create multiple transactions with
// different gas prices. What we do know is the account and nonce that the
// transaction will have which is enough to uniquely identify it.
//
// We build an association between account-nonce and tx hash by backfilling
// settlement events with the account and nonce of their tx hash. This happens
// in an always running background task.
//
// Alternatively we could change the event insertion code to do this but I (vk)
// would like to keep that code as fast as possible to not slow down event
// insertion which also needs to deal with reorgs. It is also nicer from a code
// organization standpoint.

// 2. Inserting settlement observations
//
// see database/sql/V048__create_settlement_rewards.sql
//
// Surplus and fees calculation is based on:
// a) the mined transaction call data
// b) the auction external prices fetched from orderbook
// c) the orders fetched from orderbook
// After a transaction is mined we calculate the surplus and fees for each
// transaction and insert them into the database (settlement_observations
// table).

use {
    crate::{
        database::{
            on_settlement_event_updater::{AuctionData, SettlementUpdate},
            Postgres,
        },
        decoded_settlement::DecodedSettlement,
        infra,
    },
    anyhow::{Context, Result},
    futures::StreamExt,
    primitive_types::H256,
    shared::external_prices::ExternalPrices,
    sqlx::PgConnection,
    web3::types::Transaction,
};

pub struct OnSettlementEventUpdater {
    pub eth: infra::Ethereum,
    pub db: Postgres,
}

enum AuctionIdRecoveryStatus {
    /// The auction id was recovered and the auction data should be added.
    AddAuctionData(i64, DecodedSettlement),
    /// The auction id was recovered but the auction data should not be added.
    DoNotAddAuctionData(i64),
    /// The auction id was not recovered.
    InvalidCalldata,
}

impl OnSettlementEventUpdater {
    pub async fn run_forever(self) -> ! {
        let mut current_block = self.eth.current_block().borrow().to_owned();
        let mut block_stream = ethrpc::current_block::into_stream(self.eth.current_block().clone());
        loop {
            match self.update().await {
                Ok(true) => {
                    tracing::debug!(
                        block = current_block.number,
                        "on settlement event updater ran and processed event"
                    );
                    // Don't wait until next block in case there are more pending events to process.
                    continue;
                }
                Ok(false) => {
                    tracing::debug!(
                        block = current_block.number,
                        "on settlement event updater ran without update"
                    );
                }
                Err(err) => {
                    tracing::error!(?err, "on settlement event update task failed");
                }
            }
            current_block = block_stream.next().await.expect("blockchains never end");
        }
    }

    /// Update database for settlement events that have not been processed yet.
    ///
    /// Returns whether an update was performed.
    async fn update(&self) -> Result<bool> {
        let mut ex = self
            .db
            .pool
            .begin()
            .await
            .context("acquire DB connection")?;
        let event = match database::settlements::get_settlement_without_auction(&mut ex)
            .await
            .context("get_settlement_event_without_tx_info")?
        {
            Some(event) => event,
            None => return Ok(false),
        };

        let hash = H256(event.tx_hash.0);
        tracing::debug!("updating settlement details for tx {hash:?}");

        let Some(transaction) = self.eth.transaction(hash).await? else {
            tracing::warn!(?hash, "no tx found, reorg happened");
            return Ok(false);
        };

        let (auction_id, auction_data) =
            match Self::recover_auction_id_from_calldata(&mut ex, &transaction).await? {
                AuctionIdRecoveryStatus::InvalidCalldata => {
                    // To not get stuck on indexing the same transaction over and over again, we
                    // insert the default auction ID (0)
                    (Default::default(), None)
                }
                AuctionIdRecoveryStatus::DoNotAddAuctionData(auction_id) => (auction_id, None),
                AuctionIdRecoveryStatus::AddAuctionData(auction_id, settlement) => (
                    auction_id,
                    Some(
                        self.fetch_auction_data(hash, settlement, auction_id, &mut ex)
                            .await?,
                    ),
                ),
            };

        let update = SettlementUpdate {
            block_number: event.block_number,
            log_index: event.log_index,
            auction_id,
            auction_data,
        };

        tracing::debug!(?hash, ?update, "updating settlement details for tx");

        Postgres::update_settlement_details(&mut ex, update.clone())
            .await
            .with_context(|| format!("insert_settlement_details: {update:?}"))?;
        ex.commit().await?;
        Ok(true)
    }

    async fn fetch_auction_data(
        &self,
        hash: H256,
        settlement: DecodedSettlement,
        auction_id: i64,
        ex: &mut PgConnection,
    ) -> Result<AuctionData> {
        let receipt = self
            .eth
            .transaction_receipt(hash)
            .await?
            .with_context(|| format!("no receipt {hash:?}"))?;
        let gas_used = receipt
            .gas_used
            .with_context(|| format!("no gas used {hash:?}"))?;
        let effective_gas_price = receipt
            .effective_gas_price
            .with_context(|| format!("no effective gas price {hash:?}"))?;
        let auction_external_prices = Postgres::get_auction_prices(ex, auction_id)
            .await
            .with_context(|| {
                format!("no external prices for auction id {auction_id:?} and tx {hash:?}")
            })?;
        let external_prices = ExternalPrices::try_from_auction_prices(
            self.eth.contracts().weth().address(),
            auction_external_prices.clone(),
        )?;

        tracing::debug!(
            ?auction_id,
            ?auction_external_prices,
            ?external_prices,
            "observations input"
        );

        // surplus and fees calculation
        let surplus = settlement.total_surplus(&external_prices);
        let (fee, order_executions) = {
            let domain_separator = self.eth.contracts().settlement_domain_separator();
            let all_fees = settlement.all_fees(&external_prices, domain_separator);
            // total fee used for CIP20 rewards
            let fee = all_fees
                .iter()
                .fold(0.into(), |acc, fees| acc + fees.native);
            // executed surplus fees for each order execution
            let order_executions = all_fees
                .into_iter()
                .map(|fee| (fee.order, fee.executed_surplus_fee().unwrap_or(0.into())))
                .collect();
            (fee, order_executions)
        };

        Ok(AuctionData {
            surplus,
            fee,
            gas_used,
            effective_gas_price,
            order_executions,
        })
    }

    /// With solver driver colocation solvers are supposed to append the
    /// `auction_id` to the settlement calldata. This function tries to
    /// recover that `auction_id`. It also indicates whether the auction
    /// should be indexed with its metadata. (ie. if it comes from this
    /// environment and not from a different instance of the autopilot, e.g.
    /// running in barn/prod). This function only returns an error
    /// if retrying the operation makes sense.
    async fn recover_auction_id_from_calldata(
        ex: &mut PgConnection,
        tx: &Transaction,
    ) -> Result<AuctionIdRecoveryStatus> {
        let tx_from = tx.from.context("tx is missing sender")?;
        let settlement = match DecodedSettlement::new(&tx.input.0) {
            Ok(settlement) => settlement,
            Err(err) => {
                tracing::warn!(
                    ?tx,
                    ?err,
                    "could not decode settlement tx, unclear which auction it belongs to"
                );
                return Ok(AuctionIdRecoveryStatus::InvalidCalldata);
            }
        };
        let auction_id = match settlement.metadata {
            Some(bytes) => i64::from_be_bytes(bytes.0),
            None => {
                tracing::warn!(?tx, "could not recover the auction_id from the calldata");
                return Ok(AuctionIdRecoveryStatus::InvalidCalldata);
            }
        };

        let score = database::settlement_scores::fetch(ex, auction_id).await?;
        let data_already_recorded =
            database::settlements::already_processed(ex, auction_id).await?;
        match (score, data_already_recorded) {
            (None, _) => {
                tracing::debug!(
                    auction_id,
                    "calldata claims to settle auction that has no competition"
                );
                Ok(AuctionIdRecoveryStatus::DoNotAddAuctionData(auction_id))
            }
            (Some(score), _) if score.winner.0 != tx_from.0 => {
                tracing::warn!(
                    auction_id,
                    ?tx_from,
                    winner = ?score.winner,
                    "solution submitted by solver other than the winner"
                );
                Ok(AuctionIdRecoveryStatus::DoNotAddAuctionData(auction_id))
            }
            (Some(_), true) => {
                tracing::warn!(
                    auction_id,
                    "settlement data already recorded for this auction"
                );
                Ok(AuctionIdRecoveryStatus::DoNotAddAuctionData(auction_id))
            }
            (Some(_), false) => Ok(AuctionIdRecoveryStatus::AddAuctionData(
                auction_id, settlement,
            )),
        }
    }
}
