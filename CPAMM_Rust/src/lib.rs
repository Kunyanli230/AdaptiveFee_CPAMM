use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount, Transfer, Burn};

declare_id!("Adap1veCpAMM_Rust");

/// Fixed-point scale for prices/EMA/slippage signals.
const SCALE: u128 = 1_000_000_000_000; // 1e12
/// Basis points denominator
const BPS_DENOM: u64 = 10_000;

#[program]
pub mod adaptive_cpamm {
    use super::*;

    /// Initialize a new pool.
    /// Creates:
    /// - Pool state PDA
    /// - LP mint (authority = pool PDA)
    /// - Vault token accounts owned by pool PDA
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        min_fee_bps: u16,
        max_fee_bps: u16,
        beta_vol_bps_per1e12: u16,
        gamma_slip_bps_per1e12: u16,
        delta_shallow_bps_per1e12: u16,
        ema_alpha_1e12: u64,       // e.g., 0.05 * 1e12
        breaker_vol_threshold_1e12: u64, // e.g., 0.20 * 1e12
    ) -> Result<()> {
        require!(min_fee_bps <= max_fee_bps, AmmError::BadBounds);
        let pool = &mut ctx.accounts.pool;
        pool.bump = *ctx.bumps.get("pool").unwrap();
        pool.authority = ctx.accounts.authority.key();
        pool.token0_mint = ctx.accounts.token0_mint.key();
        pool.token1_mint = ctx.accounts.token1_mint.key();
        pool.vault0 = ctx.accounts.vault0.key();
        pool.vault1 = ctx.accounts.vault1.key();
        pool.lp_mint = ctx.accounts.lp_mint.key();

        pool.reserve0 = 0;
        pool.reserve1 = 0;

        pool.min_fee_bps = min_fee_bps;
        pool.max_fee_bps = max_fee_bps;
        pool.beta_vol_bps_per1e12 = beta_vol_bps_per1e12;
        pool.gamma_slip_bps_per1e12 = gamma_slip_bps_per1e12;
        pool.delta_shallow_bps_per1e12 = delta_shallow_bps_per1e12;

        pool.ema_price_1e12 = 0; // initialize on first liquidity
        pool.ema_alpha_1e12 = ema_alpha_1e12;
        pool.breaker_vol_threshold_1e12 = breaker_vol_threshold_1e12;

        Ok(())
    }

    /// Admin: update parameters
    pub fn set_params(
        ctx: Context<SetParams>,
        min_fee_bps: u16,
        max_fee_bps: u16,
        beta_vol_bps_per1e12: u16,
        gamma_slip_bps_per1e12: u16,
        delta_shallow_bps_per1e12: u16,
        ema_alpha_1e12: u64,
        breaker_vol_threshold_1e12: u64,
    ) -> Result<()> {
        require!(min_fee_bps <= max_fee_bps, AmmError::BadBounds);
        let pool = &mut ctx.accounts.pool;
        require_keys_eq!(pool.authority, ctx.accounts.authority.key(), AmmError::NotAuthorized);

        pool.min_fee_bps = min_fee_bps;
        pool.max_fee_bps = max_fee_bps;
        pool.beta_vol_bps_per1e12 = beta_vol_bps_per1e12;
        pool.gamma_slip_bps_per1e12 = gamma_slip_bps_per1e12;
        pool.delta_shallow_bps_per1e12 = delta_shallow_bps_per1e12;
        pool.ema_alpha_1e12 = ema_alpha_1e12;
        pool.breaker_vol_threshold_1e12 = breaker_vol_threshold_1e12;
        Ok(())
    }

    /// Add liquidity (must match current price ratio when pool has liquidity).
    /// Mints LP shares to provider.
    pub fn add_liquidity(
        ctx: Context<AddLiquidity>,
        amount0: u64,
        amount1: u64,
    ) -> Result<()> {
        require!(amount0 > 0 && amount1 > 0, AmmError::ZeroAmount);

        let pool = &mut ctx.accounts.pool;

        // Enforce price invariance when reserves > 0
        if pool.reserve0 > 0 && pool.reserve1 > 0 {
            // reserve0 * amount1 == reserve1 * amount0
            let lhs = (pool.reserve0 as u128) * (amount1 as u128);
            let rhs = (pool.reserve1 as u128) * (amount0 as u128);
            require!(lhs == rhs, AmmError::BadRatio);
        }

        // Pull tokens into vaults
        transfer_into_vault(
            &ctx.accounts.user,
            &ctx.accounts.user_token0,
            &ctx.accounts.vault0,
            &ctx.accounts.token_program,
            amount0,
        )?;
        transfer_into_vault(
            &ctx.accounts.user,
            &ctx.accounts.user_token1,
            &ctx.accounts.vault1,
            &ctx.accounts.token_program,
            amount1,
        )?;

        // Update reserves from vault balances
        let new_bal0 = ctx.accounts.vault0.amount;
        let new_bal1 = ctx.accounts.vault1.amount;

        let (shares_to_mint, new_reserve0, new_reserve1) = if pool.total_lp_supply == 0 {
            // L0 = sqrt(x*y)
            let k = (new_bal0 as u128)
                .checked_mul(new_bal1 as u128)
                .ok_or(AmmError::MathOverflow)?;
            let shares = isqrt(k) as u64;

            // init EMA with first spot price
            if pool.ema_price_1e12 == 0 {
                let price = spot_price_1e12(new_bal0, new_bal1)?;
                pool.ema_price_1e12 = price;
            }

            (shares, new_bal0, new_bal1)
        } else {
            // shares = min( dx/x * T, dy/y * T )
            let t = pool.total_lp_supply as u128;
            let dx = (amount0 as u128)
                .checked_mul(t)
                .ok_or(AmmError::MathOverflow)?
                / (pool.reserve0 as u128);
            let dy = (amount1 as u128)
                .checked_mul(t)
                .ok_or(AmmError::MathOverflow)?
                / (pool.reserve1 as u128);
            let shares = u128::min(dx, dy) as u64;
            (shares, new_bal0, new_bal1)
        };

        require!(shares_to_mint > 0, AmmError::ZeroShares);

        // Mint LP shares to user
        mint_lp_shares(
            &ctx.accounts.pool,
            &ctx.accounts.lp_mint,
            &ctx.accounts.user_lp,
            &ctx.accounts.token_program,
            shares_to_mint,
            &ctx.accounts.pool_signer,
        )?;

        // Save reserves & total supply
        pool.reserve0 = new_reserve0;
        pool.reserve1 = new_reserve1;
        pool.total_lp_supply = pool
            .total_lp_supply
            .checked_add(shares_to_mint)
            .ok_or(AmmError::MathOverflow)?;

        // Optional EMA update after add
        if pool.reserve0 > 0 && pool.reserve1 > 0 {
            let price = spot_price_1e12(pool.reserve0, pool.reserve1)?;
            ema_update(&mut pool.ema_price_1e12, pool.ema_alpha_1e12, price);
        }

        emit!(MintEvent {
            sender: ctx.accounts.user.key(),
            amount0,
            amount1,
            shares: shares_to_mint
        });

        Ok(())
    }

    /// Remove liquidity: burns LP and returns tokens pro-rata.
    pub fn remove_liquidity(ctx: Context<RemoveLiquidity>, shares: u64) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        require!(shares > 0, AmmError::ZeroShares);
        require!(pool.total_lp_supply >= shares, AmmError::InsufficientLP);

        // Burn LP from user
        burn_lp_shares(
            &ctx.accounts.user,
            &ctx.accounts.user_lp,
            &ctx.accounts.lp_mint,
            &ctx.accounts.token_program,
            shares,
        )?;

        // Compute pro-rata amounts
        let bal0 = ctx.accounts.vault0.amount;
        let bal1 = ctx.accounts.vault1.amount;

        let amount0 = (shares as u128)
            .checked_mul(bal0 as u128)
            .ok_or(AmmError::MathOverflow)?
            / (pool.total_lp_supply as u128);
        let amount1 = (shares as u128)
            .checked_mul(bal1 as u128)
            .ok_or(AmmError::MathOverflow)?
            / (pool.total_lp_supply as u128);

        // Update pool supply before transfer out
        pool.total_lp_supply = pool
            .total_lp_supply
            .checked_sub(shares)
            .ok_or(AmmError::MathOverflow)?;

        // Transfer out to user
        transfer_from_vault(
            &ctx.accounts.pool,
            &ctx.accounts.vault0,
            &ctx.accounts.user_token0,
            &ctx.accounts.token_program,
            amount0 as u64,
            &ctx.accounts.pool_signer,
        )?;
        transfer_from_vault(
            &ctx.accounts.pool,
            &ctx.accounts.vault1,
            &ctx.accounts.user_token1,
            &ctx.accounts.token_program,
            amount1 as u64,
            &ctx.accounts.pool_signer,
        )?;

        // Update reserves from vault balances
        pool.reserve0 = ctx.accounts.vault0.amount;
        pool.reserve1 = ctx.accounts.vault1.amount;

        // Optional EMA update
        if pool.reserve0 > 0 && pool.reserve1 > 0 {
            let price = spot_price_1e12(pool.reserve0, pool.reserve1)?;
            ema_update(&mut pool.ema_price_1e12, pool.ema_alpha_1e12, price);
        }

        emit!(BurnEvent {
            sender: ctx.accounts.user.key(),
            shares,
            amount0: amount0 as u64,
            amount1: amount1 as u64
        });

        Ok(())
    }

    /// Swap with adaptive fee and a circuit breaker on excessive volatility.
    pub fn swap(ctx: Context<Swap>, token_in_is_0: bool, amount_in: u64) -> Result<()> {
        require!(amount_in > 0, AmmError::ZeroAmount);
        let pool = &mut ctx.accounts.pool;

        // Pull token_in from user → vault
        if token_in_is_0 {
            transfer_into_vault(
                &ctx.accounts.user,
                &ctx.accounts.user_token_in,
                &ctx.accounts.vault0,
                &ctx.accounts.token_program,
                amount_in,
            )?;
        } else {
            transfer_into_vault(
                &ctx.accounts.user,
                &ctx.accounts.user_token_in,
                &ctx.accounts.vault1,
                &ctx.accounts.token_program,
                amount_in,
            )?;
        }

        // Refresh reserves from vault balances
        let r0 = ctx.accounts.vault0.amount as u128;
        let r1 = ctx.accounts.vault1.amount as u128;
        require!(r0 > 0 && r1 > 0, AmmError::NoLiquidity);

        // Compute dynamic fee & components
        let (fee_bps, vol_1e12, _slip_1e12, _shallow_1e12) =
            compute_dynamic_fee(pool, token_in_is_0, amount_in as u128, r0, r1)?;

        // Circuit breaker
        require!(
            vol_1e12 <= pool.breaker_vol_threshold_1e12 as u128,
            AmmError::VolTooHigh
        );

        // x*y=k pricing with fee on amountIn
        let (rin, rout) = if token_in_is_0 { (r0, r1) } else { (r1, r0) };

        let fee_num = (BPS_DENOM - fee_bps as u64) as u128;
        let dx_fee = (amount_in as u128)
            .checked_mul(fee_num)
            .ok_or(AmmError::MathOverflow)?
            / (BPS_DENOM as u128);

        let amount_out = (rout
            .checked_mul(dx_fee)
            .ok_or(AmmError::MathOverflow)?)
            / (rin
            .checked_add(dx_fee)
            .ok_or(AmmError::MathOverflow)?);

        require!(amount_out > 0, AmmError::AmountOutZero);

        // Send token_out to user from vault
        if token_in_is_0 {
            transfer_from_vault(
                &ctx.accounts.pool,
                &ctx.accounts.vault1,
                &ctx.accounts.user_token_out,
                &ctx.accounts.token_program,
                amount_out as u64,
                &ctx.accounts.pool_signer,
            )?;
        } else {
            transfer_from_vault(
                &ctx.accounts.pool,
                &ctx.accounts.vault0,
                &ctx.accounts.user_token_out,
                &ctx.accounts.token_program,
                amount_out as u64,
                &ctx.accounts.pool_signer,
            )?;
        }

        // Update reserves
        pool.reserve0 = ctx.accounts.vault0.amount;
        pool.reserve1 = ctx.accounts.vault1.amount;

        // Update EMA
        let price = spot_price_1e12(pool.reserve0, pool.reserve1)?;
        ema_update(&mut pool.ema_price_1e12, pool.ema_alpha_1e12, price);

        emit!(SwapEvent {
            trader: ctx.accounts.user.key(),
            token_in_is_0,
            amount_in,
            amount_out: amount_out as u64,
            fee_bps
        });

        Ok(())
    }
}

/* ------------------------------- State ---------------------------------- */

#[account]
pub struct Pool {
    pub bump: u8,
    pub authority: Pubkey,

    pub token0_mint: Pubkey,
    pub token1_mint: Pubkey,
    pub vault0: Pubkey,
    pub vault1: Pubkey,

    pub lp_mint: Pubkey,
    pub total_lp_supply: u64,

    // reserves mirrored from vault balances
    pub reserve0: u64,
    pub reserve1: u64,

    // fee params (bps and per-1e12 coefficients)
    pub min_fee_bps: u16,
    pub max_fee_bps: u16,
    pub beta_vol_bps_per1e12: u16,
    pub gamma_slip_bps_per1e12: u16,
    pub delta_shallow_bps_per1e12: u16,

    // EMA config and circuit breaker
    pub ema_price_1e12: u64,
    pub ema_alpha_1e12: u64,
    pub breaker_vol_threshold_1e12: u64,
}

impl Pool {
    pub fn seeds(&self) -> [&[u8]; 2] {
        [b"pool", &[self.bump]]
    }
}

/* ------------------------------- Events --------------------------------- */

#[event]
pub struct SwapEvent {
    pub trader: Pubkey,
    pub token_in_is_0: bool,
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee_bps: u16,
}

#[event]
pub struct MintEvent {
    pub sender: Pubkey,
    pub amount0: u64,
    pub amount1: u64,
    pub shares: u64,
}

#[event]
pub struct BurnEvent {
    pub sender: Pubkey,
    pub shares: u64,
    pub amount0: u64,
    pub amount1: u64,
}

/* ------------------------------- Contexts -------------------------------- */

#[derive(Accounts)]
pub struct InitializePool<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    /// Pool PDA
    #[account(
        init,
        payer = authority,
        space = 8 +  // discriminator
            1 + 32 + // bump + authority
            32 + 32 + 32 + 32 + // mints/vaults
            32 + 8 + // lp_mint + total_lp_supply
            8 + 8 +  // reserves
            2 + 2 + 2 + 2 + 2 + // fee params
            8 + 8 + 8, // ema + alpha + breaker
        seeds = [b"pool"],
        bump
    )]
    pub pool: Account<'info, Pool>,

    // Token mints
    pub token0_mint: Account<'info, Mint>,
    pub token1_mint: Account<'info, Mint>,

    /// LP mint (authority = pool PDA)
    #[account(
        init,
        payer = authority,
        mint::decimals = 9,
        mint::authority = pool,
        mint::freeze_authority = pool
    )]
    pub lp_mint: Account<'info, Mint>,

    /// Vaults (owned by pool PDA)
    #[account(
        init,
        payer = authority,
        associated_token::mint = token0_mint,
        associated_token::authority = pool
    )]
    pub vault0: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = authority,
        associated_token::mint = token1_mint,
        associated_token::authority = pool
    )]
    pub vault1: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct SetParams<'info> {
    pub authority: Signer<'info>,
    #[account(mut, seeds=[b"pool"], bump=pool.bump)]
    pub pool: Account<'info, Pool>,
}

#[derive(Accounts)]
pub struct AddLiquidity<'info> {
    /// Liquidity provider
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut, seeds=[b"pool"], bump=pool.bump)]
    pub pool: Account<'info, Pool>,

    // User token accounts
    #[account(mut, constraint = user_token0.mint == pool.token0_mint)]
    pub user_token0: Account<'info, TokenAccount>,
    #[account(mut, constraint = user_token1.mint == pool.token1_mint)]
    pub user_token1: Account<'info, TokenAccount>,

    // Vaults
    #[account(mut, address = pool.vault0)]
    pub vault0: Account<'info, TokenAccount>,
    #[account(mut, address = pool.vault1)]
    pub vault1: Account<'info, TokenAccount>,

    // LP mint and recipient
    #[account(mut, address = pool.lp_mint)]
    pub lp_mint: Account<'info, Mint>,
    #[account(mut)]
    pub user_lp: Account<'info, TokenAccount>,

    /// CHECK: pool signer PDA for CPIs
    #[account(seeds=[b"pool"], bump=pool.bump)]
    pub pool_signer: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RemoveLiquidity<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut, seeds=[b"pool"], bump=pool.bump)]
    pub pool: Account<'info, Pool>,

    // Vaults
    #[account(mut, address = pool.vault0)]
    pub vault0: Account<'info, TokenAccount>,
    #[account(mut, address = pool.vault1)]
    pub vault1: Account<'info, TokenAccount>,

    // LP
    #[account(mut, address = pool.lp_mint)]
    pub lp_mint: Account<'info, Mint>,
    #[account(mut, constraint = user_lp.mint == pool.lp_mint)]
    pub user_lp: Account<'info, TokenAccount>,

    // user token outs
    #[account(mut, constraint = user_token0.mint == pool.token0_mint)]
    pub user_token0: Account<'info, TokenAccount>,
    #[account(mut, constraint = user_token1.mint == pool.token1_mint)]
    pub user_token1: Account<'info, TokenAccount>,

    /// CHECK: pool signer PDA
    #[account(seeds=[b"pool"], bump=pool.bump)]
    pub pool_signer: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Swap<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut, seeds=[b"pool"], bump=pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(mut, address = pool.vault0)]
    pub vault0: Account<'info, TokenAccount>,
    #[account(mut, address = pool.vault1)]
    pub vault1: Account<'info, TokenAccount>,

    // For convenience we pass generic "in/out" ATAs bound to the chosen side
    #[account(mut)]
    pub user_token_in: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_token_out: Account<'info, TokenAccount>,

    /// CHECK: pool signer PDA
    #[account(seeds=[b"pool"], bump=pool.bump)]
    pub pool_signer: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

/* ------------------------------- Helpers -------------------------------- */

fn transfer_into_vault<'info>(
    user: &Signer<'info>,
    user_ata: &Account<'info, TokenAccount>,
    vault: &Account<'info, TokenAccount>,
    token_program: &Program<'info, Token>,
    amount: u64,
) -> Result<()> {
    let cpi_accounts = Transfer {
        from: user_ata.to_account_info(),
        to: vault.to_account_info(),
        authority: user.to_account_info(),
    };
    token::transfer(CpiContext::new(token_program.to_account_info(), cpi_accounts), amount)
}

fn transfer_from_vault<'info>(
    pool: &Account<'info, Pool>,
    vault: &Account<'info, TokenAccount>,
    user_ata: &Account<'info, TokenAccount>,
    token_program: &Program<'info, Token>,
    amount: u64,
    pool_signer: &UncheckedAccount<'info>,
) -> Result<()> {
    let seeds = &[b"pool".as_ref(), &[pool.bump]];
    let signer = &[&seeds[..]];
    let cpi_accounts = Transfer {
        from: vault.to_account_info(),
        to: user_ata.to_account_info(),
        authority: pool_signer.to_account_info(),
    };
    token::transfer(
        CpiContext::new_with_signer(token_program.to_account_info(), cpi_accounts, signer),
        amount,
    )
}

fn mint_lp_shares<'info>(
    pool: &Account<'info, Pool>,
    lp_mint: &Account<'info, Mint>,
    user_lp: &Account<'info, TokenAccount>,
    token_program: &Program<'info, Token>,
    amount: u64,
    pool_signer: &UncheckedAccount<'info>,
) -> Result<()> {
    let seeds = &[b"pool".as_ref(), &[pool.bump]];
    let signer = &[&seeds[..]];
    let cpi_accounts = MintTo {
        mint: lp_mint.to_account_info(),
        to: user_lp.to_account_info(),
        authority: pool_signer.to_account_info(),
    };
    token::mint_to(
        CpiContext::new_with_signer(token_program.to_account_info(), cpi_accounts, signer),
        amount,
    )
}

fn burn_lp_shares<'info>(
    user: &Signer<'info>,
    user_lp: &Account<'info, TokenAccount>,
    lp_mint: &Account<'info, Mint>,
    token_program: &Program<'info, Token>,
    amount: u64,
) -> Result<()> {
    let cpi_accounts = Burn {
        from: user_lp.to_account_info(),
        mint: lp_mint.to_account_info(),
        authority: user.to_account_info(),
    };
    token::burn(CpiContext::new(token_program.to_account_info(), cpi_accounts), amount)
}

/// Spot price token0 in token1 (scaled by 1e12).
fn spot_price_1e12(reserve0: u64, reserve1: u64) -> Result<u64> {
    require!(reserve0 > 0 && reserve1 > 0, AmmError::NoLiquidity);
    let p = (reserve1 as u128)
        .checked_mul(SCALE)
        .ok_or(AmmError::MathOverflow)?
        / (reserve0 as u128);
    Ok(p as u64)
}

/// Simple integer sqrt (Babylonian)
fn isqrt(y: u128) -> u128 {
    if y == 0 {
        return 0;
    }
    let mut z = y;
    let mut x = y / 2 + 1;
    while x < z {
        z = x;
        x = (y / x + x) / 2;
    }
    z
}

/// EMA <- EMA + alpha * (price - EMA), all scaled by 1e12.
fn ema_update(ema: &mut u64, alpha_1e12: u64, price_1e12: u64) {
    let ema_u = *ema as u128;
    let price_u = price_1e12 as u128;
    if price_u >= ema_u {
        let diff = price_u - ema_u;
        let delta = diff
            .saturating_mul(alpha_1e12 as u128)
            / SCALE;
        *ema = ema_u.saturating_add(delta) as u64;
    } else {
        let diff = ema_u - price_u;
        let delta = diff
            .saturating_mul(alpha_1e12 as u128)
            / SCALE;
        *ema = ema_u.saturating_sub(delta) as u64;
    }
}

/// Compute dynamic fee and its components (vol/slip/shallow).
/// Returns (fee_bps, vol_1e12, slip_1e12, shallow_1e12).
fn compute_dynamic_fee(
    pool: &Pool,
    token_in_is_0: bool,
    amount_in: u128,
    r0: u128,
    r1: u128,
) -> Result<(u16, u128, u128, u128)> {
    require!(amount_in > 0, AmmError::ZeroAmount);

    let (rin, _rout) = if token_in_is_0 { (r0, r1) } else { (r1, r0) };

    // --- volatility proxy: |price - ema| / ema ---
    let price_now = (r1)
        .checked_mul(SCALE)
        .ok_or(AmmError::MathOverflow)?
        / r0;
    let ema = pool.ema_price_1e12 as u128;
    let vol_1e12 = if ema == 0 {
        0
    } else if price_now >= ema {
        (price_now - ema)
            .checked_mul(SCALE)
            .ok_or(AmmError::MathOverflow)?
            / ema
    } else {
        (ema - price_now)
            .checked_mul(SCALE)
            .ok_or(AmmError::MathOverflow)?
            / ema
    };

    // --- slippage proxy: amountIn / (rin + amountIn) ---
    let slip_1e12 = amount_in
        .checked_mul(SCALE)
        .ok_or(AmmError::MathOverflow)?
        / (rin
        .checked_add(amount_in)
        .ok_or(AmmError::MathOverflow)?);

    // --- shallow-depth proxy: 1 - minRes / (minRes + K) ---
    let min_res = u128::min(r0, r1);
    let k: u128 = 1_000 * 1_000_000; // scale-less depth factor ~1e9, OK for demo
    let shallow_1e12 = SCALE
        - (min_res
        .checked_mul(SCALE)
        .ok_or(AmmError::MathOverflow)?
        / (min_res.saturating_add(k)));

    // Linear combo (bps) + clamp
    let dyn_part_bps = (pool.beta_vol_bps_per1e12 as u128)
        .checked_mul(vol_1e12)
        .ok_or(AmmError::MathOverflow)?
        / SCALE
        + (pool.gamma_slip_bps_per1e12 as u128)
        .checked_mul(slip_1e12)
        .ok_or(AmmError::MathOverflow)?
        / SCALE
        + (pool.delta_shallow_bps_per1e12 as u128)
        .checked_mul(shallow_1e12)
        .ok_or(AmmError::MathOverflow)?
        / SCALE;

    let mut raw_bps = (pool.min_fee_bps as u128)
        .checked_add(dyn_part_bps)
        .ok_or(AmmError::MathOverflow)?;
    if raw_bps < pool.min_fee_bps as u128 {
        raw_bps = pool.min_fee_bps as u128;
    }
    if raw_bps > pool.max_fee_bps as u128 {
        raw_bps = pool.max_fee_bps as u128;
    }
    Ok((raw_bps as u16, vol_1e12, slip_1e12, shallow_1e12))
}

/* -------------------------------- Errors -------------------------------- */

#[error_code]
pub enum AmmError {
    #[msg("Not authorized")]
    NotAuthorized,
    #[msg("Bad bounds")]
    BadBounds,
    #[msg("Zero amount")]
    ZeroAmount,
    #[msg("Zero shares")]
    ZeroShares,
    #[msg("Insufficient LP supply")]
    InsufficientLP,
    #[msg("No liquidity")]
    NoLiquidity,
    #[msg("Bad ratio (x/y != dx/dy)")]
    BadRatio,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Amount out is zero")]
    AmountOutZero,
    #[msg("Volatility too high (circuit breaker)")]
    VolTooHigh,
}
