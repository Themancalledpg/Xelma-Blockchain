//! Core contract implementation for the XLM Price Prediction Market.

use soroban_sdk::{contract, contractimpl, panic_with_error, symbol_short, Address, Env, Map, Vec};

use crate::errors::ContractError;
use crate::types::{
    BetSide, DataKey, OraclePayload, PrecisionPrediction, Round, RoundMode, UserPosition, UserStats,
};

const DEFAULT_BET_WINDOW_LEDGERS: u32 = 6;
const DEFAULT_RUN_WINDOW_LEDGERS: u32 = 12;
const MAX_BET_WINDOW_LEDGERS: u32 = 1_440;
const MAX_RUN_WINDOW_LEDGERS: u32 = 2_880;

#[contract]
pub struct VirtualTokenContract;

#[contractimpl]
impl VirtualTokenContract {
    /// Initializes the contract with admin and oracle addresses (one-time only)
    pub fn initialize(env: Env, admin: Address, oracle: Address) -> Result<(), ContractError> {
        admin.require_auth();

        if admin == oracle {
            return Err(ContractError::AdminIsOracle);
        }

        if env.storage().persistent().has(&DataKey::Admin) {
            return Err(ContractError::AlreadyInitialized);
        }

        env.storage().persistent().set(&DataKey::Admin, &admin);
        env.storage().persistent().set(&DataKey::Oracle, &oracle);
        env.storage().persistent().set(&DataKey::Paused, &false);

        // Set default window values
        env.storage()
            .persistent()
            .set(&DataKey::BetWindowLedgers, &DEFAULT_BET_WINDOW_LEDGERS);
        env.storage()
            .persistent()
            .set(&DataKey::RunWindowLedgers, &DEFAULT_RUN_WINDOW_LEDGERS);

        Ok(())
    }

    /// Returns whether the contract is currently paused
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Pauses the contract for emergency recovery (admin only)
    pub fn pause_contract(env: Env) -> Result<(), ContractError> {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .ok_or(ContractError::AdminNotSet)?;

        admin.require_auth();
        env.storage().persistent().set(&DataKey::Paused, &true);

        Ok(())
    }

    /// Unpauses the contract after recovery (admin only)
    pub fn unpause_contract(env: Env) -> Result<(), ContractError> {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .ok_or(ContractError::AdminNotSet)?;

        admin.require_auth();
        env.storage().persistent().set(&DataKey::Paused, &false);

        Ok(())
    }

    /// Creates a new prediction round (admin only)
    /// mode: 0 = Up/Down (default), 1 = Precision (Legends)
    pub fn create_round(
        env: Env,
        start_price: u128,
        mode: Option<u32>,
    ) -> Result<(), ContractError> {
        if start_price == 0 {
            return Err(ContractError::InvalidPrice);
        }

        // Default to Up/Down mode (0) if not specified
        let mode_value = mode.unwrap_or(0);

        // Validate mode is either 0 or 1
        if mode_value > 1 {
            return Err(ContractError::InvalidMode);
        }

        let round_mode = if mode_value == 0 {
            RoundMode::UpDown
        } else {
            RoundMode::Precision
        };

        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .ok_or(ContractError::AdminNotSet)?;

        admin.require_auth();
        Self::_ensure_not_paused(&env)?;

        // Prevent overwriting an already active round
        if env.storage().persistent().has(&DataKey::ActiveRound) {
            return Err(ContractError::RoundAlreadyActive);
        }

        // Get configured windows (with defaults)
        let bet_ledgers: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::BetWindowLedgers)
            .unwrap_or(DEFAULT_BET_WINDOW_LEDGERS);
        let run_ledgers: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::RunWindowLedgers)
            .unwrap_or(DEFAULT_RUN_WINDOW_LEDGERS);

        // Generate unique round ID
        let last_round_id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LastRoundId)
            .unwrap_or(0);
        let round_id = last_round_id
            .checked_add(1)
            .ok_or(ContractError::Overflow)?;
        env.storage()
            .persistent()
            .set(&DataKey::LastRoundId, &round_id);

        let start_ledger = env.ledger().sequence();
        let bet_end_ledger = start_ledger
            .checked_add(bet_ledgers)
            .ok_or(ContractError::Overflow)?;
        let end_ledger = start_ledger
            .checked_add(run_ledgers)
            .ok_or(ContractError::Overflow)?;

        let round = Round {
            round_id,
            price_start: start_price,
            start_ledger,
            bet_end_ledger,
            end_ledger,
            pool_up: 0,
            pool_down: 0,
            mode: round_mode.clone(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::ActiveRound, &round);

        // Emit round creation event with round ID and mode
        // Topic: ("round", "created")
        // Payload: (round_id: u64, start_price: u128, start_ledger: u32, bet_end_ledger: u32, end_ledger: u32, mode: u32)
        #[allow(deprecated)]
        env.events().publish(
            (symbol_short!("round"), symbol_short!("created")),
            (
                round_id,
                start_price,
                start_ledger,
                bet_end_ledger,
                end_ledger,
                mode_value,
            ),
        );

        Ok(())
    }

    /// Returns the currently active round, if any
    pub fn get_active_round(env: Env) -> Option<Round> {
        env.storage().persistent().get(&DataKey::ActiveRound)
    }

    /// Returns the ID of the last created round (0 if no rounds created yet)
    pub fn get_last_round_id(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::LastRoundId)
            .unwrap_or(0)
    }

    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Admin)
    }

    pub fn get_oracle(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Oracle)
    }

    /// Sets the betting and execution windows (admin only)
    /// bet_ledgers: Number of ledgers users can place bets
    /// run_ledgers: Total number of ledgers before round can be resolved
    pub fn set_windows(env: Env, bet_ledgers: u32, run_ledgers: u32) -> Result<(), ContractError> {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .ok_or(ContractError::AdminNotSet)?;

        admin.require_auth();
        Self::_ensure_not_paused(&env)?;

        // Validate both values are positive
        if bet_ledgers == 0 || run_ledgers == 0 {
            return Err(ContractError::InvalidDuration);
        }

        // Reject out-of-range values before applying cross-field checks.
        if bet_ledgers > MAX_BET_WINDOW_LEDGERS || run_ledgers > MAX_RUN_WINDOW_LEDGERS {
            return Err(ContractError::WindowOutOfRange);
        }

        // Validate bet window closes before run window ends
        if bet_ledgers >= run_ledgers {
            return Err(ContractError::InvalidDuration);
        }

        env.storage()
            .persistent()
            .set(&DataKey::BetWindowLedgers, &bet_ledgers);
        env.storage()
            .persistent()
            .set(&DataKey::RunWindowLedgers, &run_ledgers);

        // Emit windows update event
        // Topic: ("windows", "updated")
        // Payload: (bet_window_ledgers: u32, run_window_ledgers: u32)
        #[allow(deprecated)]
        env.events().publish(
            (symbol_short!("windows"), symbol_short!("updated")),
            (bet_ledgers, run_ledgers),
        );

        Ok(())
    }

    /// Returns user statistics (wins, losses, streaks)
    pub fn get_user_stats(env: Env, user: Address) -> UserStats {
        let key = DataKey::UserStats(user);
        env.storage().persistent().get(&key).unwrap_or(UserStats {
            total_wins: 0,
            total_losses: 0,
            current_streak: 0,
            best_streak: 0,
        })
    }

    /// Returns user's claimable winnings
    pub fn get_pending_winnings(env: Env, user: Address) -> i128 {
        let key = DataKey::PendingWinnings(user);
        env.storage().persistent().get(&key).unwrap_or(0)
    }

    /// Places a bet on the active round (Up/Down mode only)
    pub fn place_bet(
        env: Env,
        user: Address,
        amount: i128,
        side: BetSide,
    ) -> Result<(), ContractError> {
        user.require_auth();
        Self::_ensure_not_paused(&env)?;

        if amount <= 0 {
            return Err(ContractError::InvalidBetAmount);
        }

        let mut round: Round = env
            .storage()
            .persistent()
            .get(&DataKey::ActiveRound)
            .ok_or(ContractError::NoActiveRound)?;

        // Verify round is in Up/Down mode
        if round.mode != RoundMode::UpDown {
            return Err(ContractError::WrongModeForPrediction);
        }

        let current_ledger = env.ledger().sequence();
        if current_ledger >= round.bet_end_ledger {
            return Err(ContractError::RoundEnded);
        }

        let user_balance = Self::balance(env.clone(), user.clone());
        if user_balance < amount {
            return Err(ContractError::InsufficientBalance);
        }

        // Use UpDownPositions storage for Up/Down mode
        let mut positions: Map<Address, UserPosition> = env
            .storage()
            .persistent()
            .get(&DataKey::UpDownPositions)
            .unwrap_or(Map::new(&env));

        if positions.contains_key(user.clone()) {
            return Err(ContractError::AlreadyBet);
        }

        let new_balance = user_balance
            .checked_sub(amount)
            .ok_or(ContractError::Overflow)?;
        Self::_set_balance(&env, user.clone(), new_balance);

        let position = UserPosition {
            amount,
            side: side.clone(),
        };
        positions.set(user.clone(), position);

        match side {
            BetSide::Up => {
                round.pool_up = round
                    .pool_up
                    .checked_add(amount)
                    .ok_or(ContractError::Overflow)?;
            }
            BetSide::Down => {
                round.pool_down = round
                    .pool_down
                    .checked_add(amount)
                    .ok_or(ContractError::Overflow)?;
            }
        }

        env.storage()
            .persistent()
            .set(&DataKey::UpDownPositions, &positions);
        env.storage()
            .persistent()
            .set(&DataKey::ActiveRound, &round);

        // Emit bet placed event
        // Topic: ("bet", "placed")
        // Payload: (user: Address, round_id: u64, amount: i128, side: u32 where 0=Up, 1=Down)
        let side_value: u32 = match side {
            BetSide::Up => 0,
            BetSide::Down => 1,
        };
        #[allow(deprecated)]
        env.events().publish(
            (symbol_short!("bet"), symbol_short!("placed")),
            (user, round.round_id, amount, side_value),
        );

        Ok(())
    }

    /// Places a precision prediction on the active round (Precision/Legends mode only)
    /// predicted_price: price scaled to 4 decimals (e.g., 0.2297 → 2297)
    pub fn place_precision_prediction(
        env: Env,
        user: Address,
        amount: i128,
        predicted_price: u128,
    ) -> Result<(), ContractError> {
        user.require_auth();
        Self::_ensure_not_paused(&env)?;

        if amount <= 0 {
            return Err(ContractError::InvalidBetAmount);
        }

        // Validate price scale (must be 4 decimal places, max value 9999 for 0.9999)
        // Reasonable max: 99999999 (9999.9999 XLM)
        if predicted_price > 99_999_999 {
            return Err(ContractError::InvalidPriceScale);
        }

        let round: Round = env
            .storage()
            .persistent()
            .get(&DataKey::ActiveRound)
            .ok_or(ContractError::NoActiveRound)?;

        // Verify round is in Precision mode
        if round.mode != RoundMode::Precision {
            return Err(ContractError::WrongModeForPrediction);
        }

        let current_ledger = env.ledger().sequence();
        if current_ledger >= round.bet_end_ledger {
            return Err(ContractError::RoundEnded);
        }

        let user_balance = Self::balance(env.clone(), user.clone());
        if user_balance < amount {
            return Err(ContractError::InsufficientBalance);
        }

        // Check if user already has a prediction in this round
        let mut predictions: Map<Address, PrecisionPrediction> = env
            .storage()
            .persistent()
            .get(&DataKey::PrecisionPositions)
            .unwrap_or(Map::new(&env));

        if predictions.contains_key(user.clone()) {
            return Err(ContractError::AlreadyBet);
        }

        // Deduct balance
        let new_balance = user_balance
            .checked_sub(amount)
            .ok_or(ContractError::Overflow)?;
        Self::_set_balance(&env, user.clone(), new_balance);

        // Store prediction
        let prediction = PrecisionPrediction {
            user: user.clone(),
            predicted_price,
            amount,
        };
        predictions.set(user.clone(), prediction);

        env.storage()
            .persistent()
            .set(&DataKey::PrecisionPositions, &predictions);

        // Emit event for precision prediction
        // Topic: ("predict", "price")
        // Payload: (user: Address, round_id: u64, predicted_price: u128, amount: i128)
        #[allow(deprecated)]
        env.events().publish(
            (symbol_short!("predict"), symbol_short!("price")),
            (user, round.round_id, predicted_price, amount),
        );

        Ok(())
    }

    /// Alias for place_precision_prediction - allows users to submit exact price predictions
    /// guessed_price: price scaled to 4 decimals (e.g., 0.2297 → 2297)
    pub fn predict_price(
        env: Env,
        user: Address,
        guessed_price: u128,
        amount: i128,
    ) -> Result<(), ContractError> {
        Self::place_precision_prediction(env, user, amount, guessed_price)
    }

    /// Returns user's position in the current round (Up/Down mode)
    pub fn get_user_position(env: Env, user: Address) -> Option<UserPosition> {
        let positions: Map<Address, UserPosition> = env
            .storage()
            .persistent()
            .get(&DataKey::UpDownPositions)
            .unwrap_or(Map::new(&env));

        if let Some(position) = positions.get(user.clone()) {
            return Some(position);
        }

        // Legacy read-only fallback to aid one-time migration checks.
        let legacy_positions: Map<Address, UserPosition> = env
            .storage()
            .persistent()
            .get(&DataKey::Positions)
            .unwrap_or(Map::new(&env));
        legacy_positions.get(user)
    }

    /// Returns user's precision prediction in the current round (Precision mode)
    pub fn get_user_precision_prediction(env: Env, user: Address) -> Option<PrecisionPrediction> {
        let predictions: Map<Address, PrecisionPrediction> = env
            .storage()
            .persistent()
            .get(&DataKey::PrecisionPositions)
            .unwrap_or(Map::new(&env));

        predictions.get(user)
    }

    /// Returns all precision predictions for the current round
    pub fn get_precision_predictions(env: Env) -> Vec<PrecisionPrediction> {
        let predictions: Map<Address, PrecisionPrediction> = env
            .storage()
            .persistent()
            .get(&DataKey::PrecisionPositions)
            .unwrap_or(Map::new(&env));
        predictions.values()
    }

    /// Returns all Up/Down positions for the current round
    pub fn get_updown_positions(env: Env) -> Map<Address, UserPosition> {
        env.storage()
            .persistent()
            .get(&DataKey::UpDownPositions)
            .unwrap_or(Map::new(&env))
    }

    /// Resolves the round with oracle payload (oracle only)
    /// Mode 0 (Up/Down): Winners split losers' pool proportionally; ties get refunds
    /// Mode 1 (Precision/Legends): Closest guess wins full pot; ties split evenly
    pub fn resolve_round(env: Env, payload: OraclePayload) -> Result<(), ContractError> {
        if payload.price == 0 {
            return Err(ContractError::InvalidPrice);
        }

        let oracle: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Oracle)
            .ok_or(ContractError::OracleNotSet)?;

        oracle.require_auth();
        Self::_ensure_not_paused(&env)?;

        let round: Round = env
            .storage()
            .persistent()
            .get(&DataKey::ActiveRound)
            .ok_or(ContractError::NoActiveRound)?;

        // Verify round ID matches to prevent cross-round replays
        if payload.round_id != round.start_ledger {
            return Err(ContractError::InvalidOracleRound);
        }

        // Verify data freshness (max 300 seconds / 5 minutes old)
        let current_time = env.ledger().timestamp();

        // Reject future timestamps to prevent time-skew manipulation
        if payload.timestamp > current_time {
            return Err(ContractError::FutureOracleData);
        }

        if current_time > payload.timestamp + 300 {
            return Err(ContractError::StaleOracleData);
        }

        // Verify round has reached end_ledger
        let current_ledger = env.ledger().sequence();
        if current_ledger < round.end_ledger {
            return Err(ContractError::RoundNotEnded);
        }

        // Store round ID before cleaning up
        let round_id = round.round_id;

        // Branch based on round mode
        match round.mode {
            RoundMode::UpDown => {
                Self::_resolve_updown_mode(&env, &round, payload.price)?;
            }
            RoundMode::Precision => {
                Self::_resolve_precision_mode(&env, payload.price)?;
            }
        }

        // Clean up storage
        env.storage().persistent().remove(&DataKey::ActiveRound);
        env.storage().persistent().remove(&DataKey::Positions);
        env.storage().persistent().remove(&DataKey::UpDownPositions);
        env.storage()
            .persistent()
            .remove(&DataKey::PrecisionPositions);

        // Emit resolution event with round ID, price, and mode
        // Topic: ("round", "resolved")
        // Payload: (round_id: u64, final_price: u128, mode: u32 where 0=UpDown, 1=Precision)
        let mode_value: u32 = match round.mode {
            RoundMode::UpDown => 0,
            RoundMode::Precision => 1,
        };
        #[allow(deprecated)]
        env.events().publish(
            (symbol_short!("round"), symbol_short!("resolved")),
            (round_id, payload.price, mode_value),
        );

        Ok(())
    }

    /// Resolves Up/Down mode round
    fn _resolve_updown_mode(
        env: &Env,
        round: &Round,
        final_price: u128,
    ) -> Result<(), ContractError> {
        let positions: Map<Address, UserPosition> = env
            .storage()
            .persistent()
            .get(&DataKey::UpDownPositions)
            .unwrap_or(Map::new(env));

        let price_went_up = final_price > round.price_start;
        let price_went_down = final_price < round.price_start;
        let price_unchanged = final_price == round.price_start;

        if price_unchanged {
            Self::_record_refunds(env, positions)?;
        } else if price_went_up {
            Self::_record_winnings(env, positions, BetSide::Up, round.pool_up, round.pool_down)?;
        } else if price_went_down {
            Self::_record_winnings(
                env,
                positions,
                BetSide::Down,
                round.pool_down,
                round.pool_up,
            )?;
        }

        Ok(())
    }

    /// Resolves Precision/Legends mode round
    /// Awards full pot to closest guess(es); ties split evenly
    fn _resolve_precision_mode(env: &Env, final_price: u128) -> Result<(), ContractError> {
        let predictions_map: Map<Address, PrecisionPrediction> = env
            .storage()
            .persistent()
            .get(&DataKey::PrecisionPositions)
            .unwrap_or(Map::new(env));
        let predictions = predictions_map.values();

        // If no predictions, nothing to resolve
        if predictions.is_empty() {
            return Ok(());
        }

        // Find minimum difference and collect all winners
        let mut min_diff: Option<u128> = None;
        let mut winners: Vec<PrecisionPrediction> = Vec::new(env);

        for i in 0..predictions.len() {
            if let Some(pred) = predictions.get(i) {
                // Calculate absolute difference using checked arithmetic
                let diff = if pred.predicted_price >= final_price {
                    pred.predicted_price
                        .checked_sub(final_price)
                        .ok_or(ContractError::Overflow)?
                } else {
                    final_price
                        .checked_sub(pred.predicted_price)
                        .ok_or(ContractError::Overflow)?
                };

                match min_diff {
                    None => {
                        // First prediction
                        min_diff = Some(diff);
                        winners.push_back(pred.clone());
                    }
                    Some(current_min) => {
                        if diff < current_min {
                            // New winner found, clear previous winners
                            min_diff = Some(diff);
                            winners = Vec::new(env);
                            winners.push_back(pred.clone());
                        } else if diff == current_min {
                            // Tie - add to winners
                            winners.push_back(pred.clone());
                        }
                    }
                }
            }
        }

        // Calculate total pot
        let mut total_pot: i128 = 0;
        for i in 0..predictions.len() {
            if let Some(pred) = predictions.get(i) {
                total_pot = total_pot
                    .checked_add(pred.amount)
                    .ok_or(ContractError::Overflow)?;
            }
        }

        // Distribute winnings to winner(s)
        if !winners.is_empty() && total_pot > 0 {
            let winner_count = winners.len() as i128;
            let payout_per_winner = total_pot / winner_count;
            let remainder = total_pot % winner_count;

            // Award to each winner
            for i in 0..winners.len() {
                if let Some(winner) = winners.get(i) {
                    let key = DataKey::PendingWinnings(winner.user.clone());
                    let existing_pending: i128 = env.storage().persistent().get(&key).unwrap_or(0);

                    // First winner gets the remainder (if any)
                    let payout = if i == 0 {
                        payout_per_winner
                            .checked_add(remainder)
                            .ok_or(ContractError::Overflow)?
                    } else {
                        payout_per_winner
                    };

                    let new_pending = existing_pending
                        .checked_add(payout)
                        .ok_or(ContractError::Overflow)?;
                    env.storage().persistent().set(&key, &new_pending);

                    Self::_update_stats_win(env, winner.user.clone())?;
                }
            }

            // Update stats for losers
            for i in 0..predictions.len() {
                if let Some(pred) = predictions.get(i) {
                    let is_winner = winners.iter().any(|w| w.user == pred.user);
                    if !is_winner {
                        Self::_update_stats_loss(env, pred.user.clone())?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Claims pending winnings and adds to balance
    pub fn claim_winnings(env: Env, user: Address) -> Result<i128, ContractError> {
        user.require_auth();
        Self::_ensure_not_paused(&env)?;

        let key = DataKey::PendingWinnings(user.clone());
        let pending: i128 = env.storage().persistent().get(&key).unwrap_or(0);

        if pending == 0 {
            return Ok(0);
        }

        let current_balance = Self::balance(env.clone(), user.clone());
        let new_balance = current_balance
            .checked_add(pending)
            .ok_or(ContractError::Overflow)?;
        Self::_set_balance(&env, user.clone(), new_balance);

        env.storage().persistent().remove(&key);

        // Emit claim event
        // Topic: ("claim", "winnings")
        // Payload: (user: Address, amount: i128)
        #[allow(deprecated)]
        env.events().publish(
            (symbol_short!("claim"), symbol_short!("winnings")),
            (user, pending),
        );

        Ok(pending)
    }

    /// Records refunds when price unchanged
    fn _record_refunds(
        env: &Env,
        positions: Map<Address, UserPosition>,
    ) -> Result<(), ContractError> {
        let keys: Vec<Address> = positions.keys();

        for i in 0..keys.len() {
            if let Some(user) = keys.get(i) {
                if let Some(position) = positions.get(user.clone()) {
                    let key = DataKey::PendingWinnings(user.clone());
                    let existing_pending: i128 = env.storage().persistent().get(&key).unwrap_or(0);
                    let new_pending = existing_pending
                        .checked_add(position.amount)
                        .ok_or(ContractError::Overflow)?;
                    env.storage().persistent().set(&key, &new_pending);
                }
            }
        }

        Ok(())
    }

    /// Records winnings for winning side
    /// Formula: payout = bet + (bet / winning_pool) * losing_pool
    fn _record_winnings(
        env: &Env,
        positions: Map<Address, UserPosition>,
        winning_side: BetSide,
        winning_pool: i128,
        losing_pool: i128,
    ) -> Result<(), ContractError> {
        if winning_pool == 0 {
            return Ok(());
        }

        let keys: Vec<Address> = positions.keys();

        for i in 0..keys.len() {
            if let Some(user) = keys.get(i) {
                if let Some(position) = positions.get(user.clone()) {
                    if position.side == winning_side {
                        let share_numerator = position
                            .amount
                            .checked_mul(losing_pool)
                            .ok_or(ContractError::Overflow)?;
                        let share = share_numerator / winning_pool;
                        let payout = position
                            .amount
                            .checked_add(share)
                            .ok_or(ContractError::Overflow)?;

                        let key = DataKey::PendingWinnings(user.clone());
                        let existing_pending: i128 =
                            env.storage().persistent().get(&key).unwrap_or(0);
                        let new_pending = existing_pending
                            .checked_add(payout)
                            .ok_or(ContractError::Overflow)?;
                        env.storage().persistent().set(&key, &new_pending);

                        Self::_update_stats_win(env, user)?;
                    } else {
                        Self::_update_stats_loss(env, user)?;
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn _update_stats_win(env: &Env, user: Address) -> Result<(), ContractError> {
        let key = DataKey::UserStats(user);
        let mut stats: UserStats = env.storage().persistent().get(&key).unwrap_or(UserStats {
            total_wins: 0,
            total_losses: 0,
            current_streak: 0,
            best_streak: 0,
        });

        stats.total_wins = stats
            .total_wins
            .checked_add(1)
            .ok_or(ContractError::Overflow)?;
        stats.current_streak = stats
            .current_streak
            .checked_add(1)
            .ok_or(ContractError::Overflow)?;

        if stats.current_streak > stats.best_streak {
            stats.best_streak = stats.current_streak;
        }

        env.storage().persistent().set(&key, &stats);
        Ok(())
    }

    pub(crate) fn _update_stats_loss(env: &Env, user: Address) -> Result<(), ContractError> {
        let key = DataKey::UserStats(user);
        let mut stats: UserStats = env.storage().persistent().get(&key).unwrap_or(UserStats {
            total_wins: 0,
            total_losses: 0,
            current_streak: 0,
            best_streak: 0,
        });

        stats.total_losses = stats
            .total_losses
            .checked_add(1)
            .ok_or(ContractError::Overflow)?;
        stats.current_streak = 0;

        env.storage().persistent().set(&key, &stats);
        Ok(())
    }

    /// Mints 1000 vXLM for new users (one-time only)
    pub fn mint_initial(env: Env, user: Address) -> i128 {
        user.require_auth();
        if Self::is_paused(env.clone()) {
            panic_with_error!(&env, ContractError::ContractPaused);
        }

        let key = DataKey::Balance(user.clone());

        if let Some(existing_balance) = env.storage().persistent().get(&key) {
            return existing_balance;
        }

        let initial_amount: i128 = 1000_0000000;
        env.storage().persistent().set(&key, &initial_amount);

        // Emit mint event
        // Topic: ("mint", "initial")
        // Payload: (user: Address, amount: i128)
        #[allow(deprecated)]
        env.events().publish(
            (symbol_short!("mint"), symbol_short!("initial")),
            (user, initial_amount),
        );

        initial_amount
    }

    /// Returns user's vXLM balance
    pub fn balance(env: Env, user: Address) -> i128 {
        let key = DataKey::Balance(user);
        env.storage().persistent().get(&key).unwrap_or(0)
    }

    pub(crate) fn _set_balance(env: &Env, user: Address, amount: i128) {
        let key = DataKey::Balance(user);
        env.storage().persistent().set(&key, &amount);
    }

    fn _ensure_not_paused(env: &Env) -> Result<(), ContractError> {
        if Self::is_paused(env.clone()) {
            return Err(ContractError::ContractPaused);
        }

        Ok(())
    }
}
