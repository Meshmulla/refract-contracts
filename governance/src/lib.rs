use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Map, Symbol, Vec};

/// Storage keys for the governor contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Token,
    ProposalCount,
    Proposal(u32),
    Vote(u32, Address),
    /// Records the delegatee chosen by a given caller. Absence means the
    /// caller votes with their own balance (self-delegation / no delegation).
    Delegate(Address),
    /// Running total of voting power delegated *to* a given address, i.e. the
    /// sum of the token balances of every address that has delegated to it.
    /// Maintained incrementally on delegate/undelegate so `cast_vote` never
    /// has to iterate a global delegator list.
    DelegatedWeight(Address),
}

#[contracttype]
#[derive(Clone, PartialEq)]
pub enum VoteType {
    Against,
    For,
    Abstain,
}

#[contracttype]
#[derive(Clone)]
pub struct Proposal {
    pub id: u32,
    pub proposer: Address,
    pub description: Symbol,
    pub for_votes: i128,
    pub against_votes: i128,
    pub abstain_votes: i128,
    pub executed: bool,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GovernorError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    ProposalNotFound = 3,
    AlreadyVoted = 4,
    VotingClosed = 5,
    Unauthorized = 6,
    /// A delegation would create a chain (the delegatee has itself delegated
    /// elsewhere). This contract uses a single-hop-only delegation model.
    DelegationChain = 7,
}

#[contract]
pub struct RefractGovernor;

#[contractimpl]
impl RefractGovernor {
    pub fn initialize(env: Env, admin: Address, token: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::ProposalCount, &0u32);
    }

    pub fn propose(env: Env, proposer: Address, description: Symbol) -> u32 {
        proposer.require_auth();
        let mut count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ProposalCount)
            .unwrap_or(0);
        count += 1;
        let proposal = Proposal {
            id: count,
            proposer,
            description,
            for_votes: 0,
            against_votes: 0,
            abstain_votes: 0,
            executed: false,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(count), &proposal);
        env.storage().instance().set(&DataKey::ProposalCount, &count);
        count
    }

    /// Assign `caller`'s voting power to `delegatee` without transferring the
    /// underlying tokens. Delegation is single-hop only: a delegatee may not
    /// itself have delegated elsewhere, otherwise a chain would form and the
    /// running `DelegatedWeight` counter could not be resolved in O(1).
    pub fn delegate(env: Env, caller: Address, delegatee: Address) {
        caller.require_auth();

        // Single-hop policy: reject delegating to an address that has itself
        // delegated to a third party.
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::Delegate(delegatee.clone()))
        {
            if existing != delegatee {
                panic!("delegation chain not supported");
            }
        }

        let balance = Self::token_balance(&env, &caller);

        // Remove any previous delegation from the running counter.
        if let Some(previous) = env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::Delegate(caller.clone()))
        {
            if previous != caller {
                let prev_weight: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::DelegatedWeight(previous.clone()))
                    .unwrap_or(0);
                env.storage().persistent().set(
                    &DataKey::DelegatedWeight(previous),
                    &(prev_weight - balance),
                );
            }
        }

        // Record the new delegation and add the balance to the delegatee.
        env.storage()
            .persistent()
            .set(&DataKey::Delegate(caller.clone()), &delegatee);

        if delegatee != caller {
            let new_weight: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::DelegatedWeight(delegatee.clone()))
                .unwrap_or(0);
            env.storage()
                .persistent()
                .set(&DataKey::DelegatedWeight(delegatee), &(new_weight + balance));
        }
    }

    /// Restore direct voting for `caller`. Equivalent to
    /// `delegate(caller, caller)` (self-delegation is the "no delegation"
    /// state).
    pub fn undelegate(env: Env, caller: Address) {
        caller.require_auth();
        Self::delegate(env, caller.clone(), caller);
    }

    pub fn cast_vote(env: Env, voter: Address, proposal_id: u32, vote: VoteType) {
        voter.require_auth();

        if env
            .storage()
            .persistent()
            .has(&DataKey::Vote(proposal_id, voter.clone()))
        {
            panic!("already voted");
        }

        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .expect("proposal not found");

        // Weight = own token balance + balances delegated to this address.
        let weight = Self::voting_power(&env, &voter);

        match vote {
            VoteType::For => proposal.for_votes += weight,
            VoteType::Against => proposal.against_votes += weight,
            VoteType::Abstain => proposal.abstain_votes += weight,
        }

        env.storage()
            .persistent()
            .set(&DataKey::Vote(proposal_id, voter), &vote);
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);
    }

    pub fn get_proposal(env: Env, proposal_id: u32) -> Proposal {
        env.storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .expect("proposal not found")
    }

    /// Total voting power for `account`: its own token balance plus the
    /// balances of every address that has delegated to it.
    pub fn voting_power(env: &Env, account: &Address) -> i128 {
        let own = Self::token_balance(env, account);
        let delegated: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::DelegatedWeight(account.clone()))
            .unwrap_or(0);
        own + delegated
    }

    /// Returns the delegatee for `account`, or `account` itself when no
    /// delegation is recorded (self-delegation = direct voting).
    pub fn get_delegate(env: Env, account: Address) -> Address {
        env.storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::Delegate(account.clone()))
            .unwrap_or(account)
    }

    fn token_balance(env: &Env, account: &Address) -> i128 {
        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::Token)
            .expect("token not set");
        let client = token::Client::new(env, &token_addr);
        client.balance(account)
    }
}
