use {
    crate::{
        app_data,
        database::orders::{InsertionError, OrderStoring},
        dto,
    },
    anyhow::{Context, Result},
    chrono::Utc,
    ethcontract::H256,
    model::{
        app_data::AppDataHash,
        order::{
            Order,
            OrderCancellation,
            OrderClass,
            OrderCreation,
            OrderCreationAppData,
            OrderStatus,
            OrderUid,
            SignedOrderCancellations,
        },
        quote::QuoteId,
        DomainSeparator,
    },
    primitive_types::H160,
    shared::{
        metrics::LivenessChecking,
        order_validation::{OrderValidating, ValidationError},
    },
    std::{borrow::Cow, sync::Arc},
    thiserror::Error,
};

#[derive(prometheus_metric_storage::MetricStorage, Clone, Debug)]
#[metric(subsystem = "orderbook")]
struct Metrics {
    /// Counter for measuring order statistics.
    #[metric(labels("kind", "operation"))]
    orders: prometheus::IntCounterVec,
}

enum OrderOperation {
    Created,
    Cancelled,
}

fn operation_label(op: &OrderOperation) -> &'static str {
    match op {
        OrderOperation::Created => "created",
        OrderOperation::Cancelled => "cancelled",
    }
}

fn order_class_label(class: &OrderClass) -> &'static str {
    match class {
        OrderClass::Market => "user",
        OrderClass::Liquidity => "liquidity",
        OrderClass::Limit => "limit",
    }
}

impl Metrics {
    fn get() -> &'static Self {
        Self::instance(observe::metrics::get_storage_registry())
            .expect("unexpected error getting metrics instance")
    }

    fn on_order_operation(order: &Order, operation: OrderOperation) {
        let class = order_class_label(&order.metadata.class);
        let op = operation_label(&operation);
        Self::get().orders.with_label_values(&[class, op]).inc();
    }

    // Resets all the counters to 0 so we can always use them in Grafana queries.
    fn initialize() {
        let metrics = Self::get();
        for op in &[OrderOperation::Created, OrderOperation::Cancelled] {
            let op = operation_label(op);
            for class in &[OrderClass::Market, OrderClass::Liquidity, OrderClass::Limit] {
                let class = order_class_label(class);
                metrics.orders.with_label_values(&[class, op]).reset();
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum AddOrderError {
    #[error("duplicated order")]
    DuplicatedOrder,
    #[error("{0:?}")]
    OrderValidation(ValidationError),
    #[error("database error: {0}")]
    Database(#[from] anyhow::Error),
    #[error(
        "contract app data {contract_app_data:?} is associated with full app data {existing:?} \
         which is different from the provided {provided:?}"
    )]
    AppDataMismatch {
        contract_app_data: AppDataHash,
        provided: String,
        existing: String,
    },
}

impl AddOrderError {
    fn from_insertion(err: InsertionError, order: &Order) -> Self {
        match err {
            InsertionError::DuplicatedRecord => AddOrderError::DuplicatedOrder,
            InsertionError::DbError(err) => AddOrderError::Database(err.into()),
            InsertionError::AppDataMismatch(existing) => AddOrderError::AppDataMismatch {
                contract_app_data: order.data.app_data,
                // Unwrap because this error can only occur if full app data was set.
                provided: order.metadata.full_app_data.clone().unwrap(),
                // Unwrap because we only store utf-8 full app data.
                existing: {
                    let s = String::from_utf8_lossy(&existing);
                    if let Cow::Owned(_) = s {
                        tracing::error!(uid=%order.metadata.uid, "app data is not utf-8")
                    }
                    s.into_owned()
                },
            },
        }
    }
}

// This requires a manual implementation because the `#[from]` attribute from
// `thiserror` implies `#[source]` which requires `ValidationError: Error`,
// which it currently does not!
impl From<ValidationError> for AddOrderError {
    fn from(err: ValidationError) -> Self {
        Self::OrderValidation(err)
    }
}

#[derive(Debug, Error)]
pub enum OrderCancellationError {
    #[error("invalid signature")]
    InvalidSignature,
    #[error("signer does not match order owner")]
    WrongOwner,
    #[error("order not found")]
    OrderNotFound,
    #[error("order already cancelled")]
    AlreadyCancelled,
    #[error("order fully executed")]
    OrderFullyExecuted,
    #[error("order expired")]
    OrderExpired,
    #[error("on-chain orders cannot be cancelled with off-chain signature")]
    OnChainOrder,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Error)]
pub enum ReplaceOrderError {
    #[error("unable to cancel existing order: {0}")]
    Cancellation(#[from] OrderCancellationError),
    #[error("unable to add new order: {0}")]
    Add(#[from] AddOrderError),
    #[error("the new order is not a valid replacement for the old one")]
    InvalidReplacement,
}

impl From<ValidationError> for ReplaceOrderError {
    fn from(err: ValidationError) -> Self {
        Self::Add(err.into())
    }
}

pub struct Orderbook {
    domain_separator: DomainSeparator,
    settlement_contract: H160,
    database: crate::database::Postgres,
    order_validator: Arc<dyn OrderValidating>,
    app_data: Arc<app_data::Registry>,
}

impl Orderbook {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain_separator: DomainSeparator,
        settlement_contract: H160,
        database: crate::database::Postgres,
        order_validator: Arc<dyn OrderValidating>,
        app_data: Arc<app_data::Registry>,
    ) -> Self {
        Metrics::initialize();
        Self {
            domain_separator,
            settlement_contract,
            database,
            order_validator,
            app_data,
        }
    }

    pub async fn add_order(
        &self,
        payload: OrderCreation,
    ) -> Result<(OrderUid, Option<QuoteId>), AddOrderError> {
        let full_app_data_override = match payload.app_data {
            OrderCreationAppData::Hash { hash } => self.app_data.find(&hash).await?,
            _ => None,
        };

        let (order, quote) = self
            .order_validator
            .validate_and_construct_order(
                payload,
                &self.domain_separator,
                self.settlement_contract,
                full_app_data_override,
            )
            .await?;
        let quote_id = quote.as_ref().and_then(|quote| quote.id);

        self.database
            .insert_order(&order, quote)
            .await
            .map_err(|err| AddOrderError::from_insertion(err, &order))?;
        Metrics::on_order_operation(&order, OrderOperation::Created);

        Ok((order.metadata.uid, quote_id))
    }

    /// Finds an order for cancellation.
    ///
    /// Returns an error if the order cannot be found or cannot be cancelled.
    async fn find_order_for_cancellation(
        &self,
        order_uid: &OrderUid,
    ) -> Result<Order, OrderCancellationError> {
        let order = self
            .database
            .single_order(order_uid)
            .await?
            .ok_or(OrderCancellationError::OrderNotFound)?;

        match order.metadata.status {
            OrderStatus::PresignaturePending => return Err(OrderCancellationError::OnChainOrder),
            OrderStatus::Open if !order.signature.scheme().is_ecdsa_scheme() => {
                return Err(OrderCancellationError::OnChainOrder);
            }
            OrderStatus::Fulfilled => return Err(OrderCancellationError::OrderFullyExecuted),
            OrderStatus::Cancelled => return Err(OrderCancellationError::AlreadyCancelled),
            OrderStatus::Expired => return Err(OrderCancellationError::OrderExpired),
            _ => {}
        }

        Ok(order)
    }

    pub async fn cancel_orders(
        &self,
        cancellation: SignedOrderCancellations,
    ) -> Result<(), OrderCancellationError> {
        let mut orders = Vec::new();
        for order_uid in &cancellation.data.order_uids {
            orders.push(self.find_order_for_cancellation(order_uid).await?);
        }

        // Verify the cancellation signer is the same as the order signers
        let signer = cancellation
            .validate(&self.domain_separator)
            .map_err(|_| OrderCancellationError::InvalidSignature)?;
        if orders.iter().any(|order| signer != order.metadata.owner) {
            return Err(OrderCancellationError::WrongOwner);
        };

        // orders are already known to exist in DB at this point, and signer is
        // known to be correct!
        self.database
            .cancel_orders(cancellation.data.order_uids, Utc::now())
            .await?;

        for order in &orders {
            tracing::debug!(order_uid =% order.metadata.uid, "order cancelled");
            Metrics::on_order_operation(order, OrderOperation::Cancelled);
        }

        Ok(())
    }

    pub async fn cancel_order(
        &self,
        cancellation: OrderCancellation,
    ) -> Result<(), OrderCancellationError> {
        let order = self
            .find_order_for_cancellation(&cancellation.order_uid)
            .await?;

        // Verify the cancellation signer is the same as the order signer.
        let signer = cancellation
            .validate(&self.domain_separator)
            .map_err(|_| OrderCancellationError::InvalidSignature)?;
        if signer != order.metadata.owner {
            return Err(OrderCancellationError::WrongOwner);
        };

        // order is already known to exist in DB at this point, and signer is
        // known to be correct!
        self.database
            .cancel_order(&order.metadata.uid, Utc::now())
            .await?;

        tracing::debug!(order_uid =% order.metadata.uid, "order cancelled");
        Metrics::on_order_operation(&order, OrderOperation::Cancelled);

        Ok(())
    }

    pub async fn replace_order(
        &self,
        old_order: OrderUid,
        new_order: OrderCreation,
    ) -> Result<OrderUid, ReplaceOrderError> {
        // Replacement order signatures need to be validated meaning we cannot
        // accept `PreSign` orders, otherwise anyone can cancel a user order by
        // submitting a `PreSign` order on someone's behalf.
        new_order
            .signature
            .scheme()
            .try_to_ecdsa_scheme()
            .ok_or(ReplaceOrderError::InvalidReplacement)?;

        let old_order = self.find_order_for_cancellation(&old_order).await?;
        let (new_order, new_quote) = self
            .order_validator
            .validate_and_construct_order(
                new_order,
                &self.domain_separator,
                self.settlement_contract,
                None,
            )
            .await?;

        // Verify that the new order is a valid replacement order by checking
        // that the `app_data` encodes an order cancellation and that both the
        // old and new orders have the same signer.
        let cancellation = OrderCancellation {
            order_uid: old_order.metadata.uid,
            ..Default::default()
        };
        if new_order.data.app_data != cancellation.hash_struct()
            || new_order.metadata.owner != old_order.metadata.owner
        {
            return Err(ReplaceOrderError::InvalidReplacement);
        }

        self.database
            .replace_order(&old_order.metadata.uid, &new_order, new_quote)
            .await
            .map_err(|err| AddOrderError::from_insertion(err, &new_order))?;
        Metrics::on_order_operation(&old_order, OrderOperation::Cancelled);
        Metrics::on_order_operation(&new_order, OrderOperation::Created);

        Ok(new_order.metadata.uid)
    }

    pub async fn get_order(&self, uid: &OrderUid) -> Result<Option<Order>> {
        self.database.single_order(uid).await
    }

    pub async fn get_orders_for_tx(&self, hash: &H256) -> Result<Vec<Order>> {
        self.database.orders_for_tx(hash).await
    }

    pub async fn get_auction(&self) -> Result<Option<dto::AuctionWithId>> {
        let auction = match self.database.most_recent_auction().await? {
            Some(auction) => auction,
            None => {
                tracing::warn!("there is no current auction");
                return Ok(None);
            }
        };
        Ok(Some(auction))
    }

    pub async fn get_user_orders(
        &self,
        owner: &H160,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<Order>> {
        self.database
            .user_orders(owner, offset, Some(limit))
            .await
            .context("get_user_orders error")
    }
}

#[async_trait::async_trait]
impl LivenessChecking for Orderbook {
    async fn is_alive(&self) -> bool {
        self.get_auction().await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::database::orders::MockOrderStoring,
        ethcontract::H160,
        mockall::predicate::eq,
        model::{
            app_data::AppDataHash,
            order::{OrderData, OrderMetadata},
            signature::Signature,
        },
        shared::order_validation::MockOrderValidating,
    };

    #[tokio::test]
    #[ignore]
    async fn postgres_replace_order_verifies_signer_and_app_data() {
        let old_order = Order {
            metadata: OrderMetadata {
                uid: OrderUid([1; 56]),
                owner: H160([1; 20]),
                ..Default::default()
            },
            data: OrderData {
                valid_to: u32::MAX,
                ..Default::default()
            },
            ..Default::default()
        };
        let new_order_uid = OrderUid([2; 56]);
        let cancellation = OrderCancellation {
            order_uid: old_order.metadata.uid,
            ..Default::default()
        };

        let mut database = MockOrderStoring::new();
        database
            .expect_single_order()
            .with(eq(old_order.metadata.uid))
            .returning({
                let old_order = old_order.clone();
                move |_| Ok(Some(old_order.clone()))
            });
        database.expect_replace_order().returning(|_, _, _| Ok(()));

        let mut order_validator = MockOrderValidating::new();
        order_validator
            .expect_validate_and_construct_order()
            .returning(move |creation, _, _, _| {
                Ok((
                    Order {
                        metadata: OrderMetadata {
                            owner: creation.from.unwrap(),
                            uid: new_order_uid,
                            ..Default::default()
                        },
                        data: creation.data(),
                        signature: creation.signature,
                        ..Default::default()
                    },
                    Default::default(),
                ))
            });

        let database = crate::database::Postgres::new("postgresql://").unwrap();
        database::clear_DANGER(&database.pool).await.unwrap();
        database.insert_order(&old_order, None).await.unwrap();
        let app_data = Arc::new(app_data::Registry::new(
            shared::app_data::Validator::new(8192),
            database.clone(),
            None,
        ));
        let orderbook = Orderbook {
            database,
            order_validator: Arc::new(order_validator),
            domain_separator: Default::default(),
            settlement_contract: H160([0xba; 20]),
            app_data,
        };

        // App data does not encode cancellation.
        assert!(matches!(
            orderbook
                .replace_order(
                    old_order.metadata.uid,
                    OrderCreation {
                        from: Some(old_order.metadata.owner),
                        signature: Signature::Eip712(Default::default()),
                        ..Default::default()
                    },
                )
                .await,
            Err(ReplaceOrderError::InvalidReplacement)
        ));

        // Different owner
        assert!(matches!(
            orderbook
                .replace_order(
                    old_order.metadata.uid,
                    OrderCreation {
                        from: Some(H160([2; 20])),
                        signature: Signature::Eip712(Default::default()),
                        app_data: AppDataHash(cancellation.hash_struct()).into(),
                        ..Default::default()
                    },
                )
                .await,
            Err(ReplaceOrderError::InvalidReplacement)
        ));

        // Non-signed order.
        assert!(matches!(
            orderbook
                .replace_order(
                    old_order.metadata.uid,
                    OrderCreation {
                        from: Some(old_order.metadata.owner),
                        signature: Signature::PreSign,
                        app_data: AppDataHash(cancellation.hash_struct()).into(),
                        ..Default::default()
                    },
                )
                .await,
            Err(ReplaceOrderError::InvalidReplacement)
        ));

        // Stars align...
        assert_eq!(
            orderbook
                .replace_order(
                    old_order.metadata.uid,
                    OrderCreation {
                        from: Some(old_order.metadata.owner),
                        signature: Signature::Eip712(Default::default()),
                        app_data: AppDataHash(cancellation.hash_struct()).into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            new_order_uid,
        );
    }
}
