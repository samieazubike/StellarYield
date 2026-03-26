#![no_std]

//! # YieldVault — Core Soroban Vault for Automated Rebalancing
//!
//! Accepts user deposits of SAC tokens (XLM, USDC, etc.), tracks ownership
//! via LP-style vault shares, and exposes an admin-gated `rebalance`
//! function for moving funds across liquidity pools.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

// ── Storage keys ────────────────────────────────────────────────────────

#[contracttype]
enum DataKey {
    Admin,
    Token,
    TotalShares,
    TotalAssets,
    Shares(Address),
    Initialized,
}

// ── Errors ──────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum VaultError {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    ZeroAmount = 3,
    InsufficientShares = 4,
    Unauthorized = 5,
    ZeroSupply = 6,
}

// ── Contract ────────────────────────────────────────────────────────────

#[contract]
pub struct YieldVault;

#[contractimpl]
impl YieldVault {
    // ── Initialisation ──────────────────────────────────────────────

    /// Initialise the vault with an admin (strategy) address and the
    /// deposit token address.
    ///
    /// Can only be called once. The admin is the sole address allowed to
    /// call `rebalance`.
    ///
    /// # Arguments
    /// * `admin` — The strategy / admin address that controls rebalancing.
    /// * `token` — The SAC token address accepted for deposits (e.g. USDC).
    pub fn initialize(env: Env, admin: Address, token: Address) -> Result<(), VaultError> {
        if env.storage().instance().has(&DataKey::Initialized) {
            return Err(VaultError::AlreadyInitialized);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage().instance().set(&DataKey::TotalShares, &0i128);
        env.storage().instance().set(&DataKey::TotalAssets, &0i128);
        env.storage().instance().set(&DataKey::Initialized, &true);

        env.events()
            .publish((symbol_short!("init"),), (admin.clone(), token.clone()));

        Ok(())
    }

    // ── Deposits ────────────────────────────────────────────────────

    /// Deposit `amount` of the vault token and receive proportional vault
    /// shares in return.
    ///
    /// The first depositor sets the 1:1 ratio (shares == assets). All
    /// subsequent deposits receive shares proportional to their
    /// contribution relative to total vault assets.
    ///
    /// # Arguments
    /// * `from`   — The depositor's address (must authorise the call).
    /// * `amount` — The quantity of tokens to deposit (must be > 0).
    ///
    /// # Returns
    /// The number of vault shares minted for this deposit.
    pub fn deposit(env: Env, from: Address, amount: i128) -> Result<i128, VaultError> {
        Self::require_init(&env)?;
        from.require_auth();

        if amount <= 0 {
            return Err(VaultError::ZeroAmount);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let total_shares: i128 = env.storage().instance().get(&DataKey::TotalShares).unwrap();
        let total_assets: i128 = env.storage().instance().get(&DataKey::TotalAssets).unwrap();

        // Calculate shares to mint
        let shares = if total_shares == 0 {
            amount // First deposit: 1:1
        } else {
            (amount * total_shares) / total_assets
        };

        if shares <= 0 {
            return Err(VaultError::ZeroAmount);
        }

        // Transfer tokens from depositor to vault
        let client = token::Client::new(&env, &token_addr);
        client.transfer(&from, &env.current_contract_address(), &amount);

        // Update state
        let user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(from.clone()))
            .unwrap_or(0);

        env.storage()
            .persistent()
            .set(&DataKey::Shares(from.clone()), &(user_shares + shares));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares + shares));
        env.storage()
            .instance()
            .set(&DataKey::TotalAssets, &(total_assets + amount));

        env.events()
            .publish((symbol_short!("deposit"),), (from, amount, shares));

        Ok(shares)
    }

    // ── Withdrawals ─────────────────────────────────────────────────

    /// Burn `shares` vault shares and receive the proportional amount of
    /// underlying tokens.
    ///
    /// # Arguments
    /// * `to`     — The recipient address (must authorise the call).
    /// * `shares` — Number of vault shares to redeem (must be > 0).
    ///
    /// # Returns
    /// The amount of underlying tokens transferred to the user.
    pub fn withdraw(env: Env, to: Address, shares: i128) -> Result<i128, VaultError> {
        Self::require_init(&env)?;
        to.require_auth();

        if shares <= 0 {
            return Err(VaultError::ZeroAmount);
        }

        let user_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(to.clone()))
            .unwrap_or(0);

        if user_shares < shares {
            return Err(VaultError::InsufficientShares);
        }

        let total_shares: i128 = env.storage().instance().get(&DataKey::TotalShares).unwrap();
        let total_assets: i128 = env.storage().instance().get(&DataKey::TotalAssets).unwrap();

        if total_shares == 0 {
            return Err(VaultError::ZeroSupply);
        }

        let amount = (shares * total_assets) / total_shares;

        // Transfer tokens to user
        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);
        client.transfer(&env.current_contract_address(), &to, &amount);

        // Update state
        env.storage()
            .persistent()
            .set(&DataKey::Shares(to.clone()), &(user_shares - shares));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares - shares));
        env.storage()
            .instance()
            .set(&DataKey::TotalAssets, &(total_assets - amount));

        env.events()
            .publish((symbol_short!("withdraw"),), (to, amount, shares));

        Ok(amount)
    }

    // ── Rebalancing (admin only) ────────────────────────────────────

    /// Move `amount` tokens from the vault to a target protocol address.
    ///
    /// This is the core rebalancing primitive — only callable by the
    /// contract admin (strategy address). The strategy off-chain logic
    /// determines *where* to allocate; this function executes the transfer.
    ///
    /// # Arguments
    /// * `caller` — Must be the admin address.
    /// * `target` — The protocol / pool address to send funds to.
    /// * `amount` — Amount of tokens to move.
    pub fn rebalance(
        env: Env,
        caller: Address,
        target: Address,
        amount: i128,
    ) -> Result<(), VaultError> {
        Self::require_init(&env)?;
        caller.require_auth();

        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if caller != admin {
            return Err(VaultError::Unauthorized);
        }

        if amount <= 0 {
            return Err(VaultError::ZeroAmount);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let total_assets: i128 = env.storage().instance().get(&DataKey::TotalAssets).unwrap();

        let client = token::Client::new(&env, &token_addr);
        client.transfer(&env.current_contract_address(), &target, &amount);

        // Update tracked assets to reflect funds sent out
        env.storage()
            .instance()
            .set(&DataKey::TotalAssets, &(total_assets - amount));

        env.events()
            .publish((symbol_short!("rebal"),), (target, amount));

        Ok(())
    }

    // ── View functions ──────────────────────────────────────────────

    /// Returns the number of vault shares held by `user`.
    pub fn get_shares(env: Env, user: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Shares(user))
            .unwrap_or(0)
    }

    /// Returns the total vault shares in circulation.
    pub fn total_shares(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(0)
    }

    /// Returns the total assets held by the vault.
    pub fn total_assets(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalAssets)
            .unwrap_or(0)
    }

    /// Returns the admin address.
    pub fn get_admin(env: Env) -> Result<Address, VaultError> {
        Self::require_init(&env)?;
        Ok(env.storage().instance().get(&DataKey::Admin).unwrap())
    }

    /// Returns the deposit token address.
    pub fn get_token(env: Env) -> Result<Address, VaultError> {
        Self::require_init(&env)?;
        Ok(env.storage().instance().get(&DataKey::Token).unwrap())
    }

    // ── Internal ────────────────────────────────────────────────────

    fn require_init(env: &Env) -> Result<(), VaultError> {
        if !env.storage().instance().has(&DataKey::Initialized) {
            return Err(VaultError::NotInitialized);
        }
        Ok(())
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::Env;

    fn setup_env() -> (Env, YieldVaultClient<'static>, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(YieldVault, ());
        let client = YieldVaultClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_contract = env.register_stellar_asset_contract_v2(token_admin.clone());
        let token_addr = token_contract.address();

        client.initialize(&admin, &token_addr);

        (env, client, admin, token_addr, token_admin)
    }

    fn mint_tokens(env: &Env, token_addr: &Address, _admin: &Address, to: &Address, amount: i128) {
        let admin_client = soroban_sdk::token::StellarAssetClient::new(env, token_addr);
        admin_client.mint(to, &amount);
    }

    #[test]
    fn test_initialize() {
        let (_, client, admin, token_addr, _) = setup_env();
        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.get_token(), token_addr);
        assert_eq!(client.total_shares(), 0);
        assert_eq!(client.total_assets(), 0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2)")]
    fn test_double_initialize_panics() {
        let (env, client, admin, token_addr, _) = setup_env();
        let new_admin = Address::generate(&env);
        let _ = admin;
        client.initialize(&new_admin, &token_addr);
    }

    #[test]
    fn test_deposit_first_user() {
        let (env, client, _, token_addr, token_admin) = setup_env();
        let user = Address::generate(&env);
        mint_tokens(&env, &token_addr, &token_admin, &user, 1000);

        let shares = client.deposit(&user, &1000);
        assert_eq!(shares, 1000); // 1:1 for first deposit
        assert_eq!(client.get_shares(&user), 1000);
        assert_eq!(client.total_shares(), 1000);
        assert_eq!(client.total_assets(), 1000);
    }

    #[test]
    fn test_deposit_second_user_proportional() {
        let (env, client, _, token_addr, token_admin) = setup_env();
        let user1 = Address::generate(&env);
        let user2 = Address::generate(&env);

        mint_tokens(&env, &token_addr, &token_admin, &user1, 1000);
        mint_tokens(&env, &token_addr, &token_admin, &user2, 500);

        client.deposit(&user1, &1000);
        let shares2 = client.deposit(&user2, &500);

        assert_eq!(shares2, 500); // proportional to existing ratio
        assert_eq!(client.total_shares(), 1500);
        assert_eq!(client.total_assets(), 1500);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn test_deposit_zero_panics() {
        let (env, client, _, _, _) = setup_env();
        let user = Address::generate(&env);
        client.deposit(&user, &0);
    }

    #[test]
    fn test_withdraw() {
        let (env, client, _, token_addr, token_admin) = setup_env();
        let user = Address::generate(&env);
        mint_tokens(&env, &token_addr, &token_admin, &user, 1000);

        client.deposit(&user, &1000);
        let amount = client.withdraw(&user, &500);

        assert_eq!(amount, 500);
        assert_eq!(client.get_shares(&user), 500);
        assert_eq!(client.total_shares(), 500);
        assert_eq!(client.total_assets(), 500);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #4)")]
    fn test_withdraw_insufficient_shares_panics() {
        let (env, client, _, token_addr, token_admin) = setup_env();
        let user = Address::generate(&env);
        mint_tokens(&env, &token_addr, &token_admin, &user, 1000);

        client.deposit(&user, &1000);
        client.withdraw(&user, &2000);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn test_withdraw_zero_panics() {
        let (env, client, _, token_addr, token_admin) = setup_env();
        let user = Address::generate(&env);
        mint_tokens(&env, &token_addr, &token_admin, &user, 1000);

        client.deposit(&user, &1000);
        client.withdraw(&user, &0);
    }

    #[test]
    fn test_rebalance_by_admin() {
        let (env, client, admin, token_addr, token_admin) = setup_env();
        let user = Address::generate(&env);
        let target_pool = Address::generate(&env);

        mint_tokens(&env, &token_addr, &token_admin, &user, 1000);
        client.deposit(&user, &1000);

        client.rebalance(&admin, &target_pool, &300);

        // Token balance of target should have 300
        let token_client = token::Client::new(&env, &token_addr);
        assert_eq!(token_client.balance(&target_pool), 300);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_rebalance_by_non_admin_panics() {
        let (env, client, _, token_addr, token_admin) = setup_env();
        let user = Address::generate(&env);
        let target = Address::generate(&env);
        let impostor = Address::generate(&env);

        mint_tokens(&env, &token_addr, &token_admin, &user, 1000);
        client.deposit(&user, &1000);

        client.rebalance(&impostor, &target, &100);
    }

    #[test]
    fn test_full_lifecycle() {
        let (env, client, admin, token_addr, token_admin) = setup_env();
        let user = Address::generate(&env);
        let pool = Address::generate(&env);

        // Deposit
        mint_tokens(&env, &token_addr, &token_admin, &user, 5000);
        client.deposit(&user, &5000);
        assert_eq!(client.get_shares(&user), 5000);

        // Rebalance some to pool
        client.rebalance(&admin, &pool, &2000);

        // Withdraw remaining shares
        let withdrawn = client.withdraw(&user, &5000);
        // User gets proportional amount of what's left in vault
        assert_eq!(withdrawn, 3000);
        assert_eq!(client.get_shares(&user), 0);
        assert_eq!(client.total_shares(), 0);
    }

    #[test]
    fn test_get_shares_unregistered_user() {
        let (env, client, _, _, _) = setup_env();
        let unknown = Address::generate(&env);
        assert_eq!(client.get_shares(&unknown), 0);
    }
}
