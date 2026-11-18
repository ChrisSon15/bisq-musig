//! Trade parameters, constants of the protocol and configuration values of the engine. The rules
//! are specified in `docs/specifications/trade/parameters.md`.

use std::fmt::{self, Display, Formatter};

use bdk_wallet::bitcoin::address::NetworkUnchecked;
use bdk_wallet::bitcoin::amount::CheckedSum as _;
use bdk_wallet::bitcoin::relative::LockTime;
use bdk_wallet::bitcoin::{Amount, FeeRate, Network};
use thiserror::Error;

use crate::receiver::Receiver;
use crate::transaction::{
    ANCHOR_AMOUNT, RedirectTxBuilder, SIGNED_FORWARDING_TX_WEIGHT, WarningTxBuilder,
};

/// The relative lock time of both redirect transactions. With a lock time of one block, a redirect
/// transaction cannot enter the mempool before the warning transaction is confirmed, so the peer
/// cannot block a fee increase of the warning transaction.
pub const REDIRECT_LOCK_TIME: LockTime = LockTime::from_height(REDIRECT_LOCK_TIME_BLOCKS);
const REDIRECT_LOCK_TIME_BLOCKS: u16 = 1;

/// The number of confirmations at which the deposit transaction counts as confirmed. Below this
/// depth no signature for the swap transaction and no key share may leave the engine.
pub const MIN_CONFIRMATIONS: u16 = 3;

/// The fee rate of the transactions that the engine builds on its own, for example the drain
/// transaction at the close of the wallet.
// TODO: Use a current fee rate. The source of fee rates is not decided yet (point N1 of the plan).
pub const fn engine_fee_rate() -> FeeRate {
    // 10 sat per vbyte.
    FeeRate::from_sat_per_kwu(2_500)
}

/// The parameters of one trade. They are part of the contract of the two traders, and both must
/// use the same values. The caller of the engine supplies them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TradeParams {
    pub trade_amount: Amount,
    pub buyers_security_deposit: Amount,
    pub sellers_security_deposit: Amount,
    pub deposit_tx_fee_rate: FeeRate,
    /// The fee rate of the swap, warning and redirect transactions.
    pub prepared_tx_fee_rate: FeeRate,
    /// At most one output of the deposit transaction that the seller pays.
    pub trade_fee_receiver: Option<Receiver<NetworkUnchecked>>,
}

/// The configuration of one trader's engine. It is not part of a request: the program that runs
/// the engine reads it from its own configuration.
///
/// The lock times decide the transactions of a trade, so the engines of both traders must have
/// the same lock times. The engine cannot compare them directly. If they differ, the partial
/// signatures of the peer are not valid for the trader's transactions, and the setup fails before
/// the deposit transaction is signed.
///
/// A value of this type satisfies the limits of the specification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineConfig {
    warning_lock_time: u16,
    claim_lock_time: u16,
    deposit_timeout: u16,
    release_safety_margin: u16,
    forced_close_margin: u16,
}

impl EngineConfig {
    // TODO: The default values are not decided yet (open point O7 of the specification). The
    //  lock times are the values that the engine used for mainnet before they became
    //  configuration values.
    pub const DEFAULT_WARNING_LOCK_TIME: u16 = 144 * 10;
    pub const DEFAULT_CLAIM_LOCK_TIME: u16 = 144 * 5;
    pub const DEFAULT_DEPOSIT_TIMEOUT: u16 = 144;
    pub const DEFAULT_RELEASE_SAFETY_MARGIN: u16 = 6;

    /// Creates a configuration, or returns an error if a value is outside the limits. All values
    /// are numbers of blocks.
    ///
    /// - `warning_lock_time`: the relative lock time of both warning transactions, counted from the
    ///   confirmation of the deposit transaction.
    /// - `claim_lock_time`: the relative lock time of both claim transactions, counted from the
    ///   confirmation of the warning transaction.
    /// - `deposit_timeout`: the number of blocks without a confirmed deposit after which the trader
    ///   aborts the deposit.
    /// - `release_safety_margin`: the seller does not release the payout when fewer blocks than
    ///   this remain before the buyer's warning transaction becomes valid.
    pub fn new(
        warning_lock_time: u16,
        claim_lock_time: u16,
        deposit_timeout: u16,
        release_safety_margin: u16,
    ) -> Result<Self, EngineConfigError> {
        let minimum_warning_lock_time =
            u32::from(MIN_CONFIRMATIONS) + u32::from(release_safety_margin);
        if u32::from(warning_lock_time) <= minimum_warning_lock_time {
            return Err(EngineConfigError(format!(
                "the warning lock time of {warning_lock_time} blocks is not larger than \
                 {minimum_warning_lock_time} blocks (minimum confirmations plus release safety \
                 margin)"
            )));
        }
        if claim_lock_time <= REDIRECT_LOCK_TIME_BLOCKS {
            return Err(EngineConfigError(format!(
                "the claim lock time of {claim_lock_time} blocks is not larger than the redirect \
                 lock time of {REDIRECT_LOCK_TIME_BLOCKS} block"
            )));
        }
        Ok(Self {
            warning_lock_time,
            claim_lock_time,
            deposit_timeout,
            release_safety_margin,
            forced_close_margin: release_safety_margin,
        })
    }

    /// Sets the forced close margin: the seller's engine publishes the swap transaction on its
    /// own when this number of blocks, or fewer, remain before the buyer's warning transaction
    /// becomes valid. Without this call the margin is the release safety margin.
    ///
    /// The margin must not be smaller than the release safety margin, which is the time that
    /// the swap transaction needs to confirm. It must be smaller than the warning lock time
    /// minus the confirmations that a deposit needs, because otherwise every release would be
    /// followed by the swap transaction at once.
    pub fn with_forced_close_margin(
        mut self,
        forced_close_margin: u16,
    ) -> Result<Self, EngineConfigError> {
        if forced_close_margin < self.release_safety_margin {
            return Err(EngineConfigError(format!(
                "the forced close margin of {forced_close_margin} blocks is smaller than the \
                 release safety margin of {} blocks",
                self.release_safety_margin
            )));
        }
        let limit = u32::from(self.warning_lock_time) - u32::from(MIN_CONFIRMATIONS);
        if u32::from(forced_close_margin) >= limit {
            return Err(EngineConfigError(format!(
                "the forced close margin of {forced_close_margin} blocks is not smaller than \
                 {limit} blocks (warning lock time minus minimum confirmations)"
            )));
        }
        self.forced_close_margin = forced_close_margin;
        Ok(self)
    }

    pub const fn warning_lock_time(&self) -> LockTime {
        LockTime::from_height(self.warning_lock_time)
    }

    pub const fn claim_lock_time(&self) -> LockTime {
        LockTime::from_height(self.claim_lock_time)
    }

    /// The warning lock time as a number of blocks.
    pub const fn warning_lock_time_blocks(&self) -> u16 {
        self.warning_lock_time
    }

    /// The claim lock time as a number of blocks.
    pub const fn claim_lock_time_blocks(&self) -> u16 {
        self.claim_lock_time
    }

    pub const fn deposit_timeout(&self) -> u16 {
        self.deposit_timeout
    }

    pub const fn release_safety_margin(&self) -> u16 {
        self.release_safety_margin
    }

    /// The number of blocks before the buyer's warning transaction becomes valid at which the
    /// seller's engine publishes the swap transaction on its own.
    pub const fn forced_close_margin(&self) -> u16 {
        self.forced_close_margin
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            warning_lock_time: Self::DEFAULT_WARNING_LOCK_TIME,
            claim_lock_time: Self::DEFAULT_CLAIM_LOCK_TIME,
            deposit_timeout: Self::DEFAULT_DEPOSIT_TIMEOUT,
            release_safety_margin: Self::DEFAULT_RELEASE_SAFETY_MARGIN,
            forced_close_margin: Self::DEFAULT_RELEASE_SAFETY_MARGIN,
        }
    }
}

impl Display for EngineConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "warning lock time {} blocks, claim lock time {} blocks, deposit timeout {} blocks, \
             release safety margin {} blocks, forced close margin {} blocks",
            self.warning_lock_time,
            self.claim_lock_time,
            self.deposit_timeout,
            self.release_safety_margin,
            self.forced_close_margin
        )
    }
}

#[derive(Error, Debug, Eq, PartialEq)]
#[error("invalid configuration of the trade engine: {0}")]
pub struct EngineConfigError(pub String);

impl TradeParams {
    /// Checks the limits of the specification. The check needs no wallet and no data of the peer,
    /// so it can run before any coin is reserved.
    pub fn validate(&self, network: Network) -> Result<(), TradeParamsError> {
        let invalid = |reason: String| Err(TradeParamsError(reason));

        if let Some(receiver) = &self.trade_fee_receiver {
            let receiver = match receiver.clone().require_network(network) {
                Ok(receiver) => receiver,
                Err(e) => return invalid(format!("the address of the trade fee receiver: {e}")),
            };
            if receiver.amount < receiver.address.script_pubkey().minimal_non_dust() {
                return invalid(format!("the trade fee of {} is dust", receiver.amount));
            }
        }

        // Every output of every transaction of the trade must be above the dust limit. All outputs
        // that the engine creates for itself are Taproot outputs, whose dust limit is the anchor
        // amount.
        let dust_limit = ANCHOR_AMOUNT;
        let Some(buyer_payout) = self.trade_amount.checked_add(self.buyers_security_deposit) else {
            return invalid("the amounts are too large".to_owned());
        };
        if buyer_payout < dust_limit || self.sellers_security_deposit < dust_limit {
            return invalid("a payout of the deposit transaction is dust".to_owned());
        }
        let forwarding_fee = self
            .prepared_tx_fee_rate
            .checked_mul_by_weight(SIGNED_FORWARDING_TX_WEIGHT);
        let swap_output =
            forwarding_fee.and_then(|fee| self.sellers_security_deposit.checked_sub(fee));
        if swap_output.is_none_or(|amount| amount < dust_limit) {
            return invalid(
                "the seller's security deposit does not cover the fee of the swap transaction"
                    .to_owned(),
            );
        }
        let escrow_amount = [buyer_payout, self.sellers_security_deposit]
            .into_iter()
            .checked_sum()
            .and_then(|_| {
                WarningTxBuilder::escrow_amount(
                    [buyer_payout, self.sellers_security_deposit],
                    self.prepared_tx_fee_rate,
                )
                .ok()
            });
        let penalty_output = escrow_amount
            .zip(forwarding_fee)
            .and_then(|(escrow, fee)| escrow.checked_sub(fee));
        if penalty_output.is_none_or(|amount| amount < dust_limit) {
            return invalid(
                "the amounts do not cover the fees of the warning and penalty transactions"
                    .to_owned(),
            );
        }
        let redirection_amount = escrow_amount.and_then(|escrow| {
            RedirectTxBuilder::available_amount_msat(escrow, self.prepared_tx_fee_rate).ok()
        });
        if redirection_amount.is_none() {
            return invalid(
                "the amounts do not cover the fee of the redirect transaction".to_owned(),
            );
        }
        Ok(())
    }
}

#[derive(Error, Debug, Eq, PartialEq)]
#[error("invalid trade parameters: {0}")]
pub struct TradeParamsError(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_params() -> TradeParams {
        TradeParams {
            trade_amount: Amount::from_sat(200_000),
            buyers_security_deposit: Amount::from_sat(30_000),
            sellers_security_deposit: Amount::from_sat(30_000),
            deposit_tx_fee_rate: FeeRate::from_sat_per_kwu(3_125),
            prepared_tx_fee_rate: FeeRate::from_sat_per_kwu(2_500),
            trade_fee_receiver: None,
        }
    }

    #[track_caller]
    fn assert_invalid(params: &TradeParams, expected: &str) {
        let error = params
            .validate(Network::Regtest)
            .expect_err("params are invalid");
        assert!(error.0.contains(expected), "unexpected error: {error}");
    }

    #[track_caller]
    fn assert_invalid_config(config: Result<EngineConfig, EngineConfigError>, expected: &str) {
        let error = config.expect_err("the configuration is invalid");
        assert!(error.0.contains(expected), "unexpected error: {error}");
    }

    #[test]
    fn valid_params_are_accepted() {
        assert_eq!(valid_params().validate(Network::Regtest), Ok(()));
    }

    /// The default configuration satisfies the limits.
    #[test]
    fn default_config_is_valid() {
        let config = EngineConfig::new(
            EngineConfig::DEFAULT_WARNING_LOCK_TIME,
            EngineConfig::DEFAULT_CLAIM_LOCK_TIME,
            EngineConfig::DEFAULT_DEPOSIT_TIMEOUT,
            EngineConfig::DEFAULT_RELEASE_SAFETY_MARGIN,
        );
        assert_eq!(config, Ok(EngineConfig::default()));
    }

    /// A12: the warning lock time must be larger than the minimum confirmations plus the release
    /// safety margin, and the claim lock time must be larger than the redirect lock time. A
    /// configuration outside these limits cannot be created.
    #[test]
    fn a12_lock_times_outside_the_limits_are_rejected() {
        let release_safety_margin = 6;
        assert_invalid_config(
            EngineConfig::new(9, 10, 144, release_safety_margin),
            "warning lock time of 9 blocks",
        );
        let config = EngineConfig::new(10, 10, 144, release_safety_margin).expect("valid");
        assert_eq!(config.warning_lock_time(), LockTime::from_height(10));
        assert_eq!(config.claim_lock_time(), LockTime::from_height(10));
        assert_eq!(config.deposit_timeout(), 144);
        assert_eq!(config.release_safety_margin(), 6);

        // Another margin changes the limit (A15).
        let release_safety_margin = 0;
        assert!(EngineConfig::new(4, 10, 144, release_safety_margin).is_ok());
        assert_invalid_config(
            EngineConfig::new(3, 10, 144, release_safety_margin),
            "warning lock time of 3 blocks",
        );

        for claim_lock_time in [0, 1] {
            assert_invalid_config(
                EngineConfig::new(20, claim_lock_time, 144, 6),
                "claim lock time",
            );
        }
        assert!(EngineConfig::new(20, 2, 144, 6).is_ok());
    }

    /// A10: amounts that would give a dust output in one of the transactions are rejected.
    /// O8: the forced close margin is the release safety margin unless it is set. It cannot be
    /// smaller than that margin, and it must leave room for a release.
    #[test]
    fn forced_close_margin_has_limits() {
        let config = EngineConfig::new(20, 10, 144, 6).expect("valid");
        assert_eq!(config.forced_close_margin(), 6);
        assert_eq!(EngineConfig::default().forced_close_margin(), 6);

        let earlier = config.with_forced_close_margin(12).expect("valid");
        assert_eq!(earlier.forced_close_margin(), 12);
        assert_eq!(earlier.release_safety_margin(), 6);
        assert!(
            earlier
                .to_string()
                .contains("forced close margin 12 blocks")
        );
        assert!(config.with_forced_close_margin(6).is_ok());
        assert!(config.with_forced_close_margin(16).is_ok());

        assert_invalid_config(
            config.with_forced_close_margin(5),
            "smaller than the release safety margin of 6 blocks",
        );
        assert_invalid_config(
            config.with_forced_close_margin(17),
            "not smaller than 17 blocks",
        );
    }

    #[test]
    fn a10_amounts_below_the_minimum_are_rejected() {
        // The swap transaction pays 1110 sat of fee at this fee rate, and its output must not be
        // below 330 sat.
        let params = TradeParams {
            sellers_security_deposit: Amount::from_sat(1_439),
            ..valid_params()
        };
        assert_invalid(&params, "fee of the swap transaction");
        let params = TradeParams {
            sellers_security_deposit: Amount::from_sat(1_440),
            ..valid_params()
        };
        assert_eq!(params.validate(Network::Regtest), Ok(()));

        let params = TradeParams {
            trade_amount: Amount::ZERO,
            buyers_security_deposit: Amount::from_sat(329),
            ..valid_params()
        };
        assert_invalid(&params, "payout of the deposit transaction is dust");
    }

    /// The trade fee receiver must have an address of the wallet's network and an amount above
    /// the dust limit.
    #[test]
    fn invalid_trade_fee_receiver_is_rejected() {
        let receiver = |address: &str, amount| {
            Some(Receiver {
                address: address.parse().unwrap(),
                amount: Amount::from_sat(amount),
            })
        };
        let regtest_address = "bcrt1qwk6p86mzqmstcsg99qlu2mhsp3766u68jktv6k";

        let params = TradeParams {
            trade_fee_receiver: receiver(regtest_address, 5_000),
            ..valid_params()
        };
        assert_eq!(params.validate(Network::Regtest), Ok(()));
        let error = params
            .validate(Network::Bitcoin)
            .expect_err("address of another network");
        assert!(
            error.0.contains("address of the trade fee receiver"),
            "unexpected error: {error}"
        );
        let params = TradeParams {
            trade_fee_receiver: receiver(regtest_address, 100),
            ..valid_params()
        };
        assert_invalid(&params, "is dust");
    }
}
