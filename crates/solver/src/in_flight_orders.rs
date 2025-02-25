use {
    crate::settlement::{Settlement, TradeExecution},
    itertools::Itertools,
    model::{
        auction::Auction,
        order::{Order, OrderKind, OrderUid},
    },
    number::conversions::u256_to_big_uint,
    std::collections::{BTreeMap, HashMap, HashSet},
};

#[derive(Debug, Clone)]
struct PartiallyFilledOrder {
    order: Order,
    in_flight_trades: Vec<TradeExecution>,
}

impl PartiallyFilledOrder {
    pub fn order_with_remaining_amounts(&self) -> Order {
        let mut updated_order = self.order.clone();

        for trade in &self.in_flight_trades {
            updated_order.metadata.executed_buy_amount += u256_to_big_uint(&trade.buy_amount);
            updated_order.metadata.executed_sell_amount +=
                u256_to_big_uint(&(trade.sell_amount + trade.fee_amount));
            updated_order.metadata.executed_sell_amount_before_fees += trade.sell_amount;
            updated_order.metadata.executed_fee_amount += trade.fee_amount;
        }

        updated_order
    }
}

/// After a settlement transaction we need to keep track of in flight orders
/// until the api has seen the tx. Otherwise we would attempt to solve already
/// matched orders again leading to failures.
#[derive(Default)]
pub struct InFlightOrders {
    /// Maps block to orders settled in that block.
    in_flight: BTreeMap<u64, Vec<OrderUid>>,
    /// Tracks in flight trades which use liquidity from partially fillable
    /// orders.
    in_flight_trades: HashMap<OrderUid, PartiallyFilledOrder>,
}

impl InFlightOrders {
    /// Takes note of the new set of solvable orders and returns the ones that
    /// aren't in flight and scales down partially fillable orders if there
    /// are currently orders in-flight tapping into their executable
    /// amounts. Returns the set of order uids that are considered in
    /// flight.
    pub fn update_and_filter(&mut self, auction: &mut Auction) -> HashSet<OrderUid> {
        let uids = |in_flight: &BTreeMap<u64, Vec<OrderUid>>| {
            in_flight
                .values()
                .flatten()
                .copied()
                .collect::<HashSet<_>>()
        };
        let inflight_before = uids(&self.in_flight);
        let orders_before = auction.orders.len();

        // If api has seen block X then trades starting at X + 1 are still in flight.
        self.in_flight = self
            .in_flight
            .split_off(&(auction.latest_settlement_block + 1));

        let in_flight = uids(&self.in_flight);
        self.in_flight_trades
            .retain(|uid, _| in_flight.contains(uid));

        auction.orders.iter_mut().for_each(|order| {
            let uid = &order.metadata.uid;

            if order.data.partially_fillable {
                if let Some(trades) = self.in_flight_trades.get(uid) {
                    *order = trades.order_with_remaining_amounts();
                }
            } else if in_flight.contains(uid) {
                // fill-or-kill orders can only be used once and there is already a trade in
                // flight for this one => Modify it such that it gets filtered
                // out in the next step.
                order.metadata.executed_buy_amount = u256_to_big_uint(&order.data.buy_amount);
                order.metadata.executed_sell_amount_before_fees = order.data.sell_amount;
            }
        });
        auction.orders.retain(|order| match order.data.kind {
            OrderKind::Sell => {
                u256_to_big_uint(&order.data.sell_amount)
                    > u256_to_big_uint(&order.metadata.executed_sell_amount_before_fees)
            }
            OrderKind::Buy => {
                u256_to_big_uint(&order.data.buy_amount) > order.metadata.executed_buy_amount
            }
        });

        tracing::trace!(
            auction_block = %auction.block,
            latest_settlement_block = %auction.latest_settlement_block,
            inflight_before_count = %inflight_before.len(),
            inflight_after_count = %in_flight.len(),
            orders_before_count = %orders_before,
            orders_after_count = %auction.orders.len(),
            inflight_before = ?inflight_before,
            inflight_after = ?in_flight,
            "inflight stats"
        );

        in_flight
    }

    /// Tracks all in_flight orders and how much of the executable amount of
    /// partially fillable orders is currently used in in-flight trades.
    pub fn mark_settled_orders(&mut self, block: u64, settlement: &Settlement) {
        let uids = settlement.traded_orders().map(|order| order.metadata.uid);
        self.in_flight.entry(block).or_default().extend(uids);

        settlement
            .trades()
            .zip(settlement.trade_executions())
            .filter(|(trade, _)| trade.order.data.partially_fillable)
            .into_group_map_by(|(trade, _)| trade.order.metadata.uid)
            .into_iter()
            .for_each(|(uid, trades)| {
                let most_recent_data = PartiallyFilledOrder {
                    order: trades[0].0.order.clone(),
                    in_flight_trades: trades.into_iter().map(|(_, execution)| execution).collect(),
                };
                // always overwrite existing data with the most recent data
                self.in_flight_trades.insert(uid, most_recent_data);
            });
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::settlement::{SettlementEncoder, Trade},
        maplit::hashmap,
        model::order::{Order, OrderData, OrderKind, OrderMetadata},
        primitive_types::H160,
    };

    #[test]
    fn test() {
        let token0 = H160::from_low_u64_be(0);
        let token1 = H160::from_low_u64_be(1);

        let fill_or_kill = Order {
            data: OrderData {
                sell_token: token0,
                buy_token: token1,
                sell_amount: 100u8.into(),
                buy_amount: 100u8.into(),
                kind: OrderKind::Sell,
                ..Default::default()
            },
            metadata: OrderMetadata {
                uid: OrderUid::from_integer(1),
                ..Default::default()
            },
            ..Default::default()
        };

        // partially fillable order 30% filled
        let mut partially_fillable_1 = fill_or_kill.clone();
        partially_fillable_1.data.partially_fillable = true;
        partially_fillable_1.metadata.uid = OrderUid::from_integer(2);
        partially_fillable_1.metadata.executed_buy_amount = 30u8.into();
        partially_fillable_1.metadata.executed_sell_amount = 30u8.into();
        partially_fillable_1
            .metadata
            .executed_sell_amount_before_fees = 30u8.into();

        // a different partially fillable order 30% filled
        let mut partially_fillable_2 = partially_fillable_1.clone();
        partially_fillable_2.metadata.uid = OrderUid::from_integer(3);

        let trades = vec![
            Trade {
                order: fill_or_kill.clone(),
                executed_amount: 100u8.into(),
                ..Default::default()
            },
            // This order uses some of the remaining executable amount of partially_fillable_1
            Trade {
                order: partially_fillable_2.clone(),
                executed_amount: 20u8.into(),
                ..Default::default()
            },
            // Following orders use remaining executable amount of partially_fillable_2
            Trade {
                order: partially_fillable_1.clone(),
                executed_amount: 50u8.into(),
                ..Default::default()
            },
            Trade {
                order: partially_fillable_1.clone(),
                executed_amount: 20u8.into(),
                ..Default::default()
            },
        ];

        let prices = hashmap! {token0 => 1u8.into(), token1 => 1u8.into()};
        let settlement = Settlement {
            encoder: SettlementEncoder::with_trades(prices, trades),
            ..Default::default()
        };

        let mut inflight = InFlightOrders::default();
        inflight.mark_settled_orders(1, &settlement);
        let mut order0 = fill_or_kill.clone();
        order0.metadata.uid = OrderUid::from_integer(0);
        let mut auction = Auction {
            block: 0,
            orders: vec![
                order0,
                fill_or_kill,
                partially_fillable_1,
                partially_fillable_2,
            ],
            ..Default::default()
        };

        let mut update_and_get_filtered_orders = |auction: &Auction| {
            let mut auction = auction.clone();
            inflight.update_and_filter(&mut auction);
            auction.orders
        };

        let filtered = update_and_get_filtered_orders(&auction);
        assert_eq!(filtered.len(), 2);
        // keep order 0 because there are no trades for it in flight
        assert_eq!(filtered[0].metadata.uid, OrderUid::from_integer(0));
        // drop order 1 because it's fill-or-kill and there is already one trade in
        // flight keep order 2 and reduce remaning executable amount by trade
        // amounts currently in flight
        assert_eq!(filtered[1].metadata.uid, OrderUid::from_integer(3));
        assert_eq!(filtered[1].metadata.executed_buy_amount, 50u8.into());
        assert_eq!(filtered[1].metadata.executed_sell_amount, 50u8.into());
        assert_eq!(
            filtered[1].metadata.executed_sell_amount_before_fees,
            50u8.into()
        );
        // drop order 3 because in flight orders filled the remaining executable amount

        auction.block = 1;
        let filtered = update_and_get_filtered_orders(&auction);
        // same behaviour as above
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].metadata.uid, OrderUid::from_integer(0));
        assert_eq!(filtered[1].metadata.uid, OrderUid::from_integer(3));
        assert_eq!(filtered[1].metadata.executed_buy_amount, 50u8.into());
        assert_eq!(
            filtered[1].metadata.executed_sell_amount_before_fees,
            50u8.into()
        );

        auction.latest_settlement_block = 1;
        let filtered = update_and_get_filtered_orders(&auction);
        // Because we drop all in-flight trades from blocks older than the settlement
        // block there is nothing left to filter solvable orders by => keep all
        // orders unaltered
        assert_eq!(filtered.len(), 4);
    }

    #[test]
    fn test_order_is_not_excluded_when_min_buy_amount_is_reached() {
        let order = Order {
            data: OrderData {
                sell_token: H160::from_low_u64_be(0),
                buy_token: H160::from_low_u64_be(1),
                sell_amount: 100u8.into(),
                buy_amount: 100u8.into(),
                kind: OrderKind::Sell,
                partially_fillable: true,
                ..Default::default()
            },
            metadata: OrderMetadata {
                uid: OrderUid::from_integer(1),
                // Only half filled but min buy amount already reached
                executed_sell_amount: 50u8.into(),
                executed_buy_amount: 100u8.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut auction = Auction {
            block: 0,
            orders: vec![order],
            ..Default::default()
        };
        let mut inflight = InFlightOrders::default();
        inflight.update_and_filter(&mut auction);
        assert_eq!(auction.orders.len(), 1);
    }

    #[test]
    fn test_filled_buy_order_gets_filtered() {
        let order = Order {
            data: OrderData {
                sell_token: H160::from_low_u64_be(0),
                buy_token: H160::from_low_u64_be(1),
                sell_amount: 100u8.into(),
                buy_amount: 100u8.into(),
                kind: OrderKind::Buy,
                ..Default::default()
            },
            metadata: OrderMetadata {
                uid: OrderUid::from_integer(1),
                // Filled with a lot of surplus (only needed to sell half of maxSellAmount)
                executed_sell_amount: 50u8.into(),
                executed_buy_amount: 100u8.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut auction = Auction {
            block: 0,
            orders: vec![order],
            ..Default::default()
        };
        let mut inflight = InFlightOrders::default();
        inflight.update_and_filter(&mut auction);
        assert_eq!(auction.orders.len(), 0);
    }
}
