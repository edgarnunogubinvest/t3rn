// This file is part of Substrate.

// Copyright (C) 2017-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Substrate block-author/full-node API.

// #[cfg(test)]
// mod tests;

mod api;

use crate::api::GatewayApi;

use log::warn;
use std::sync::Arc;

use sp_blockchain::HeaderBackend;

use codec::{Decode, Encode};
use futures::future::{ready, FutureExt, TryFutureExt};
use futures::{compat::Compat, StreamExt as _};
use jsonrpc_core::futures::{future::result, Future, Sink};
use jsonrpc_pubsub::{manager::SubscriptionManager, typed::Subscriber, SubscriptionId};
use sc_rpc_api::DenyUnsafe;
use sp_api::ProvideRuntimeApi;
use sp_core::Bytes;
use sp_keystore::SyncCryptoStorePtr;
use sp_runtime::generic;
use sp_session::SessionKeys;
use sp_transaction_pool::{
    error::IntoPoolError, BlockHash, InPoolTransaction, TransactionFor, TransactionPool,
    TransactionSource, TransactionStatus, TxHash,
};

use self::error::{FutureResult, Result};
/// Re-export the API for backward compatibility.
pub use sc_rpc_api::author::*;

/// Authoring API
pub struct Gateway<P, Client> {
    /// Substrate client
    client: Arc<Client>,
    /// Transactions pool
    pool: Arc<P>,
    /// Subscriptions manager
    subscriptions: SubscriptionManager,
    /// The key store.
    _keystore: SyncCryptoStorePtr,
    /// Whether to deny unsafe calls
    deny_unsafe: DenyUnsafe,
}

impl<P, Client> Gateway<P, Client> {
    /// Create new instance of Authoring API.
    pub fn new(
        client: Arc<Client>,
        pool: Arc<P>,
        subscriptions: SubscriptionManager,
        _keystore: SyncCryptoStorePtr,
        deny_unsafe: DenyUnsafe,
    ) -> Self {
        Gateway {
            client,
            pool,
            subscriptions,
            _keystore,
            deny_unsafe,
        }
    }
}

/// Currently we treat all RPC transactions as externals.
///
/// Possibly in the future we could allow opt-in for special treatment
/// of such transactions, so that the block authors can inject
/// some unique transactions via RPC and have them included in the pool.
const TX_SOURCE: TransactionSource = TransactionSource::External;

impl<P, Client> GatewayApi<TxHash<P>, BlockHash<P>> for Gateway<P, Client>
where
    P: TransactionPool + Sync + Send + 'static,
    Client: HeaderBackend<P::Block> + ProvideRuntimeApi<P::Block> + Send + Sync + 'static,
    Client::Api: SessionKeys<P::Block>,
{
    type Metadata = sc_rpc::Metadata;

    fn submit_extrinsic(&self, ext: Bytes) -> FutureResult<TxHash<P>> {
        let xt = match Decode::decode(&mut &ext[..]) {
            Ok(xt) => xt,
            Err(err) => return Box::new(result(Err(err.into()))),
        };
        let best_block_hash = self.client.info().best_hash;
        Box::new(
            self.pool
                .submit_one(&generic::BlockId::hash(best_block_hash), TX_SOURCE, xt)
                .compat()
                .map_err(|e| {
                    e.into_pool_error()
                        .map(Into::into)
                        .unwrap_or_else(|e| error::Error::Verification(Box::new(e)).into())
                }),
        )
    }

    fn pending_extrinsics(&self) -> Result<Vec<Bytes>> {
        Ok(self
            .pool
            .ready()
            .map(|tx| tx.data().encode().into())
            .collect())
    }

    fn remove_extrinsic(
        &self,
        bytes_or_hash: Vec<crate::api::ExtrinsicOrHash<TxHash<P>>>,
    ) -> Result<Vec<TxHash<P>>> {
        self.deny_unsafe.check_if_safe()?;

        let hashes = bytes_or_hash
            .into_iter()
            .map(|x| match x {
                crate::api::ExtrinsicOrHash::Hash(h) => Ok(h),
                crate::api::ExtrinsicOrHash::Extrinsic(bytes) => {
                    let xt = Decode::decode(&mut &bytes[..])?;
                    Ok(self.pool.hash_of(&xt))
                }
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(self
            .pool
            .remove_invalid(&hashes)
            .into_iter()
            .map(|tx| tx.hash().clone())
            .collect())
    }

    fn watch_extrinsic(
        &self,
        _metadata: Self::Metadata,
        subscriber: Subscriber<TransactionStatus<TxHash<P>, BlockHash<P>>>,
        xt: Bytes,
    ) {
        let submit = || -> Result<_> {
            let best_block_hash = self.client.info().best_hash;
            let dxt = TransactionFor::<P>::decode(&mut &xt[..]).map_err(error::Error::from)?;
            Ok(self
                .pool
                .submit_and_watch(&generic::BlockId::hash(best_block_hash), TX_SOURCE, dxt)
                .map_err(|e| {
                    e.into_pool_error()
                        .map(error::Error::from)
                        .unwrap_or_else(|e| error::Error::Verification(Box::new(e)).into())
                }))
        };

        let subscriptions = self.subscriptions.clone();
        let future = ready(submit())
            .and_then(|res| res)
            // convert the watcher into a `Stream`
            .map(|res| res.map(|stream| stream.map(|v| Ok::<_, ()>(Ok(v)))))
            // now handle the import result,
            // start a new subscrition
            .map(move |result| match result {
                Ok(watcher) => {
                    subscriptions.add(subscriber, move |sink| {
                        sink.sink_map_err(|e| log::debug!("Subscription sink failed: {:?}", e))
                            .send_all(Compat::new(watcher))
                            .map(|_| ())
                    });
                }
                Err(err) => {
                    warn!("Failed to submit extrinsic: {}", err);
                    // reject the subscriber (ignore errors - we don't care if subscriber is no longer there).
                    let _ = subscriber.reject(err.into());
                }
            });

        let res = self
            .subscriptions
            .executor()
            .execute(Box::new(Compat::new(future.map(|_| Ok(())))));
        if res.is_err() {
            warn!("Error spawning subscription RPC task.");
        }
    }

    fn unwatch_extrinsic(
        &self,
        _metadata: Option<Self::Metadata>,
        id: SubscriptionId,
    ) -> Result<bool> {
        Ok(self.subscriptions.cancel(id))
    }
}
