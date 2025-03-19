//! Utilities for serving `eth_simulateV1`

use alloy_consensus::{BlockHeader, Transaction as _, TxType};
use alloy_rpc_types_eth::{
    simulate::{SimCallResult, SimulateError, SimulatedBlock},
    transaction::TransactionRequest,
    Block, BlockTransactionsKind, Header,
};
use jsonrpsee_types::ErrorObject;
use reth_evm::{
    execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutor},
    Evm,
};
use reth_primitives_traits::{
    block::BlockTx, BlockBody as _, Recovered, RecoveredBlock, SignedTransaction, TxTy,
};
use reth_rpc_server_types::result::rpc_err;
use reth_rpc_types_compat::{block::from_block, TransactionCompat};
use reth_storage_api::noop::NoopProvider;
use revm::{
    context_interface::result::ExecutionResult,
    primitives::{Address, Bytes, TxKind},
    Database,
};

use crate::{
    error::{
        api::{FromEthApiError, FromEvmHalt},
        ToRpcError,
    },
    EthApiError, RevertError,
};

/// Errors which may occur during `eth_simulateV1` execution.
#[derive(Debug, thiserror::Error)]
pub enum EthSimulateError {
    /// Total gas limit of transactions for the block exceeds the block gas limit.
    #[error("Block gas limit exceeded by the block's transactions")]
    BlockGasLimitExceeded,
    /// Max gas limit for entire operation exceeded.
    #[error("Client adjustable limit reached")]
    GasLimitReached,
}

impl EthSimulateError {
    const fn error_code(&self) -> i32 {
        match self {
            Self::BlockGasLimitExceeded => -38015,
            Self::GasLimitReached => -38026,
        }
    }
}

impl ToRpcError for EthSimulateError {
    fn to_rpc_error(&self) -> ErrorObject<'static> {
        rpc_err(self.error_code(), self.to_string(), None)
    }
}

/// Converts all [`TransactionRequest`]s into [`Recovered`] transactions and applies them to the
/// given [`BlockExecutor`].
///
/// Returns all executed transactions and the result of the execution.
#[expect(clippy::type_complexity)]
pub fn execute_transactions<S, T>(
    mut builder: S,
    calls: Vec<TransactionRequest>,
    validation: bool,
    default_gas_limit: u64,
    chain_id: u64,
    tx_resp_builder: &T,
) -> Result<
    (
        BlockBuilderOutcome<S::Primitives>,
        Vec<ExecutionResult<<<S::Executor as BlockExecutor>::Evm as Evm>::HaltReason>>,
    ),
    EthApiError,
>
where
    S: BlockBuilder<Executor: BlockExecutor<Evm: Evm<DB: Database<Error: Into<EthApiError>>>>>,
    T: TransactionCompat<TxTy<S::Primitives>>,
{
    builder.apply_pre_execution_changes()?;

    let mut results = Vec::with_capacity(calls.len());
    for call in calls {
        // Resolve transaction, populate missing fields and enforce calls
        // correctness.
        let tx = resolve_transaction(
            call,
            validation,
            default_gas_limit,
            chain_id,
            builder.evm_mut().db_mut(),
            tx_resp_builder,
        )?;

        builder
            .execute_transaction_with_result_closure(tx, |result| results.push(result.clone()))?;
    }

    // Pass noop provider to skip state root calculations.
    let result = builder.finish(NoopProvider::default())?;

    Ok((result, results))
}

/// Goes over the list of [`TransactionRequest`]s and populates missing fields trying to resolve
/// them into primitive transactions.
///
/// If validation is enabled, the function will return error if any of the transactions can't be
/// built right away.
pub fn resolve_transaction<DB: Database, Tx, T: TransactionCompat<Tx>>(
    mut tx: TransactionRequest,
    validation: bool,
    default_gas_limit: u64,
    chain_id: u64,
    db: &mut DB,
    tx_resp_builder: &T,
) -> Result<Recovered<Tx>, EthApiError>
where
    DB::Error: Into<EthApiError>,
{
    if tx.buildable_type().is_none() && validation {
        return Err(EthApiError::TransactionConversionError);
    }
    // If we're missing any fields and validation is disabled, we try filling nonce, gas and
    // gas price.
    let tx_type = tx.preferred_type();

    let from = if let Some(from) = tx.from {
        from
    } else {
        tx.from = Some(Address::ZERO);
        Address::ZERO
    };

    if tx.nonce.is_none() {
        tx.nonce =
            Some(db.basic(from).map_err(Into::into)?.map(|acc| acc.nonce).unwrap_or_default());
    }

    if tx.gas.is_none() {
        tx.gas = Some(default_gas_limit);
    }

    if tx.chain_id.is_none() {
        tx.chain_id = Some(chain_id);
    }

    if tx.to.is_none() {
        tx.to = Some(TxKind::Create);
    }

    match tx_type {
        TxType::Legacy | TxType::Eip2930 => {
            if tx.gas_price.is_none() {
                tx.gas_price = Some(0);
            }
        }
        _ => {
            if tx.max_fee_per_gas.is_none() {
                tx.max_fee_per_gas = Some(0);
                tx.max_priority_fee_per_gas = Some(0);
            }
        }
    }

    let tx = tx_resp_builder
        .build_simulate_v1_transaction(tx)
        .map_err(|e| EthApiError::other(e.into()))?;

    Ok(Recovered::new_unchecked(tx, from))
}

/// Handles outputs of the calls execution and builds a [`SimulatedBlock`].
#[expect(clippy::type_complexity)]
pub fn build_simulated_block<T, B, Halt: Clone>(
    block: RecoveredBlock<B>,
    results: Vec<ExecutionResult<Halt>>,
    full_transactions: bool,
    tx_resp_builder: &T,
) -> Result<SimulatedBlock<Block<T::Transaction, Header<B::Header>>>, T::Error>
where
    T: TransactionCompat<BlockTx<B>, Error: FromEthApiError + FromEvmHalt<Halt>>,
    B: reth_primitives_traits::Block,
{
    let mut calls: Vec<SimCallResult> = Vec::with_capacity(results.len());

    let mut log_index = 0;
    for (index, (result, tx)) in results.iter().zip(block.body().transactions()).enumerate() {
        let call = match result {
            ExecutionResult::Halt { reason, gas_used } => {
                let error = T::Error::from_evm_halt(reason.clone(), tx.gas_limit());
                SimCallResult {
                    return_data: Bytes::new(),
                    error: Some(SimulateError {
                        message: error.to_string(),
                        code: error.into().code(),
                    }),
                    gas_used: *gas_used,
                    logs: Vec::new(),
                    status: false,
                }
            }
            ExecutionResult::Revert { output, gas_used } => {
                let error = RevertError::new(output.clone());
                SimCallResult {
                    return_data: output.clone(),
                    error: Some(SimulateError {
                        code: error.error_code(),
                        message: error.to_string(),
                    }),
                    gas_used: *gas_used,
                    status: false,
                    logs: Vec::new(),
                }
            }
            ExecutionResult::Success { output, gas_used, logs, .. } => SimCallResult {
                return_data: output.clone().into_data(),
                error: None,
                gas_used: *gas_used,
                logs: logs
                    .iter()
                    .map(|log| {
                        log_index += 1;
                        alloy_rpc_types_eth::Log {
                            inner: log.clone(),
                            log_index: Some(log_index - 1),
                            transaction_index: Some(index as u64),
                            transaction_hash: Some(*tx.tx_hash()),
                            block_number: Some(block.header().number()),
                            block_timestamp: Some(block.header().timestamp()),
                            ..Default::default()
                        }
                    })
                    .collect(),
                status: true,
            },
        };

        calls.push(call);
    }

    let txs_kind =
        if full_transactions { BlockTransactionsKind::Full } else { BlockTransactionsKind::Hashes };

    let block = from_block(block, txs_kind, tx_resp_builder)?;
    Ok(SimulatedBlock { inner: block, calls })
}
