use bdk_wallet::WeightedUtxo;
use bdk_wallet::bitcoin::{Amount, FeeRate, Script, key};
use bdk_wallet::coin_selection::{
    CoinSelectionAlgorithm, CoinSelectionResult, DefaultCoinSelectionAlgorithm, InsufficientFunds,
};

#[derive(Debug)]
pub struct AlwaysSpendImportedFirst(pub Vec<WeightedUtxo>);
#[derive(Debug)]
pub struct SpendImportedOnly(pub Vec<WeightedUtxo>);

impl CoinSelectionAlgorithm for AlwaysSpendImportedFirst {
    fn coin_select<R: key::rand::RngCore>(
        &self,
        mut required_utxos: Vec<WeightedUtxo>,
        mut optional_utxos: Vec<WeightedUtxo>,
        fee_rate: FeeRate,
        target_amount: Amount,
        drain_script: &Script,
        rand: &mut R,
    ) -> Result<CoinSelectionResult, InsufficientFunds> {
        // The imported coins are spent in every transaction, so they are required. The coins
        // that the builder of the transaction requires stay required as well: a transaction that
        // must spend a certain coin, like the child that raises the fee of its parent, is
        // useless without it.
        for imported in &self.0 {
            let outpoint = imported.utxo.outpoint();
            if required_utxos
                .iter()
                .all(|required| required.utxo.outpoint() != outpoint)
            {
                required_utxos.push(imported.clone());
            }
        }
        optional_utxos.retain(|optional| {
            required_utxos
                .iter()
                .all(|required| required.utxo.outpoint() != optional.utxo.outpoint())
        });
        DefaultCoinSelectionAlgorithm::default().coin_select(
            required_utxos,
            optional_utxos,
            fee_rate,
            target_amount,
            drain_script,
            rand,
        )
    }
}

impl CoinSelectionAlgorithm for SpendImportedOnly {
    fn coin_select<R: key::rand::RngCore>(
        &self,
        _required_utxos: Vec<WeightedUtxo>,
        mut optional_utxos: Vec<WeightedUtxo>,
        fee_rate: FeeRate,
        target_amount: Amount,
        drain_script: &Script,
        rand: &mut R,
    ) -> Result<CoinSelectionResult, InsufficientFunds> {
        let imported_utxos = self.0.clone();
        let bnb = DefaultCoinSelectionAlgorithm::default();
        // Clearing the optional utxos to make sure no additional output is added.
        optional_utxos.clear();

        bnb.coin_select(
            imported_utxos.clone(),
            optional_utxos.clone(),
            fee_rate,
            target_amount,
            drain_script,
            rand,
        )
    }
}
#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::key::rand::thread_rng;
    use bdk_wallet::bitcoin::{Amount, FeeRate, ScriptBuf};
    use bdk_wallet::coin_selection::{CoinSelectionAlgorithm as _, Excess};

    use crate::coin_selection::AlwaysSpendImportedFirst;
    use crate::test_utils::{confirmed_utxo, foreign_utxo};

    #[test]
    fn test_coin_selection() {
        let mut imported_utxos = (0..1)
            .map(|i| foreign_utxo(Amount::from_int_btc(1), i))
            .collect::<Vec<_>>();

        imported_utxos.push(foreign_utxo(Amount::from_btc(0.3).unwrap(), 0));

        let local_utxos = (0..3)
            .map(|i| confirmed_utxo(Amount::from_int_btc(1), i, 1, 1_231_006_505))
            .collect::<Vec<_>>();

        let selection_strategy = AlwaysSpendImportedFirst(imported_utxos.clone());
        let target_amount = Amount::from_int_btc(1);
        let drain_script = ScriptBuf::default();

        let res = selection_strategy
            .coin_select(
                imported_utxos.clone(),
                local_utxos.clone(),
                FeeRate::from_sat_per_kwu(50_000),
                target_amount,
                &drain_script,
                &mut thread_rng(),
            )
            .unwrap();

        // Target amount is 1 BTC so the selected coins + fees should be from foreign
        let selected = res.selected;

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].txout().script_pubkey.as_bytes(), vec![0, 0, 1]);

        // Target amount is 1.5 imported only have 1.3 so this will include another output from main
        // wallet
        let target_amount = Amount::from_btc(1.5).unwrap();

        let res = selection_strategy
            .coin_select(
                imported_utxos.clone(),
                local_utxos.clone(),
                FeeRate::from_sat_per_kwu(50_000),
                target_amount,
                &drain_script,
                &mut thread_rng(),
            )
            .unwrap();

        assert_eq!(res.selected.len(), 3);
        assert!(matches!(res.excess, Excess::Change { amount: _, fee: _ }));

        // Target is 0.5 and there's no imported keys, the main wallet should be able to fulfill
        let target_amount = Amount::from_btc(0.5).unwrap();
        let selection_strategy = AlwaysSpendImportedFirst(vec![]);

        let res = selection_strategy
            .coin_select(
                Vec::new(),
                local_utxos.clone(),
                FeeRate::from_sat_per_kwu(50_000),
                target_amount,
                &drain_script,
                &mut thread_rng(),
            )
            .unwrap();

        assert_eq!(res.selected.len(), 1);
        assert!(matches!(res.excess, Excess::Change { amount: _, fee: _ }));

        // A coin that the builder of the transaction requires is spent, also when the other
        // coins would pay for the target: it stays required next to the imported coins.
        let required = local_utxos[2].clone();
        let imported_utxos = vec![
            foreign_utxo(Amount::from_int_btc(1), 0),
            foreign_utxo(Amount::from_btc(0.3).unwrap(), 1),
        ];
        let selection_strategy = AlwaysSpendImportedFirst(imported_utxos.clone());
        let res = selection_strategy
            .coin_select(
                vec![required.clone()],
                local_utxos.clone(),
                FeeRate::from_sat_per_kwu(50_000),
                target_amount,
                &drain_script,
                &mut thread_rng(),
            )
            .unwrap();
        let selected = res
            .selected
            .iter()
            .map(bdk_wallet::Utxo::outpoint)
            .collect::<Vec<_>>();
        assert_eq!(
            selected.len(),
            3,
            "the required coin and both imported coins"
        );
        assert!(selected.contains(&required.utxo.outpoint()));
        for imported in &imported_utxos {
            assert!(selected.contains(&imported.utxo.outpoint()));
        }
    }
}
