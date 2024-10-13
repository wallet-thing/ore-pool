use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use drillx::Solution;
use ore_api::{
    consts::{BUS_ADDRESSES, BUS_COUNT},
    state::Bus,
};
use ore_pool_types::Challenge;
use rand::Rng;
use sha3::{Digest, Sha3_256};
use solana_sdk::{pubkey::Pubkey, signer::Signer};
use steel::AccountDeserialize;

use crate::{
    database,
    error::Error,
    operator::{Operator, BUFFER_OPERATOR},
    tx,
    webhook::{self, Rewards},
};

/// The client submits slightly earlier
/// than the operator's cutoff time to create a "submission window".
pub const BUFFER_CLIENT: u64 = 2 + BUFFER_OPERATOR;

/// Aggregates contributions from the pool members.
pub struct Aggregator {
    /// The current challenge.
    pub challenge: Challenge,

    /// The rewards channel receiver.
    pub rewards_rx: tokio::sync::mpsc::Receiver<webhook::Rewards>,

    /// The set of contributions aggregated for the current challenge.
    pub contributions: HashSet<Contribution>,

    /// The total difficulty score of all the contributions aggregated so far.
    pub total_score: u64,

    /// The best solution submitted.
    pub winner: Option<Winner>,

    /// The number of workers that have been approved for the current challenge.
    pub num_members: u64,

    /// The map of stake contributors for attribution.
    pub stake: Stakers,
}

pub type BoostMint = Pubkey;
pub type StakerBalances = HashMap<Pubkey, u64>;
pub type Stakers = HashMap<BoostMint, StakerBalances>;

// Best hash to be submitted for the current challenge.
#[derive(Clone, Copy, Debug)]
pub struct Winner {
    // The winning solution.
    pub solution: Solution,

    // The current largest difficulty.
    pub difficulty: u32,
}

/// A recorded contribution from a particular member of the pool.
#[derive(Clone, Copy, Debug)]
pub struct Contribution {
    /// The member who submitted this solution.
    pub member: Pubkey,

    /// The difficulty score of the solution.
    pub score: u64,

    /// The drillx solution submitted representing the member's best hash.
    pub solution: Solution,
}

impl PartialEq for Contribution {
    fn eq(&self, other: &Self) -> bool {
        self.member == other.member
    }
}

impl Eq for Contribution {}

impl Hash for Contribution {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.member.hash(state);
    }
}

pub async fn process_contributions(
    aggregator: &tokio::sync::RwLock<Aggregator>,
    operator: &Operator,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Contribution>,
) -> Result<(), Error> {
    // outer loop for new challenges
    loop {
        let timer = tokio::time::Instant::now();
        let cutoff_time = {
            let read = aggregator.read().await;
            read.challenge.cutoff_time
        };
        let mut remaining_time = cutoff_time.saturating_sub(timer.elapsed().as_secs());
        // inner loop to process contributions until cutoff time
        while remaining_time > 0 {
            // race the next contribution against remaining time
            match tokio::time::timeout(tokio::time::Duration::from_secs(remaining_time), rx.recv())
                .await
            {
                Ok(Some(contribution)) => {
                    {
                        let mut aggregator = aggregator.write().await;
                        aggregator.insert(&contribution);
                    }
                    // recalculate the remaining time after processing the contribution
                    remaining_time = cutoff_time.saturating_sub(timer.elapsed().as_secs());
                }
                Ok(None) => {
                    // if the receiver is closed, exit server
                    return Err(Error::Internal("contribution channel closed".to_string()));
                }
                Err(_) => {
                    // timeout expired, meaning cutoff time has been reached
                    break;
                }
            }
        }
        // at this point, the cutoff time has been reached
        let total_score = {
            let read = aggregator.read().await;
            read.total_score
        };
        if total_score > 0 {
            // submit if contributions exist
            let mut aggregator = aggregator.write().await;
            if let Err(err) = aggregator.submit_and_reset(operator).await {
                log::error!("{:?}", err);
            }
        } else {
            // no contributions yet, wait for the first one to submit
            if let Some(contribution) = rx.recv().await {
                let mut aggregator = aggregator.write().await;
                aggregator.insert(&contribution);
                if let Err(err) = aggregator.submit_and_reset(operator).await {
                    log::error!("{:?}", err);
                }
            }
        }
    }
}

impl Aggregator {
    pub async fn new(
        operator: &Operator,
        rewards_rx: tokio::sync::mpsc::Receiver<webhook::Rewards>,
    ) -> Result<Self, Error> {
        // fetch accounts
        let pool = operator.get_pool().await?;
        let proof = operator.get_proof().await?;
        log::info!("proof: {:?}", proof);
        let cutoff_time = operator.get_cutoff(&proof).await?;
        let min_difficulty = operator.min_difficulty().await?;
        let challenge = Challenge {
            challenge: proof.challenge,
            lash_hash_at: pool.last_hash_at,
            min_difficulty,
            cutoff_time,
        };
        // fetch staker balances
        let mut stake: Stakers = HashMap::new();
        let boost_acounts = operator.boost_accounts.iter();
        for ba in boost_acounts {
            let stakers = operator.get_stakers_onchain(&ba.mint).await?;
            stake.insert(ba.mint, stakers);
        }
        // build self
        let aggregator = Aggregator {
            challenge,
            rewards_rx,
            contributions: HashSet::new(),
            total_score: 0,
            winner: None,
            num_members: pool.last_total_members,
            stake,
        };
        Ok(aggregator)
    }

    fn insert(&mut self, contribution: &Contribution) {
        match self.contributions.insert(*contribution) {
            true => {
                let difficulty = contribution.solution.to_hash().difficulty();
                let contender = Winner {
                    solution: contribution.solution,
                    difficulty,
                };
                self.total_score += contribution.score;
                match self.winner {
                    Some(winner) => {
                        if difficulty > winner.difficulty {
                            self.winner = Some(contender);
                        }
                    }
                    None => self.winner = Some(contender),
                }
            }
            false => {
                log::error!("already received contribution: {:?}", contribution.member);
            }
        }
    }

    // TODO Publish block to S3
    async fn submit_and_reset(&mut self, operator: &Operator) -> Result<(), Error> {
        // check if reset is needed
        // this may happen if a solution is landed on chain
        // but a subsequent application error is thrown before resetting
        if self.check_for_reset(operator).await? {
            log::error!("irregular reset");
            self.reset(operator).await?;
        };
        // prepare best solution and attestation of hash-power
        let winner = self.winner()?;
        log::info!("winner: {:?}", winner);
        let best_solution = winner.solution;
        let attestation = self.attestation();
        // derive accounts for instructions
        let authority = &operator.keypair.pubkey();
        let (pool_pda, _) = ore_pool_api::state::pool_pda(*authority);
        let (pool_proof_pda, _) = ore_pool_api::state::pool_proof_pda(pool_pda);
        let bus = self.find_bus(operator).await?;
        // build instructions
        let auth_ix = ore_api::sdk::auth(pool_proof_pda);
        let submit_ix = ore_pool_api::sdk::submit(
            operator.keypair.pubkey(),
            best_solution,
            attestation,
            bus,
            operator.get_boost_mine_accounts(),
        );
        let rpc_client = &operator.rpc_client;
        let sig = tx::submit::submit_and_confirm_instructions(
            &operator.keypair,
            rpc_client,
            &[auth_ix, submit_ix],
            1_500_000,
            500_000,
        )
        .await?;
        log::info!("{:?}", sig);
        // listen for rewards
        let rewards_rx = &mut self.rewards_rx;
        let rewards = rewards_rx
            .recv()
            .await
            .ok_or(Error::Internal("rewards channel closed".to_string()))?;
        // compute attributions for miners
        log::info!("reward: {:?}", rewards);
        log::info!("// miner ////////////////////////");
        let rewards_distribution = self.rewards_distribution(
            pool_pda,
            &rewards,
            operator.operator_commission,
            operator.staker_commission,
        );
        log::info!("// staker ////////////////////////");
        // compute attributions for stakers
        let rewards_distribution_boost_1 =
            self.rewards_distribution_boost(pool_pda, rewards.boost_1, operator.staker_commission)?;
        let rewards_distribution_boost_2 =
            self.rewards_distribution_boost(pool_pda, rewards.boost_2, operator.staker_commission)?;
        let rewards_distribution_boost_3 =
            self.rewards_distribution_boost(pool_pda, rewards.boost_3, operator.staker_commission)?;
        log::info!("// operator ////////////////////////");
        // compute attribution for operator
        let rewards_distribution_operator = self.rewards_distribution_operator(
            pool_pda,
            operator.keypair.pubkey(),
            &rewards,
            operator.operator_commission,
        );
        // write rewards to db
        let mut db_client = operator.db_client.get().await?;
        tokio::spawn(async move {
            database::write_member_total_balances(&mut db_client, rewards_distribution).await?;
            database::write_member_total_balances(&mut db_client, rewards_distribution_boost_1)
                .await?;
            database::write_member_total_balances(&mut db_client, rewards_distribution_boost_2)
                .await?;
            database::write_member_total_balances(&mut db_client, rewards_distribution_boost_3)
                .await?;
            database::write_member_total_balances(
                &mut db_client,
                vec![rewards_distribution_operator],
            )
            .await
        });
        // reset
        self.reset(operator).await?;
        Ok(())
    }

    fn rewards_distribution(
        &self,
        pool: Pubkey,
        rewards: &Rewards,
        operator_commission: u64,
        staker_commission: u64,
    ) -> Vec<(String, u64)> {
        // compute denominator
        let denominator = self.total_score as u128;
        log::info!("base reward denominator: {}", denominator);
        // compute miner split
        let miner_commission = 100 - operator_commission;
        log::info!("miner commission: {}", miner_commission);
        let miner_rewards = (rewards.base * miner_commission / 100) as u128;
        log::info!("miner rewards as commission for miners: {}", miner_rewards);
        // compute miner split from stake rewards
        let miner_rewards_from_stake_1 = Self::split_stake_rewards_for_miners(
            rewards.boost_1,
            operator_commission,
            staker_commission,
        );
        let miner_rewards_from_stake_2 = Self::split_stake_rewards_for_miners(
            rewards.boost_2,
            operator_commission,
            staker_commission,
        );
        let miner_rewards_from_stake_3 = Self::split_stake_rewards_for_miners(
            rewards.boost_3,
            operator_commission,
            staker_commission,
        );
        let total_rewards = miner_rewards
            + miner_rewards_from_stake_1
            + miner_rewards_from_stake_2
            + miner_rewards_from_stake_3;
        log::info!("total rewards as commission for miners: {}", total_rewards);
        let contributions = self.contributions.iter();
        contributions
            .map(|c| {
                log::info!("raw base reward score: {}", c.score);
                let score = (c.score as u128).saturating_mul(total_rewards);
                let score = score.checked_div(denominator).unwrap_or(0);
                log::info!("attributed base reward score: {}", score);
                let (member_pda, _) = ore_pool_api::state::member_pda(c.member, pool);
                (member_pda.to_string(), score as u64)
            })
            .collect()
    }

    fn split_stake_rewards_for_miners(
        boost_event: Option<ore_api::event::BoostEvent>,
        operator_commission: u64,
        staker_commission: u64,
    ) -> u128 {
        let miner_rewards_from_stake: u128 = match boost_event {
            Some(boost_event) => {
                log::info!("{:?}", boost_event);
                let miner_commission_for_stake: u128 =
                    (100 - operator_commission - staker_commission) as u128;
                log::info!("miner commission for stake: {}", miner_commission_for_stake);
                let stake_rewards = boost_event.reward as u128;
                stake_rewards * miner_commission_for_stake / 100
            }
            None => 0,
        };
        log::info!(
            "stake rewards as commission for miners: {}",
            miner_rewards_from_stake
        );
        miner_rewards_from_stake
    }

    fn rewards_distribution_boost(
        &self,
        pool: Pubkey,
        boost_event: Option<ore_api::event::BoostEvent>,
        staker_commission: u64,
    ) -> Result<Vec<(String, u64)>, Error> {
        match boost_event {
            None => Ok(vec![]),
            Some(boost_event) => {
                log::info!("{:?}", boost_event);
                let total_reward = boost_event.reward as u128;
                let staker_commission: u128 = staker_commission as u128;
                log::info!("staker commission: {}", staker_commission);
                let staker_rewards = total_reward * staker_commission / 100;
                log::info!("total rewards from stake: {}", total_reward);
                log::info!(
                    "total rewards as commission for stakers: {}",
                    staker_rewards
                );
                let stakers = self
                    .stake
                    .get(&boost_event.mint)
                    .ok_or(Error::Internal(format!(
                        "missing staker balances: {:?}",
                        boost_event.mint,
                    )))?;
                let denominator_iter = stakers.iter();
                let distribution_iter = stakers.iter();
                let denominator: u64 = denominator_iter.map(|(_, balance)| balance).sum();
                let denominator = denominator as u128;
                log::info!("staked reward denominator: {}", denominator);
                let res = distribution_iter
                    .map(|(stake_authority, balance)| {
                        log::info!("staked balance: {:?}", (stake_authority, balance));
                        let balance = *balance as u128;
                        let score = balance.saturating_mul(staker_rewards);
                        log::info!("scaled score from stake: {}", score);
                        let score = score.checked_div(denominator).unwrap_or(0);
                        log::info!("attributed reward from stake: {}", score);
                        let (member_pda, _) =
                            ore_pool_api::state::member_pda(*stake_authority, pool);
                        (member_pda.to_string(), score as u64)
                    })
                    .collect();
                Ok(res)
            }
        }
    }

    fn rewards_distribution_operator(
        &self,
        pool: Pubkey,
        pool_authority: Pubkey,
        rewards: &Rewards,
        operator_commission: u64,
    ) -> (String, u64) {
        // compute split from mine rewards
        let mine_rewards = rewards.base * operator_commission / 100;
        // compute split from stake rewads
        let mut stake_rewards = 0;
        if let Some(boost_event) = rewards.boost_1 {
            let r = boost_event.reward * operator_commission / 100;
            log::info!(
                "staker rewards for operator: {} from {:?}",
                r,
                boost_event.mint
            );
            stake_rewards += r;
        }
        if let Some(boost_event) = rewards.boost_2 {
            let r = boost_event.reward * operator_commission / 100;
            log::info!(
                "staker rewards for operator: {} from {:?}",
                r,
                boost_event.mint
            );
            stake_rewards += r;
        }
        if let Some(boost_event) = rewards.boost_3 {
            let r = boost_event.reward * operator_commission / 100;
            log::info!(
                "staker rewards for operator: {} from {:?}",
                r,
                boost_event.mint
            );
            stake_rewards += r;
        }
        log::info!("operator commission: {}", operator_commission);
        log::info!("mine rewards for operator: {}", mine_rewards);
        log::info!("stake rewards for operator: {}", stake_rewards);
        let total_rewards = mine_rewards + stake_rewards;
        log::info!("total rewards for operator: {}", total_rewards);
        let (member_pda, _) = ore_pool_api::state::member_pda(pool_authority, pool);
        (member_pda.to_string(), total_rewards)
    }

    async fn find_bus(&self, operator: &Operator) -> Result<Pubkey, Error> {
        // Fetch the bus with the largest balance
        let rpc_client = &operator.rpc_client;
        let accounts = rpc_client.get_multiple_accounts(&BUS_ADDRESSES).await?;
        let mut top_bus_balance: u64 = 0;
        let bus_index = rand::thread_rng().gen_range(0..BUS_COUNT);
        let mut top_bus = BUS_ADDRESSES[bus_index];
        for account in accounts.into_iter().flatten() {
            if let Ok(bus) = Bus::try_from_bytes(&account.data) {
                if bus.rewards.gt(&top_bus_balance) {
                    top_bus_balance = bus.rewards;
                    top_bus = BUS_ADDRESSES[bus.id as usize];
                }
            }
        }
        Ok(top_bus)
    }

    fn attestation(&self) -> [u8; 32] {
        let mut hasher = Sha3_256::new();
        let contributions = &self.contributions;
        let num_contributions = contributions.len();
        log::info!("num contributions: {}", num_contributions);
        for contribution in contributions.iter() {
            let hex_string: String =
                contribution
                    .solution
                    .d
                    .iter()
                    .fold(String::new(), |mut acc, byte| {
                        acc.push_str(&format!("{:02x}", byte));
                        acc
                    });
            let line = format!(
                "{} {} {}\n",
                contribution.member,
                hex_string,
                u64::from_le_bytes(contribution.solution.n)
            );
            hasher.update(&line);
        }
        let mut attestation: [u8; 32] = [0; 32];
        attestation.copy_from_slice(&hasher.finalize()[..]);
        attestation
    }

    async fn reset(&mut self, operator: &Operator) -> Result<(), Error> {
        self.update_challenge(operator).await?;
        let pool = operator.get_pool().await?;
        self.contributions = HashSet::new();
        self.total_score = 0;
        self.winner = None;
        self.num_members = pool.last_total_members;
        Ok(())
    }

    fn winner(&self) -> Result<Winner, Error> {
        self.winner
            .ok_or(Error::Internal("no solutions were submitted".to_string()))
    }

    async fn check_for_reset(&self, operator: &Operator) -> Result<bool, Error> {
        let last_hash_at = self.challenge.lash_hash_at;
        let pool = operator.get_pool().await?;
        let needs_reset = pool.last_hash_at != last_hash_at;
        Ok(needs_reset)
    }

    async fn update_challenge(&mut self, operator: &Operator) -> Result<(), Error> {
        let max_retries = 10;
        let mut retries = 0;
        let last_hash_at = self.challenge.lash_hash_at;
        loop {
            let proof = operator.get_proof().await?;
            let pool = operator.get_pool().await?;
            if pool.last_hash_at != last_hash_at {
                let cutoff_time = operator.get_cutoff(&proof).await?;
                let min_difficulty = operator.min_difficulty().await?;
                self.challenge.challenge = proof.challenge;
                self.challenge.lash_hash_at = pool.last_hash_at;
                self.challenge.min_difficulty = min_difficulty;
                self.challenge.cutoff_time = cutoff_time;
                return Ok(());
            } else {
                retries += 1;
                if retries == max_retries {
                    return Err(Error::Internal("failed to fetch new challenge".to_string()));
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }
    }
}
