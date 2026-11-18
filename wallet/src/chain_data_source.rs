use std::collections::BTreeMap;

use bdk_kyoto::ScanType;
use bdk_wallet::chain::DescriptorExt as _;
use bdk_wallet::{KeychainKind, PersistedWallet};
use chain::CBFScanner;

use crate::error::ChainDataSourceError;
use crate::persisted::BMPWalletPersister;

#[trait_variant::make(Send)]
pub trait ChainDataSource {
    async fn sync(
        &self,
        _persister: Vec<&mut PersistedWallet<impl BMPWalletPersister>>,
    ) -> Result<(), ChainDataSourceError>;
}

impl ChainDataSource for CBFScanner {
    async fn sync(
        &self,
        mut wallets: Vec<&mut PersistedWallet<impl BMPWalletPersister>>,
    ) -> Result<(), ChainDataSourceError> {
        let network = wallets[0].network();
        let wallet_iter = wallets
            .iter()
            .map(|w| {
                let wallet_deref = &***w;
                (wallet_deref, ScanType::Sync)
            })
            .collect::<Vec<_>>();

        let descriptors_map = wallets
            .iter()
            .enumerate()
            .map(|(idx, w)| {
                let key = w.public_descriptor(KeychainKind::External).descriptor_id();
                (key, idx)
            })
            .collect::<BTreeMap<_, _>>();

        let updates = self.sync_cbf(network, wallet_iter).await?;

        for (descriptor, update) in updates {
            let idx = descriptors_map
                .get(&descriptor)
                .copied()
                .ok_or(ChainDataSourceError::UnknownDescriptor { descriptor })?;
            let wallet_to_update = wallets
                .get_mut(idx)
                .ok_or(ChainDataSourceError::MissingWallet { index: idx })?;
            let wallet_to_update = &mut **wallet_to_update;
            wallet_to_update.apply_update(update)?;
        }
        Ok(())
    }
}
